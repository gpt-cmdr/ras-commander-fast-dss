//! A chunk's stored bytes to its decoded bytes: none, Zstandard, zlib and gzip.
//!
//! Sources: Zarr v2 specification, "Compressor" and "Chunks"; numcodecs `zstd.pyx`, `zlib.py` and
//! `gzip.py`, which name the ids; RFC 8878 section 3.1.1.1 for the Zstandard frame header.
//!
//! A chunk decodes to exactly the bytes its array's metadata gives it: the chunk shape times the
//! item size. So the output is reserved once, at that size, and a stream is read only one byte
//! past it. Whatever a stream declares or holds, no more than that is decoded or held.

use super::{decode_error, refused};
use crate::error::VortexError;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// What compresses an array's chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Compressor {
    None,
    Zstd,
    Zlib,
    Gzip,
}

impl Compressor {
    pub fn name(self) -> &'static str {
        match self {
            Compressor::None => "none",
            Compressor::Zstd => "zstd",
            Compressor::Zlib => "zlib",
            Compressor::Gzip => "gzip",
        }
    }
}

/// Most stored bytes a chunk of `expected` decoded bytes may have. Zstandard's worst case adds
/// 3 bytes per 128 KiB block and a frame header (`ZSTD_COMPRESSBOUND`), and deflate's 5 bytes per
/// 16 KiB stored block and a wrapper; a sixteenth and 4 KiB is over both.
pub(super) fn max_stored_bytes(expected: usize) -> u64 {
    (expected as u64)
        .saturating_add(expected as u64 / 16)
        .saturating_add(4096)
}

/// What a Zstandard frame's header states: `(window size, content size if stated)`.
/// RFC 8878 section 3.1.1.1.
pub(super) fn zstd_header(stored: &[u8]) -> Result<(u64, Option<u64>), String> {
    let need = |n: usize| stored.get(..n).ok_or("the frame header is cut short");
    if need(5)?[..4] != [0x28, 0xB5, 0x2F, 0xFD] {
        return Err("not a Zstandard frame".into());
    }
    let descriptor = stored[4];
    let single_segment = descriptor & 0x20 != 0;
    if descriptor & 0x08 != 0 {
        return Err("a reserved bit is set in the frame header".into());
    }
    let dictionary_bytes = [0usize, 1, 2, 4][usize::from(descriptor & 3)];
    let size_flag = descriptor >> 6;
    let size_bytes = match (size_flag, single_segment) {
        (0, true) => 1,
        (0, false) => 0,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let mut at = 5;
    let mut window = None;
    if !single_segment {
        let byte = need(at + 1)?[at];
        at += 1;
        let exponent = u32::from(byte >> 3);
        let mantissa = u64::from(byte & 7);
        let base = 1u64 << (10 + exponent);
        window = Some(base + (base / 8) * mantissa);
    }
    if dictionary_bytes != 0 {
        // numcodecs writes none, and none can be supplied.
        return Err("the frame needs a dictionary".into());
    }
    let field = need(at + size_bytes)?[at..]
        .get(..size_bytes)
        .unwrap_or(&[]);
    let mut size = [0u8; 8];
    size[..size_bytes].copy_from_slice(field);
    let stated = match size_bytes {
        0 => None,
        2 => Some(u64::from_le_bytes(size) + 256),
        _ => Some(u64::from_le_bytes(size)),
    };
    // A single segment's window is its content.
    let window = window
        .or(stated)
        .ok_or("the frame header gives no window")?;
    Ok((window, stated))
}

fn read_exactly(mut stream: impl Read, expected: usize, what: &str) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(expected)
        .map_err(|_| format!("could not allocate {expected} bytes for a chunk"))?;
    out.resize(expected, 0);
    let mut filled = 0;
    while filled < expected {
        match stream.read(&mut out[filled..]) {
            Ok(0) => {
                return Err(format!(
                    "the {what} stream ends after {filled} of the chunk's {expected} bytes"
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("the {what} stream is damaged: {e}")),
        }
    }
    // One byte more would be a chunk larger than its array's metadata says.
    let mut extra = [0u8; 1];
    loop {
        match stream.read(&mut extra) {
            Ok(0) => return Ok(out),
            Ok(_) => {
                return Err(format!(
                    "the {what} stream holds more than the chunk's {expected} bytes"
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("the {what} stream is damaged: {e}")),
        }
    }
}

/// Room the Zstandard decoder takes besides its window: one block, and its tables.
const ZSTD_WORKING_BYTES: usize = 1 << 20;

/// The largest window a frame may state for a chunk smaller than it.
const ZSTD_WINDOW_ALLOWANCE: u64 = 8 << 20;

fn zstd(stored: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let (window, stated) = zstd_header(stored)?;
    if let Some(stated) = stated {
        if stated != expected as u64 {
            return Err(format!(
                "the Zstandard frame states {stated} bytes, and the chunk has {expected}"
            ));
        }
    }
    // A window is what the decoder keeps to copy from, and `ruzstd` reserves it whole when a frame
    // begins (`DecodeBuffer::reset`), growing by doubling. A real encoder's window is its level's,
    // whatever the input's size: 8 MiB at level 19. One over that and over the chunk is what a
    // hostile frame states to make a reader allocate.
    let most = (expected as u64).max(ZSTD_WINDOW_ALLOWANCE);
    if window > most {
        return Err(format!(
            "the Zstandard frame's window of {window} bytes is over the limit of {most} for a \
             chunk of {expected}"
        ));
    }
    // The decoder's own allocations cannot fail softly, so find out first whether they will fit.
    let room = (window as usize)
        .saturating_mul(2)
        .saturating_add(ZSTD_WORKING_BYTES);
    Vec::<u8>::new()
        .try_reserve_exact(room.saturating_add(expected))
        .map_err(|_| format!("could not allocate {room} bytes to decode a chunk"))?;
    let mut source = stored;
    let decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(&mut source, most)
        .map_err(|e| format!("the Zstandard frame is damaged: {e}"))?;
    let out = read_exactly(decoder, expected, "Zstandard")?;
    // numcodecs writes one frame, and `ZSTD_decompress` on two gives both: more than the chunk.
    if !source.is_empty() {
        return Err(format!("{} bytes follow the Zstandard frame", source.len()));
    }
    Ok(out)
}

/// Decode one chunk to exactly `expected` bytes. A decoder's panic is a typed error.
pub(super) fn decode(
    compressor: Compressor,
    stored: &[u8],
    expected: usize,
    what: &str,
) -> Result<Vec<u8>, VortexError> {
    let run = || -> Result<Vec<u8>, String> {
        match compressor {
            Compressor::None => {
                if stored.len() != expected {
                    return Err(format!(
                        "{} stored bytes, and the chunk has {expected}",
                        stored.len()
                    ));
                }
                read_exactly(stored, expected, "stored")
            }
            Compressor::Zstd => zstd(stored, expected),
            Compressor::Zlib => {
                read_exactly(flate2::read::ZlibDecoder::new(stored), expected, "zlib")
            }
            // One member, as numcodecs writes. `MultiGzDecoder` would join members, and
            // `GzDecoder` stops after the first, so bytes after it read as the end.
            Compressor::Gzip => {
                let mut decoder = flate2::read::GzDecoder::new(stored);
                let out = read_exactly(&mut decoder, expected, "gzip")?;
                Ok(out)
            }
        }
    };
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(why)) => Err(VortexError::Decode {
            format: "Zarr",
            message: format!("{what}: {why}"),
        }),
        Err(_) => Err(decode_error(format!(
            "{what}: the {} decoder panicked on this input",
            compressor.name()
        ))),
    }
}

/// The compressor an array's metadata names, or why it is not read.
pub(super) fn compressor_of(
    what: &str,
    compressor: &super::json::Json,
) -> Result<Compressor, VortexError> {
    use super::json::Json;
    if *compressor == Json::Null {
        return Ok(Compressor::None);
    }
    let id = compressor
        .get("id")
        .and_then(Json::as_text)
        .ok_or_else(|| decode_error(format!("{what}: its compressor has no id")))?;
    match id {
        "zstd" => Ok(Compressor::Zstd),
        "zlib" => Ok(Compressor::Zlib),
        "gzip" => Ok(Compressor::Gzip),
        "blosc" => Err(refused(
            what,
            "its chunks are compressed with blosc, which is C and is not in this build. \
             Decode it in Python and pass the arrays, or rewrite the store with zstd",
        )),
        other => Err(refused(
            what,
            format!("compressor {other:?} has no reader here; zstd, zlib, gzip and none do"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8878: magic, descriptor, [window], [content size], then a raw block.
    fn raw_frame(descriptor: u8, window: Option<u8>, size: &[u8], data: &[u8]) -> Vec<u8> {
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, descriptor];
        f.extend(window);
        f.extend(size);
        let header = (data.len() as u32) << 3 | 1;
        f.extend(&header.to_le_bytes()[..3]);
        f.extend(data);
        f
    }

    #[test]
    fn a_raw_block_frame_decodes() {
        let frame = raw_frame(0x20, None, &[4], b"abcd");
        assert_eq!(zstd_header(&frame).unwrap(), (4, Some(4)));
        assert_eq!(
            decode(Compressor::Zstd, &frame, 4, "chunk").unwrap(),
            b"abcd"
        );
    }

    #[test]
    fn a_frame_that_disagrees_with_its_chunk_is_an_error() {
        let frame = raw_frame(0x20, None, &[4], b"abcd");
        for expected in [3, 5] {
            let e = decode(Compressor::Zstd, &frame, expected, "chunk").unwrap_err();
            assert!(e.to_string().contains("states 4 bytes"), "{e}");
        }
        // No stated size: the stream itself is too long, or too short.
        let unsized_frame = raw_frame(0x00, Some(0), &[], b"abcd");
        assert_eq!(zstd_header(&unsized_frame).unwrap(), (1024, None));
        assert!(decode(Compressor::Zstd, &unsized_frame, 4, "chunk").is_ok());
        let e = decode(Compressor::Zstd, &unsized_frame, 3, "chunk").unwrap_err();
        assert!(e.to_string().contains("more than the chunk's 3"), "{e}");
        let e = decode(Compressor::Zstd, &unsized_frame, 5, "chunk").unwrap_err();
        assert!(e.to_string().contains("ends after 4"), "{e}");
    }

    #[test]
    fn a_huge_declared_size_or_window_is_refused_before_decoding() {
        // 2^63 bytes of content, in eight bytes.
        let huge = raw_frame(0xE0, None, &(1u64 << 63).to_le_bytes(), b"abcd");
        let e = decode(Compressor::Zstd, &huge, 4, "chunk").unwrap_err();
        assert!(e.to_string().contains("states 9223372036854775808"), "{e}");
        // Window descriptor 0xFF: 2^41 * 1.875.
        let wide = raw_frame(0x00, Some(0xFF), &[], b"abcd");
        let e = decode(Compressor::Zstd, &wide, 4, "chunk").unwrap_err();
        assert!(e.to_string().contains("window of"), "{e}");
    }

    #[test]
    fn bytes_after_a_frame_and_a_dictionary_are_errors() {
        let mut two = raw_frame(0x20, None, &[4], b"abcd");
        two.extend(raw_frame(0x20, None, &[4], b"efgh"));
        let e = decode(Compressor::Zstd, &two, 4, "chunk").unwrap_err();
        assert!(e.to_string().contains("follow the Zstandard frame"), "{e}");
        let dictionary = raw_frame(0x21, None, &[7, 4], b"abcd");
        let e = decode(Compressor::Zstd, &dictionary, 4, "chunk").unwrap_err();
        assert!(e.to_string().contains("dictionary"), "{e}");
        for cut in 0..9 {
            let frame = raw_frame(0x20, None, &[4], b"abcd");
            assert!(decode(Compressor::Zstd, &frame[..cut], 4, "chunk").is_err());
        }
    }

    #[test]
    fn deflate_streams_decode_to_their_chunk_only() {
        use flate2::{write::GzEncoder, write::ZlibEncoder, Compression};
        use std::io::Write;
        let data = vec![7u8; 1000];
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&data).unwrap();
        let z = z.finish().unwrap();
        assert_eq!(decode(Compressor::Zlib, &z, 1000, "c").unwrap(), data);
        assert!(decode(Compressor::Zlib, &z, 999, "c").is_err());
        assert!(decode(Compressor::Zlib, &z, 1001, "c").is_err());
        assert!(decode(Compressor::Zlib, &z[..z.len() / 2], 1000, "c").is_err());
        let mut g = GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(&data).unwrap();
        let g = g.finish().unwrap();
        assert_eq!(decode(Compressor::Gzip, &g, 1000, "c").unwrap(), data);
        assert!(decode(Compressor::Gzip, &g, 999, "c").is_err());
        assert!(decode(Compressor::Zlib, &g, 1000, "c").is_err());
        assert_eq!(decode(Compressor::None, &data, 1000, "c").unwrap(), data);
        assert!(decode(Compressor::None, &data, 999, "c").is_err());
    }

    #[test]
    fn stored_bytes_are_bounded_by_the_chunk() {
        assert_eq!(max_stored_bytes(0), 4096);
        assert_eq!(max_stored_bytes(1 << 20), (1 << 20) + (1 << 16) + 4096);
    }
}

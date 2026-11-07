//! A float32 GeoTIFF in strips, written directly.
//!
//! The `tiff` crate's encoder is not used: `DirectoryEncoder::write_tag` allocates each tag with
//! `Vec::with_capacity` and its strip offsets grow the same way (`tiff-0.11.3/src/encoder/mod.rs`),
//! so a failed allocation aborts the process and nothing can catch it. Its decoder is bypassed for
//! the same reason (see `source::raster`). Here every buffer is either fixed on the stack or
//! reserved fallibly; the only allocation left is `flate2`'s compressor state, which is a fixed
//! size, made once before the first write, and not sized by anything in the frame.
//!
//! Layout: header, strips, tag values, directory, then the header's offset is filled in. Tags are
//! as GDAL 3.2.1's GTiff driver reads them (`frmts/gtiff/geotiff.cpp`), with Rows Per Strip chosen
//! as the `tiff` crate did, so a strip is about 1 MB.

use crate::error::VortexError;
use crate::source::raster::alloc_zeroed;
use flate2::{Compress, Compression, FlushCompress, Status};
use std::io::{Seek, SeekFrom, Write};

const TARGET_STRIP_BYTES: usize = 1_000_000;
/// Size of the buffers cells pass through, in bytes.
const SCRATCH_BYTES: usize = 64 * 1024;
const MAX_TAGS: usize = 24;

const KIND_ASCII: u16 = 2;
const KIND_SHORT: u16 = 3;
const KIND_LONG: u16 = 4;
const KIND_DOUBLE: u16 = 12;
const KIND_LONG8: u16 = 16;

fn refuse(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "GeoTIFF write".to_string(),
        reason: reason.into(),
    }
}

/// One tag's value, borrowed so that building the table allocates nothing.
#[derive(Clone, Copy)]
pub(super) enum Value<'a> {
    Short(u16),
    Long(u32),
    Shorts(&'a [u16]),
    Doubles(&'a [f64]),
    /// Written with the NUL that ends it.
    Ascii(&'a str),
    /// Strip offsets or counts: LONG in a classic file, LONG8 in a BigTIFF.
    Offsets(&'a [u64]),
}

impl Value<'_> {
    fn kind(&self, bigtiff: bool) -> u16 {
        match self {
            Value::Short(_) | Value::Shorts(_) => KIND_SHORT,
            Value::Long(_) => KIND_LONG,
            Value::Doubles(_) => KIND_DOUBLE,
            Value::Ascii(_) => KIND_ASCII,
            Value::Offsets(_) if bigtiff => KIND_LONG8,
            Value::Offsets(_) => KIND_LONG,
        }
    }

    fn count(&self) -> u64 {
        match self {
            Value::Short(_) | Value::Long(_) => 1,
            Value::Shorts(v) => v.len() as u64,
            Value::Doubles(v) => v.len() as u64,
            Value::Ascii(v) => v.len() as u64 + 1,
            Value::Offsets(v) => v.len() as u64,
        }
    }

    fn byte_len(&self, bigtiff: bool) -> u64 {
        let each = match self.kind(bigtiff) {
            KIND_ASCII => 1,
            KIND_SHORT => 2,
            KIND_LONG => 4,
            _ => 8,
        };
        self.count() * each
    }

    fn write_to<W: Write>(&self, out: &mut W, bigtiff: bool) -> Result<(), VortexError> {
        match self {
            Value::Short(v) => out.write_all(&v.to_le_bytes())?,
            Value::Long(v) => out.write_all(&v.to_le_bytes())?,
            Value::Shorts(values) => {
                for v in *values {
                    out.write_all(&v.to_le_bytes())?;
                }
            }
            Value::Doubles(values) => {
                for v in *values {
                    out.write_all(&v.to_le_bytes())?;
                }
            }
            Value::Ascii(text) => {
                out.write_all(text.as_bytes())?;
                out.write_all(&[0])?;
            }
            Value::Offsets(values) => {
                for v in *values {
                    if bigtiff {
                        out.write_all(&v.to_le_bytes())?;
                    } else {
                        let v = u32::try_from(*v)
                            .map_err(|_| refuse("over 4 GiB does not fit a classic TIFF"))?;
                        out.write_all(&v.to_le_bytes())?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A writer that knows where it is, so offsets need no `Seek` call per strip.
struct Counting<W> {
    inner: W,
    at: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(data)?;
        self.at += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn deflate_error(e: flate2::CompressError) -> VortexError {
    refuse(format!("deflate: {e}"))
}

fn fill_le(bytes: &mut [u8], cells: &[f32]) {
    for (index, cell) in cells.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&cell.to_le_bytes());
    }
}

/// Cells of one strip, little-endian, through `scratch`, compressed if there is a compressor.
fn write_strip<W: Write>(
    out: &mut Counting<W>,
    cells: &[f32],
    compressor: Option<&mut Compress>,
    scratch: &mut [u8],
) -> Result<(), VortexError> {
    let (input, output) = scratch.split_at_mut(SCRATCH_BYTES / 2);
    let per_pass = input.len() / 4;
    let Some(compressor) = compressor else {
        for chunk in cells.chunks(per_pass) {
            let bytes = &mut input[..chunk.len() * 4];
            fill_le(bytes, chunk);
            out.write_all(bytes)?;
        }
        return Ok(());
    };
    compressor.reset();
    for chunk in cells.chunks(per_pass) {
        let bytes = &mut input[..chunk.len() * 4];
        fill_le(bytes, chunk);
        let mut offered = 0;
        while offered < bytes.len() {
            let (before_in, before_out) = (compressor.total_in(), compressor.total_out());
            compressor
                .compress(&bytes[offered..], output, FlushCompress::None)
                .map_err(deflate_error)?;
            offered += (compressor.total_in() - before_in) as usize;
            out.write_all(&output[..(compressor.total_out() - before_out) as usize])?;
        }
    }
    loop {
        let before_out = compressor.total_out();
        let status = compressor
            .compress(&[], output, FlushCompress::Finish)
            .map_err(deflate_error)?;
        out.write_all(&output[..(compressor.total_out() - before_out) as usize])?;
        if matches!(status, Status::StreamEnd) {
            return Ok(());
        }
    }
}

fn pad_to<W: Write>(out: &mut Counting<W>, alignment: u64) -> Result<(), VortexError> {
    const ZEROS: [u8; 8] = [0; 8];
    let over = out.at % alignment;
    if over != 0 {
        out.write_all(&ZEROS[..(alignment - over) as usize])?;
    }
    Ok(())
}

/// The whole file. `extra` are tags after the image's own, in ascending order.
pub(super) fn encode<W: Write + Seek>(
    writer: W,
    (nx, ny): (usize, usize),
    cells: &[f32],
    extra: &[(u16, Value<'_>)],
    bigtiff: bool,
    deflate: bool,
) -> Result<(), VortexError> {
    let width = u32::try_from(nx).map_err(|_| refuse("the grid is too wide for TIFF"))?;
    let height = u32::try_from(ny).map_err(|_| refuse("the grid is too tall for TIFF"))?;
    if nx == 0 || ny == 0 || cells.len() != nx.saturating_mul(ny) {
        return Err(refuse("the grid has no cells or the wrong number"));
    }
    if extra.len() + 11 > MAX_TAGS {
        return Err(refuse("too many tags"));
    }
    let row_bytes = nx
        .checked_mul(4)
        .ok_or_else(|| refuse("the grid is too wide for TIFF"))?;
    let rows_per_strip = (TARGET_STRIP_BYTES / row_bytes).clamp(1, ny);
    let strips = ny.div_ceil(rows_per_strip);

    // Everything fallible or fixed comes before the first write.
    let mut compressor = deflate.then(|| Compress::new(Compression::fast(), true));
    let mut scratch: Vec<u8> = alloc_zeroed(SCRATCH_BYTES, "raster write")?;
    let mut offsets: Vec<u64> = alloc_zeroed(strips, "raster write")?;
    let mut counts: Vec<u64> = alloc_zeroed(strips, "raster write")?;

    let mut out = Counting {
        inner: writer,
        at: 0,
    };
    if bigtiff {
        out.write_all(b"II\x2b\0\x08\0\0\0\0\0\0\0\0\0\0\0")?;
    } else {
        out.write_all(b"II*\0\0\0\0\0")?;
    }

    for (index, strip) in cells.chunks(rows_per_strip * nx).enumerate() {
        offsets[index] = out.at;
        write_strip(&mut out, strip, compressor.as_mut(), &mut scratch)?;
        counts[index] = out.at - offsets[index];
    }

    let own: [(u16, Value<'_>); 11] = [
        (256, Value::Long(width)),
        (257, Value::Long(height)),
        (258, Value::Short(32)),
        (259, Value::Short(if deflate { 8 } else { 1 })),
        (262, Value::Short(1)),
        (273, Value::Offsets(&offsets)),
        (277, Value::Short(1)),
        (278, Value::Long(rows_per_strip as u32)),
        (279, Value::Offsets(&counts)),
        (284, Value::Short(1)),
        (339, Value::Short(3)),
    ];
    let inline = if bigtiff { 8 } else { 4 };
    // TIFF 6.0: words. The BigTIFF specification: "All values must begin at an 8-byte-aligned
    // address", and its directory count is 8 bytes "to keep IFD entries 8-byte-aligned".
    let alignment = if bigtiff { 8 } else { 2 };
    let mut at_of_value = [0u64; MAX_TAGS];
    for (slot, (_, value)) in at_of_value.iter_mut().zip(own.iter().chain(extra)) {
        if value.byte_len(bigtiff) > inline {
            pad_to(&mut out, alignment)?;
            *slot = out.at;
            value.write_to(&mut out, bigtiff)?;
        }
    }

    pad_to(&mut out, alignment)?;
    let directory_at = out.at;
    let tag_count = own.len() + extra.len();
    if bigtiff {
        out.write_all(&(tag_count as u64).to_le_bytes())?;
    } else {
        out.write_all(&(tag_count as u16).to_le_bytes())?;
    }
    for (slot, (tag, value)) in at_of_value.iter().zip(own.iter().chain(extra)) {
        out.write_all(&tag.to_le_bytes())?;
        out.write_all(&value.kind(bigtiff).to_le_bytes())?;
        let count = value.count();
        let bytes = value.byte_len(bigtiff);
        if bigtiff {
            out.write_all(&count.to_le_bytes())?;
        } else {
            let count =
                u32::try_from(count).map_err(|_| refuse("over 4 GiB does not fit classic TIFF"))?;
            out.write_all(&count.to_le_bytes())?;
        }
        if bytes > inline {
            Value::Offsets(std::slice::from_ref(slot)).write_to(&mut out, bigtiff)?;
        } else {
            value.write_to(&mut out, bigtiff)?;
            for _ in bytes..inline {
                out.write_all(&[0])?;
            }
        }
    }
    if bigtiff {
        out.write_all(&0u64.to_le_bytes())?;
    } else {
        out.write_all(&0u32.to_le_bytes())?;
    }
    if !bigtiff && out.at > u64::from(u32::MAX) {
        return Err(refuse("over 4 GiB does not fit a classic TIFF"));
    }
    let end = out.at;
    let mut inner = out.inner;
    if bigtiff {
        inner.seek(SeekFrom::Start(8))?;
        inner.write_all(&directory_at.to_le_bytes())?;
    } else {
        inner.seek(SeekFrom::Start(4))?;
        inner.write_all(&(directory_at as u32).to_le_bytes())?;
    }
    inner.seek(SeekFrom::Start(end))?;
    Ok(())
}

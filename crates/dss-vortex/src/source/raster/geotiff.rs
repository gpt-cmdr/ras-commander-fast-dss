//! TIFF and BigTIFF to cells and a geotransform.
//!
//! Tag numbers are TIFF 6.0, the BigTIFF design note, TIFF Technical Note 3 (floating point
//! predictor) and GeoTIFF 1.1. Georeference rules follow GDAL 3.2.1
//! `GTiffDataset::LoadGeoreferencingAndPamIfNeeded` (`frmts/gtiff/geotiff.cpp`); predictors follow
//! libtiff `tif_predict.c`.
//!
//! Read, bit for bit: float32 and unsigned 8 and 16-bit and signed 16-bit samples (the integers
//! widen exactly); strips and tiles; none, LZW and deflate; predictors 1, 2 and 3; either byte
//! order; classic and BigTIFF; rotated and sheared transforms.
//!
//! Refused with a typed error: more than one band, other sample types, other compressions, sparse
//! chunks, a chunk whose data is short or over-long, an orientation other than top-left, palette
//! and other photometric types, further full-size images or masks, no georeference, ground control
//! points, a negative scale. Overview images are skipped, as they do not hold the base data.
//! Scale and offset in `GDAL_METADATA` are not applied, as `Band.ReadRaster` does not apply them.

use super::{alloc_zeroed, geokeys, Decoded, Limits};
use crate::error::VortexError;
use crate::GeoTransform;
use flate2::{Decompress, FlushDecompress, Status};
use weezl::{decode::Decoder as LzwDecoder, BitOrder, LzwStatus};

const FORMAT: &str = "GeoTIFF";

const TAG_NEW_SUBFILE_TYPE: u16 = 254;
const TAG_SUBFILE_TYPE: u16 = 255;
const TAG_WIDTH: u16 = 256;
const TAG_HEIGHT: u16 = 257;
const TAG_BITS: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_FILL_ORDER: u16 = 266;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_ORIENTATION: u16 = 274;
const TAG_SAMPLES: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_COUNTS: u16 = 279;
const TAG_PLANAR: u16 = 284;
const TAG_PREDICTOR: u16 = 317;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_COUNTS: u16 = 325;
const TAG_SUB_IFDS: u16 = 330;
const TAG_EXTRA_SAMPLES: u16 = 338;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TAG_PIXEL_SCALE: u16 = 33550;
const TAG_TIEPOINT: u16 = 33922;
const TAG_TRANSFORMATION: u16 = 34264;
const TAG_GEO_KEYS: u16 = 34735;
const TAG_GEO_DOUBLES: u16 = 34736;
const TAG_GEO_ASCII: u16 = 34737;
const TAG_GDAL_METADATA: u16 = 42112;
const TAG_GDAL_NODATA: u16 = 42113;

/// Most entries read from one directory, and most directories walked. Both far above any file
/// seen; a file over either is refused rather than walked.
const MAX_ENTRIES: u64 = 4096;
const MAX_IFDS: usize = 1024;

/// Most output per input byte. Deflate cannot exceed 1032 to 1; 12-bit LZW codes can each
/// emit up to 4096 bytes, which is at most 2731 to 1 and is what this bound rests on.
const MAX_DEFLATE_RATIO: u64 = 1032;
const MAX_LZW_RATIO: u64 = 2731;
/// Slack for short streams, whose headers and first codes move the ratio.
const RATIO_SLACK: u64 = 4096;

pub(crate) fn malformed(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: FORMAT,
        message: message.into(),
    }
}

fn refused(what: &str, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: format!("GeoTIFF {what}"),
        reason: reason.into(),
    }
}

/// `II*\0`, `MM\0*`, `II+\0` or `MM\0+`.
pub(crate) fn has_magic(data: &[u8]) -> bool {
    matches!(
        data.get(..4),
        Some(b"II*\0") | Some(b"MM\0*") | Some(b"II+\0") | Some(b"MM\0+")
    )
}

#[derive(Clone, Copy)]
pub(crate) struct Tiff<'a> {
    pub data: &'a [u8],
    pub little: bool,
    big: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    tag: u16,
    kind: u16,
    count: u64,
    /// Where the value or its offset is stored.
    field: usize,
}

fn type_size(kind: u16) -> Option<u64> {
    match kind {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

impl<'a> Tiff<'a> {
    fn bytes(&self, at: u64, len: u64) -> Result<&'a [u8], VortexError> {
        let end = at
            .checked_add(len)
            .ok_or_else(|| malformed("offset overflows"))?;
        usize::try_from(at)
            .ok()
            .zip(usize::try_from(end).ok())
            .and_then(|(a, e)| self.data.get(a..e))
            .ok_or_else(|| malformed(format!("{len} bytes at {at} run past the file's end")))
    }

    fn u16_at(&self, at: u64) -> Result<u16, VortexError> {
        let b: [u8; 2] = self.bytes(at, 2)?.try_into().unwrap_or([0; 2]);
        Ok(if self.little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32_at(&self, at: u64) -> Result<u32, VortexError> {
        let b: [u8; 4] = self.bytes(at, 4)?.try_into().unwrap_or([0; 4]);
        Ok(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn u64_at(&self, at: u64) -> Result<u64, VortexError> {
        let b: [u8; 8] = self.bytes(at, 8)?.try_into().unwrap_or([0; 8]);
        Ok(if self.little {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }

    fn open(data: &'a [u8]) -> Result<(Self, u64), VortexError> {
        let little = match data.get(..2) {
            Some(b"II") => true,
            Some(b"MM") => false,
            _ => return Err(malformed("no byte order mark")),
        };
        let mut tiff = Tiff {
            data,
            little,
            big: false,
        };
        match tiff.u16_at(2)? {
            42 => Ok((tiff, u64::from(tiff.u32_at(4)?))),
            43 => {
                tiff.big = true;
                if tiff.u16_at(4)? != 8 || tiff.u16_at(6)? != 0 {
                    return Err(malformed("BigTIFF offset size is not 8"));
                }
                Ok((tiff, tiff.u64_at(8)?))
            }
            other => Err(malformed(format!("version {other}"))),
        }
    }

    /// The directory at `at`: where its entries start, how many, and the next directory's offset.
    fn directory(&self, at: u64) -> Result<(u64, u64, u64), VortexError> {
        if at == 0 {
            return Err(malformed("directory offset is zero"));
        }
        let (count, first, entry) = if self.big {
            (self.u64_at(at)?, at + 8, 20u64)
        } else {
            (u64::from(self.u16_at(at)?), at + 2, 12u64)
        };
        if count == 0 {
            return Err(malformed("directory has no entries"));
        }
        if count > MAX_ENTRIES {
            return Err(refused(
                "directory",
                format!("{count} entries is over the limit of {MAX_ENTRIES}"),
            ));
        }
        let next_at = first + count * entry;
        let next = if self.big {
            self.u64_at(next_at)?
        } else {
            u64::from(self.u32_at(next_at)?)
        };
        Ok((first, count, next))
    }

    fn entry(&self, first: u64, index: u64) -> Result<Entry, VortexError> {
        let at = first + index * if self.big { 20 } else { 12 };
        let tag = self.u16_at(at)?;
        let kind = self.u16_at(at + 2)?;
        let (count, field) = if self.big {
            (self.u64_at(at + 4)?, at + 12)
        } else {
            (u64::from(self.u32_at(at + 4)?), at + 8)
        };
        let field = usize::try_from(field).map_err(|_| malformed("offset overflows"))?;
        Ok(Entry {
            tag,
            kind,
            count,
            field,
        })
    }

    /// The bytes of an entry's value, checked to lie inside the file. Nothing is allocated.
    fn value(&self, e: &Entry) -> Result<&'a [u8], VortexError> {
        let size = type_size(e.kind)
            .ok_or_else(|| malformed(format!("tag {} has type {}", e.tag, e.kind)))?;
        let len = size
            .checked_mul(e.count)
            .ok_or_else(|| malformed(format!("tag {} count overflows", e.tag)))?;
        let inline = if self.big { 8 } else { 4 };
        if len <= inline {
            return self.bytes(e.field as u64, len);
        }
        let at = if self.big {
            self.u64_at(e.field as u64)?
        } else {
            u64::from(self.u32_at(e.field as u64)?)
        };
        self.bytes(at, len)
    }

    /// The directory's only entry for `tag`. libtiff keeps the first of a repeated tag with a
    /// warning; here a repeat is refused, as two values could disagree.
    fn find(&self, first: u64, count: u64, tag: u16) -> Result<Option<Entry>, VortexError> {
        let mut found = None;
        for index in 0..count {
            let e = self.entry(first, index)?;
            if e.tag == tag {
                if found.is_some() {
                    return Err(malformed(format!("tag {tag} is repeated")));
                }
                found = Some(e);
            }
        }
        Ok(found)
    }

    fn uint(&self, e: &Entry, index: u64) -> Result<u64, VortexError> {
        let value = self.value(e)?;
        if index >= e.count {
            return Err(malformed(format!("tag {} has {} values", e.tag, e.count)));
        }
        let size = match e.kind {
            1 => 1,
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            kind => {
                return Err(malformed(format!(
                    "tag {} has non-integer type {kind}",
                    e.tag
                )))
            }
        };
        let at = (index * size) as usize;
        let b = value
            .get(at..at + size as usize)
            .ok_or_else(|| malformed("tag value is short"))?;
        let mut out = 0u64;
        for i in 0..b.len() {
            let byte = if self.little {
                b[b.len() - 1 - i]
            } else {
                b[i]
            };
            out = (out << 8) | u64::from(byte);
        }
        Ok(out)
    }

    fn double(&self, e: &Entry, index: u64) -> Result<f64, VortexError> {
        if e.kind != 12 {
            return Err(malformed(format!("tag {} is not double", e.tag)));
        }
        if index >= e.count {
            return Err(malformed(format!("tag {} has {} values", e.tag, e.count)));
        }
        let value = self.value(e)?;
        let at = (index * 8) as usize;
        let b: [u8; 8] = value
            .get(at..at + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| malformed("tag value is short"))?;
        Ok(if self.little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Sample {
    F32,
    U8,
    U16,
    I16,
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Sample::U8 => 1,
            Sample::U16 | Sample::I16 => 2,
            Sample::F32 => 4,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Compression {
    None,
    Lzw,
    Deflate,
}

/// Directories after the first may only be overviews. GDAL opens further full-size images as other
/// subdatasets and a mask as the band's mask, either of which would change what a reader sees.
fn check_other_directories(tiff: &Tiff, mut next: u64) -> Result<(), VortexError> {
    let mut walked = 0;
    while next != 0 {
        walked += 1;
        if walked > MAX_IFDS {
            return Err(refused(
                "directories",
                format!("more than {MAX_IFDS}, or a cycle"),
            ));
        }
        let (first, count, after) = tiff.directory(next)?;
        let kind = match tiff.find(first, count, TAG_NEW_SUBFILE_TYPE)? {
            Some(e) => tiff.uint(&e, 0)?,
            None => 0,
        };
        if kind & 4 != 0 {
            return Err(refused("mask", "an internal mask directory is present"));
        }
        if kind & 1 == 0 {
            return Err(refused("pages", "more than one full-size image"));
        }
        next = after;
    }
    Ok(())
}

fn expect(tiff: &Tiff, first: u64, count: u64, tag: u16, name: &str) -> Result<u64, VortexError> {
    let e = tiff
        .find(first, count, tag)?
        .ok_or_else(|| malformed(format!("no {name} tag")))?;
    if e.count != 1 {
        return Err(malformed(format!("{name} has {} values", e.count)));
    }
    tiff.uint(&e, 0)
}

fn optional(
    tiff: &Tiff,
    first: u64,
    count: u64,
    tag: u16,
    default: u64,
) -> Result<u64, VortexError> {
    match tiff.find(first, count, tag)? {
        Some(e) if e.count == 1 => tiff.uint(&e, 0),
        Some(e) => Err(malformed(format!("tag {tag} has {} values", e.count))),
        None => Ok(default),
    }
}

/// How the grid is cut into chunks.
struct Layout {
    /// Cells across and down one stored chunk.
    chunk_width: u64,
    chunk_height: u64,
    across: u64,
    down: u64,
    offsets: Entry,
    counts: Entry,
    tiled: bool,
}

fn layout(
    tiff: &Tiff,
    first: u64,
    count: u64,
    width: u64,
    height: u64,
) -> Result<Layout, VortexError> {
    let tile_w = tiff.find(first, count, TAG_TILE_WIDTH)?;
    let strips = tiff.find(first, count, TAG_STRIP_OFFSETS)?;
    let (lay, label) = if let Some(tile_w) = tile_w {
        if strips.is_some() {
            return Err(malformed("both strip and tile tags"));
        }
        let chunk_width = tiff.uint(&tile_w, 0)?;
        let chunk_height = expect(tiff, first, count, TAG_TILE_LENGTH, "TileLength")?;
        if chunk_width == 0 || chunk_height == 0 {
            return Err(malformed("zero tile size"));
        }
        let offsets = tiff
            .find(first, count, TAG_TILE_OFFSETS)?
            .ok_or_else(|| malformed("no TileOffsets tag"))?;
        let counts = tiff
            .find(first, count, TAG_TILE_COUNTS)?
            .ok_or_else(|| malformed("no TileByteCounts tag"))?;
        (
            Layout {
                chunk_width,
                chunk_height,
                across: width.div_ceil(chunk_width),
                down: height.div_ceil(chunk_height),
                offsets,
                counts,
                tiled: true,
            },
            "tile",
        )
    } else {
        let offsets = strips.ok_or_else(|| malformed("neither strips nor tiles"))?;
        let counts = tiff
            .find(first, count, TAG_STRIP_COUNTS)?
            .ok_or_else(|| malformed("no StripByteCounts tag"))?;
        // libtiff's default is 2^32 - 1, one strip.
        let rows = optional(tiff, first, count, TAG_ROWS_PER_STRIP, u64::from(u32::MAX))?;
        if rows == 0 {
            return Err(malformed("zero RowsPerStrip"));
        }
        (
            Layout {
                chunk_width: width,
                chunk_height: rows.min(height),
                across: 1,
                down: height.div_ceil(rows),
                offsets,
                counts,
                tiled: false,
            },
            "strip",
        )
    };
    let expected = lay
        .across
        .checked_mul(lay.down)
        .ok_or_else(|| malformed("chunk count overflows"))?;
    if lay.offsets.count != expected || lay.counts.count != expected {
        return Err(malformed(format!(
            "{} {label} offsets and {} counts, for {expected} {label}s",
            lay.offsets.count, lay.counts.count
        )));
    }
    Ok(lay)
}

/// One chunk's stored bytes to its full uncompressed length, exactly. Output cannot exceed `out`.
fn inflate(
    input: &[u8],
    out: &mut [u8],
    state: &mut Decompress,
    chunk: u64,
) -> Result<(), VortexError> {
    state.reset(true);
    let status = state
        .decompress(input, out, FlushDecompress::Finish)
        .map_err(|e| malformed(format!("chunk {chunk} deflate: {e}")))?;
    if state.total_out() as usize != out.len() {
        return Err(malformed(format!(
            "chunk {chunk} holds {} bytes, expected {}",
            state.total_out(),
            out.len()
        )));
    }
    // Full, but not yet at the end of the stream: is there more?
    let mut spare = [0u8; 1];
    let end = match status {
        Status::StreamEnd => true,
        _ => {
            let consumed = state.total_in() as usize;
            let rest = input.get(consumed..).unwrap_or(&[]);
            let status = state
                .decompress(rest, &mut spare, FlushDecompress::Finish)
                .map_err(|e| malformed(format!("chunk {chunk} deflate: {e}")))?;
            if state.total_out() as usize != out.len() {
                return Err(refused(
                    "chunk",
                    format!("chunk {chunk} holds more data than its size"),
                ));
            }
            matches!(status, Status::StreamEnd)
        }
    };
    if !end {
        return Err(malformed(format!("chunk {chunk} deflate stream is cut")));
    }
    if (state.total_in() as usize) < input.len() {
        return Err(refused(
            "chunk",
            format!("chunk {chunk} has bytes after its deflate stream"),
        ));
    }
    Ok(())
}

fn unlzw(
    input: &[u8],
    out: &mut [u8],
    decoder: &mut LzwDecoder,
    chunk: u64,
) -> Result<(), VortexError> {
    // libtiff's old-style LSB-first compatibility form.
    if input.first() == Some(&0) && input.get(1).is_some_and(|b| b & 1 != 0) {
        return Err(refused(
            "compression",
            "old-style LZW, which no sample uses",
        ));
    }
    decoder.reset();
    let (mut read, mut written) = (0, 0);
    while written < out.len() {
        let result = decoder.decode_bytes(&input[read..], &mut out[written..]);
        read += result.consumed_in;
        written += result.consumed_out;
        match result.status {
            Ok(LzwStatus::Done) | Ok(LzwStatus::NoProgress) => break,
            Ok(LzwStatus::Ok) => {}
            Err(e) => return Err(malformed(format!("chunk {chunk} LZW: {e}"))),
        }
    }
    if written != out.len() {
        return Err(malformed(format!(
            "chunk {chunk} holds {written} bytes, expected {}",
            out.len()
        )));
    }
    // A code past the full chunk, other than the end-of-information code, is over-long data.
    let mut spare = [0u8; 1];
    let result = decoder.decode_bytes(&input[read..], &mut spare);
    if result.consumed_out != 0 {
        return Err(refused(
            "chunk",
            format!("chunk {chunk} holds more data than its size"),
        ));
    }
    // libtiff always writes the end code (`LZWPostEncode`), and a writer may pad to an even size.
    // A stream that stops short of it, or holds anything else after it, is not one a writer made.
    if !matches!(result.status, Ok(LzwStatus::Done)) {
        return Err(malformed(format!("chunk {chunk} LZW has no end code")));
    }
    read += result.consumed_in;
    if input[read..].iter().any(|b| *b != 0) {
        return Err(malformed(format!(
            "chunk {chunk} LZW has data after its end code"
        )));
    }
    Ok(())
}

/// Undo horizontal differencing in one row (`tif_predict.c`, `horAcc8`, `horAcc16`, `horAcc32`).
fn undo_horizontal(row: &mut [u8], size: usize, little: bool) {
    match size {
        1 => {
            for i in 1..row.len() {
                row[i] = row[i].wrapping_add(row[i - 1]);
            }
        }
        2 => {
            let get = |b: &[u8; 2]| {
                if little {
                    u16::from_le_bytes(*b)
                } else {
                    u16::from_be_bytes(*b)
                }
            };
            let mut last = 0u16;
            for word in row.as_chunks_mut::<2>().0 {
                last = last.wrapping_add(get(word));
                let b = if little {
                    last.to_le_bytes()
                } else {
                    last.to_be_bytes()
                };
                word.copy_from_slice(&b);
            }
        }
        _ => {
            let get = |b: &[u8; 4]| {
                if little {
                    u32::from_le_bytes(*b)
                } else {
                    u32::from_be_bytes(*b)
                }
            };
            let mut last = 0u32;
            for word in row.as_chunks_mut::<4>().0 {
                last = last.wrapping_add(get(word));
                let b = if little {
                    last.to_le_bytes()
                } else {
                    last.to_be_bytes()
                };
                word.copy_from_slice(&b);
            }
        }
    }
}

/// Undo the floating point predictor in one row of float32 (`tif_predict.c`, `fpAcc`). Output is
/// big-endian in `scratch`, whatever the file's byte order: the predictor stores planes of bytes
/// from most to least significant.
fn undo_float(row: &mut [u8], scratch: &mut [u8]) {
    for i in 1..row.len() {
        row[i] = row[i].wrapping_add(row[i - 1]);
    }
    let width = row.len() / 4;
    for column in 0..width {
        for byte in 0..4 {
            scratch[column * 4 + byte] = row[byte * width + column];
        }
    }
}

fn read_sample(kind: Sample, b: &[u8], little: bool) -> f32 {
    match kind {
        Sample::U8 => f32::from(b[0]),
        Sample::U16 => {
            let v = [b[0], b[1]];
            f32::from(if little {
                u16::from_le_bytes(v)
            } else {
                u16::from_be_bytes(v)
            })
        }
        Sample::I16 => {
            let v = [b[0], b[1]];
            f32::from(if little {
                i16::from_le_bytes(v)
            } else {
                i16::from_be_bytes(v)
            })
        }
        Sample::F32 => {
            let v = [b[0], b[1], b[2], b[3]];
            f32::from_bits(if little {
                u32::from_le_bytes(v)
            } else {
                u32::from_be_bytes(v)
            })
        }
    }
}

/// `GDAL_NODATA`, parsed as a double and narrowed as a float32 band does. A value no float32 can
/// hold would match no cell and would read as infinity, so it is refused rather than dropped.
pub(crate) fn parse_nodata(text: &str) -> Result<f32, VortexError> {
    let trimmed = text.trim();
    let value: f64 = trimmed
        .parse()
        .map_err(|_| refused("nodata", format!("cannot read {trimmed:?}")))?;
    if value.is_finite() && value.abs() > f64::from(f32::MAX) {
        return Err(refused(
            "nodata",
            format!("{trimmed} does not fit a float32"),
        ));
    }
    Ok(value as f32)
}

fn geotransform(
    tiff: &Tiff,
    first: u64,
    count: u64,
    point: bool,
) -> Result<GeoTransform, VortexError> {
    let scale = tiff.find(first, count, TAG_PIXEL_SCALE)?;
    let tie = tiff.find(first, count, TAG_TIEPOINT)?;
    let matrix = tiff.find(first, count, TAG_TRANSFORMATION)?;
    let mut t = match (scale, tie, matrix) {
        (Some(scale), Some(tie), None) => {
            if scale.count != 3 {
                return Err(malformed(format!(
                    "ModelPixelScale has {} values",
                    scale.count
                )));
            }
            if tie.count != 6 {
                return Err(refused(
                    "georeference",
                    "more than one tie point, which is ground control rather than a transform",
                ));
            }
            let (sx, sy) = (tiff.double(&scale, 0)?, tiff.double(&scale, 1)?);
            // GDAL warns and treats a negative ScaleY as north-up, contrary to GeoTIFF.
            if sy.is_nan() || sx.is_nan() || sy <= 0.0 || sx <= 0.0 {
                return Err(refused(
                    "georeference",
                    format!("ModelPixelScale ({sx}, {sy}) is not positive"),
                ));
            }
            let dy = -sy;
            let (i, j) = (tiff.double(&tie, 0)?, tiff.double(&tie, 1)?);
            let (x, y) = (tiff.double(&tie, 3)?, tiff.double(&tie, 4)?);
            GeoTransform {
                origin_x: x - i * sx,
                dx: sx,
                row_rotation: 0.0,
                origin_y: y - j * dy,
                column_rotation: 0.0,
                dy,
            }
        }
        (None, None, Some(matrix)) => {
            if matrix.count != 16 {
                return Err(malformed(format!(
                    "ModelTransformation has {} values",
                    matrix.count
                )));
            }
            let m = |i: u64| tiff.double(&matrix, i);
            // The z coefficients and the last row have no place in a geotransform.
            if m(2)? != 0.0
                || m(6)? != 0.0
                || m(12)? != 0.0
                || m(13)? != 0.0
                || m(14)? != 0.0
                || m(15)? != 1.0
            {
                return Err(refused(
                    "georeference",
                    "ModelTransformation is not a 2D affine transform",
                ));
            }
            GeoTransform {
                origin_x: m(3)?,
                dx: m(0)?,
                row_rotation: m(1)?,
                origin_y: m(7)?,
                column_rotation: m(4)?,
                dy: m(5)?,
            }
        }
        (None, None, None) => {
            return Err(refused(
                "georeference",
                "the file states none; GDAL would report a unit transform",
            ))
        }
        (None, Some(_), None) => {
            return Err(refused(
                "georeference",
                "tie points with no scale are ground control, not a transform",
            ))
        }
        _ => return Err(refused("georeference", "conflicting georeference tags")),
    };
    if point {
        // RasterPixelIsPoint: the transform's origin is a cell centre.
        t.origin_x -= t.dx * 0.5 + t.row_rotation * 0.5;
        t.origin_y -= t.column_rotation * 0.5 + t.dy * 0.5;
    }
    Ok(t)
}

pub(crate) fn decode(data: &[u8], limits: &Limits) -> Result<Decoded, VortexError> {
    let (tiff, at) = Tiff::open(data)?;
    let (first, count, next) = tiff.directory(at)?;
    let t = &tiff;

    if optional(t, first, count, TAG_NEW_SUBFILE_TYPE, 0)? & 5 != 0 {
        return Err(refused(
            "first image",
            "it is marked as an overview or a mask",
        ));
    }
    if optional(t, first, count, TAG_SUBFILE_TYPE, 1)? != 1 {
        return Err(refused(
            "first image",
            "SubfileType marks it as an overview or a page",
        ));
    }
    if t.find(first, count, TAG_SUB_IFDS)?.is_some() {
        return Err(refused("sub-images", "the file has SubIFDs"));
    }
    check_other_directories(t, next)?;

    let width = expect(t, first, count, TAG_WIDTH, "ImageWidth")?;
    let height = expect(t, first, count, TAG_HEIGHT, "ImageLength")?;
    if width == 0 || height == 0 {
        return Err(malformed(format!("empty image, {width} x {height}")));
    }
    let cells = width
        .checked_mul(height)
        .filter(|n| *n <= limits.max_cells as u64)
        .ok_or_else(|| {
            refused(
                "grid",
                format!(
                    "{width} x {height} is over the limit of {} cells",
                    limits.max_cells
                ),
            )
        })? as usize;

    let samples = optional(t, first, count, TAG_SAMPLES, 1)?;
    if samples != 1 {
        return Err(refused(
            "bands",
            format!("{samples} bands; only single-band rasters are read"),
        ));
    }
    if t.find(first, count, TAG_EXTRA_SAMPLES)?.is_some() {
        return Err(refused("bands", "ExtraSamples is present"));
    }
    let planar = optional(t, first, count, TAG_PLANAR, 1)?;
    if planar != 1 && planar != 2 {
        return Err(malformed(format!("PlanarConfiguration {planar}")));
    }
    let bits = expect(t, first, count, TAG_BITS, "BitsPerSample")?;
    let format_code = optional(t, first, count, TAG_SAMPLE_FORMAT, 1)?;
    let kind = match (bits, format_code) {
        (32, 3) => Sample::F32,
        (8, 1) => Sample::U8,
        (16, 1) => Sample::U16,
        (16, 2) => Sample::I16,
        // GDAL 3.2 in Vortex has no Int8 and reads SampleFormat 2 at 8 bits as bytes; rasterio's
        // GDAL 3.12 reads it as Int8. int32, uint32 and float64 would have to narrow.
        _ => {
            return Err(refused(
                "sample type",
                format!("{bits} bits, SampleFormat {format_code}"),
            ))
        }
    };
    // GDAL's legacy spelling of Int8: an unsigned byte with PIXELTYPE=SIGNEDBYTE in its metadata
    // (`GTiffDataset::OpenOffset`). Versions differ on it, as they do on SampleFormat 2.
    if bits == 8 {
        if let Some(entry) = t.find(first, count, TAG_GDAL_METADATA)? {
            if t.ascii(&entry)?.windows(10).any(|w| w == b"SIGNEDBYTE") {
                return Err(refused(
                    "sample type",
                    "PIXELTYPE=SIGNEDBYTE in GDAL_METADATA",
                ));
            }
        }
    }
    let compression = match expect(t, first, count, TAG_COMPRESSION, "Compression")? {
        1 => Compression::None,
        5 => Compression::Lzw,
        8 | 32946 => Compression::Deflate,
        other => return Err(refused("compression", format!("type {other}"))),
    };
    let predictor = optional(t, first, count, TAG_PREDICTOR, 1)?;
    match predictor {
        1 | 2 => {}
        3 if kind == Sample::F32 => {}
        other => {
            return Err(refused(
                "predictor",
                format!("{other} with {bits}-bit samples"),
            ))
        }
    }
    let photometric = expect(
        t,
        first,
        count,
        TAG_PHOTOMETRIC,
        "PhotometricInterpretation",
    )?;
    // Min-is-black only. Min-is-white has no reference case here.
    if photometric != 1 {
        return Err(refused("photometric type", format!("{photometric}")));
    }
    if optional(t, first, count, TAG_ORIENTATION, 1)? != 1 {
        return Err(refused("orientation", "it is not top-left"));
    }
    if optional(t, first, count, TAG_FILL_ORDER, 1)? != 1 {
        return Err(refused(
            "fill order",
            "it is not most significant bit first",
        ));
    }

    // Tags GDAL honours whose absence changes the reading come before the buffers.
    let keys = t.find(first, count, TAG_GEO_KEYS)?;
    let doubles = t.find(first, count, TAG_GEO_DOUBLES)?;
    let ascii = t.find(first, count, TAG_GEO_ASCII)?;
    let geo = geokeys::GeoTags::read(t, keys, doubles, ascii)?;
    let point = geo.pixel_is_point()?;
    let transform = geotransform(t, first, count, point)?;
    let crs = geokeys::resolve(&geo);
    let stored_wkt = geokeys::stored_text(&geo);
    let nodata = match t.find(first, count, TAG_GDAL_NODATA)? {
        Some(e) => {
            if e.kind != 2 {
                return Err(malformed("GDAL_NODATA is not ASCII"));
            }
            let bytes = t.value(&e)?;
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            let text = std::str::from_utf8(&bytes[..end])
                .map_err(|_| refused("nodata", "its text is not ASCII"))?;
            Some(parse_nodata(text)?)
        }
        None => None,
    };

    let lay = layout(t, first, count, width, height)?;
    let size = kind.bytes() as u64;
    let chunk_row = lay
        .chunk_width
        .checked_mul(size)
        .ok_or_else(|| malformed("chunk size overflows"))?;
    let chunk_len = chunk_row
        .checked_mul(lay.chunk_height)
        .filter(|n| *n / size <= limits.max_cells as u64)
        .ok_or_else(|| {
            refused(
                "chunk",
                format!(
                    "{} x {} is over the limit of {} cells",
                    lay.chunk_width, lay.chunk_height, limits.max_cells
                ),
            )
        })?;
    let chunks = lay.across * lay.down;

    // Every chunk must exist and be able to produce its length, before anything is allocated.
    // A strip's last chunk may be shorter, and is measured here with the rest.
    let chunk_rows = |chunk: u64| {
        if lay.tiled {
            lay.chunk_height
        } else {
            lay.chunk_height.min(height - chunk * lay.chunk_height)
        }
    };
    for chunk in 0..chunks {
        let offset = t.uint(&lay.offsets, chunk)?;
        let stored = t.uint(&lay.counts, chunk)?;
        if offset == 0 || stored == 0 {
            return Err(refused(
                "chunk",
                format!("chunk {chunk} is sparse, which no sample uses"),
            ));
        }
        t.bytes(offset, stored)?;
        let want = chunk_row * chunk_rows(chunk);
        let ok = match compression {
            Compression::None => stored == want,
            Compression::Deflate => {
                want <= stored
                    .saturating_mul(MAX_DEFLATE_RATIO)
                    .saturating_add(RATIO_SLACK)
            }
            Compression::Lzw => {
                want <= stored
                    .saturating_mul(MAX_LZW_RATIO)
                    .saturating_add(RATIO_SLACK)
            }
        };
        if !ok {
            return Err(malformed(format!(
                "chunk {chunk} stores {stored} bytes, which cannot hold {want}"
            )));
        }
    }

    let mut out: Vec<f32> = alloc_zeroed(cells, FORMAT)?;
    let mut buffer: Vec<u8> = alloc_zeroed(chunk_len as usize, FORMAT)?;
    let mut scratch: Vec<u8> = if predictor == 3 {
        alloc_zeroed(chunk_row as usize, FORMAT)?
    } else {
        Vec::new()
    };
    let mut inflater = (compression == Compression::Deflate).then(|| Decompress::new(true));
    let mut lzw = (compression == Compression::Lzw)
        .then(|| LzwDecoder::with_tiff_size_switch(BitOrder::Msb, 8));

    let nx = width as usize;
    let sample_len = kind.bytes();
    for chunk in 0..chunks {
        let rows = chunk_rows(chunk);
        let want = (chunk_row * rows) as usize;
        let stored = t.bytes(t.uint(&lay.offsets, chunk)?, t.uint(&lay.counts, chunk)?)?;
        let target = &mut buffer[..want];
        match compression {
            Compression::None => target.copy_from_slice(stored),
            Compression::Deflate => {
                if let Some(state) = inflater.as_mut() {
                    inflate(stored, target, state, chunk)?;
                }
            }
            Compression::Lzw => {
                if let Some(state) = lzw.as_mut() {
                    unlzw(stored, target, state, chunk)?;
                }
            }
        }
        let column0 = (chunk % lay.across) * lay.chunk_width;
        let row0 = (chunk / lay.across) * lay.chunk_height;
        for (r, row) in target.chunks_exact_mut(chunk_row as usize).enumerate() {
            let y = row0 + r as u64;
            if y >= height {
                break;
            }
            let (bytes, little): (&[u8], bool) = match predictor {
                2 => {
                    undo_horizontal(row, sample_len, tiff.little);
                    (row, tiff.little)
                }
                3 => {
                    undo_float(row, &mut scratch);
                    (&scratch, false)
                }
                _ => (row, tiff.little),
            };
            let copy = lay.chunk_width.min(width - column0) as usize;
            let start = y as usize * nx + column0 as usize;
            for (cell, sample) in out[start..start + copy]
                .iter_mut()
                .zip(bytes.chunks_exact(sample_len))
            {
                *cell = read_sample(kind, sample, little);
            }
        }
    }

    Ok(Decoded {
        cells: out,
        nx,
        ny: height as usize,
        transform,
        nodata,
        crs,
        stored_wkt,
    })
}

/// A tag value for the geokey reader: doubles and text, in place.
impl<'a> Tiff<'a> {
    pub(crate) fn shorts(&self, e: &Entry) -> Result<&'a [u8], VortexError> {
        if e.kind != 3 {
            return Err(malformed("GeoKeyDirectory is not SHORT"));
        }
        self.value(e)
    }

    pub(crate) fn doubles(&self, e: &Entry) -> Result<&'a [u8], VortexError> {
        if e.kind != 12 {
            return Err(malformed("GeoDoubleParams is not DOUBLE"));
        }
        self.value(e)
    }

    pub(crate) fn ascii(&self, e: &Entry) -> Result<&'a [u8], VortexError> {
        if e.kind != 2 {
            return Err(malformed("GeoAsciiParams is not ASCII"));
        }
        self.value(e)
    }
}

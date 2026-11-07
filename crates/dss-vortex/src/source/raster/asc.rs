//! ASCII grid text to cells, as GDAL 3.2.1 `frmts/aaigrid/aaigriddataset.cpp` reads it.
//!
//! - **Header** (`AAIGDataset::ParseHeader`): the same keys and transform, including the correction
//!   for 360-degree grids. A key this reader does not know, a repeated key, and no corner are
//!   refused. GDAL reads a missing corner as origin 0.
//! - **Type** (`CommonOpen`): a nodata value with `.` or `,` or outside `int`, or any of `.,eE` in
//!   the data, makes a float grid, whose tokens are `CPLAtofM` as `f32`. Otherwise the tokens are
//!   `atoi` as `int`, which `Band.ReadRaster` then casts to `f32`.
//! - **Refused where GDAL returns a value:** a token that does not parse whole (`atoi` gives 0), a
//!   float token outside `f32` (`DoubleToFloatClamp` gives `FLT_MAX`), a nodata value that makes
//!   GDAL use a Float64 band, more tokens than cells, and a `.prj` that is not WKT1.
//!
//! The `.prj` keeps its text verbatim. `importFromESRI` in GDAL also renames some fields, which the
//! P50 gate shows does not change what the CRS is on the samples it reads.

use super::{alloc_zeroed, geokeys, is_wkt1_crs, CrsResolution, Decoded, Limits};
use crate::error::VortexError;
use crate::frame::Crs;
use crate::GeoTransform;
use std::path::Path;

const FORMAT: &str = "ASCII grid";
/// `AAIGRasterBand::IReadBlock`'s token buffer.
const MAX_TOKEN: usize = 498;
/// Longer than any CRS, and `CrsDef` refuses longer.
const MAX_PRJ: u64 = 1 << 16;

fn malformed(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: FORMAT,
        message: message.into(),
    }
}

fn refused(what: &str, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: format!("ASCII grid {what}"),
        reason: reason.into(),
    }
}

/// C's `isspace` in the default locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The `.prj` beside a grid, trying the case GDAL tries on a case-sensitive system too.
pub(super) fn read_prj(path: &Path) -> Result<Option<Vec<u8>>, VortexError> {
    for extension in ["prj", "PRJ"] {
        let candidate = path.with_extension(extension);
        match std::fs::metadata(&candidate) {
            Ok(meta) => {
                if meta.len() > MAX_PRJ {
                    return Err(refused(".prj", format!("{} bytes", meta.len())));
                }
                return Ok(Some(std::fs::read(&candidate)?));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn number(text: &str, key: &str) -> Result<f64, VortexError> {
    let value: f64 = text
        .parse()
        .map_err(|_| malformed(format!("{key} {text:?} is not a number")))?;
    if !value.is_finite() {
        return Err(malformed(format!("{key} is not finite")));
    }
    Ok(value)
}

fn dimension(text: &str, key: &str) -> Result<usize, VortexError> {
    text.parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .ok_or_else(|| malformed(format!("{key} {text:?} is not a positive count")))
}

#[derive(Default)]
struct Header<'a> {
    ncols: Option<&'a str>,
    nrows: Option<&'a str>,
    xllcorner: Option<&'a str>,
    yllcorner: Option<&'a str>,
    xllcenter: Option<&'a str>,
    yllcenter: Option<&'a str>,
    cellsize: Option<&'a str>,
    dx: Option<&'a str>,
    dy: Option<&'a str>,
    nodata: Option<&'a str>,
}

/// Header lines, and where the data starts: the first line that does not start with a letter
/// (`CommonOpen`).
fn parse_header(text: &str) -> Result<(Header<'_>, usize), VortexError> {
    let mut header = Header::default();
    let mut at = 0;
    loop {
        let rest = &text[at..];
        let line_end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let line = &rest[..line_end];
        let Some(first) = line.as_bytes().first() else {
            if line_end == rest.len() {
                return Err(malformed("no data values"));
            }
            at += line_end + 1;
            continue;
        };
        if !first.is_ascii_alphabetic() {
            return Ok((header, at));
        }
        let mut words = line.split_ascii_whitespace();
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            return Err(malformed(format!("header line {line:?} has no value")));
        };
        if words.next().is_some() {
            return Err(malformed(format!("header line {line:?} has extra text")));
        }
        let slot = match key.to_ascii_lowercase().as_str() {
            "ncols" => &mut header.ncols,
            "nrows" => &mut header.nrows,
            "xllcorner" => &mut header.xllcorner,
            "yllcorner" => &mut header.yllcorner,
            "xllcenter" => &mut header.xllcenter,
            "yllcenter" => &mut header.yllcenter,
            "cellsize" => &mut header.cellsize,
            "dx" => &mut header.dx,
            "dy" => &mut header.dy,
            "nodata_value" => &mut header.nodata,
            _ => return Err(refused("header", format!("unknown key {key:?}"))),
        };
        if slot.is_some() {
            return Err(malformed(format!("{key} is repeated")));
        }
        *slot = Some(value);
        at += line_end;
        if at >= text.len() {
            return Err(malformed("no data values"));
        }
        at += 1;
    }
}

fn transform(h: &Header, nx: usize, ny: usize) -> Result<GeoTransform, VortexError> {
    let (mut dx, mut dy) = match (h.cellsize, h.dx, h.dy) {
        (Some(c), None, None) => {
            let c = number(c, "cellsize")?;
            (c, c)
        }
        (None, Some(x), Some(y)) => (number(x, "dx")?, number(y, "dy")?),
        _ => return Err(malformed("needs cellsize, or dx and dy")),
    };
    if dx <= 0.0 || dy <= 0.0 {
        return Err(malformed(format!("cell size {dx} x {dy} is not positive")));
    }
    let (ny_f, center) = (ny as f64, h.xllcenter.is_some());
    let (x, y) =
        match (h.xllcorner, h.yllcorner, h.xllcenter, h.yllcenter) {
            (Some(x), Some(y), None, None) => (number(x, "xllcorner")?, number(y, "yllcorner")?),
            (None, None, Some(x), Some(y)) => (number(x, "xllcenter")?, number(y, "yllcenter")?),
            _ => return Err(refused(
                "georeference",
                "needs xllcorner and yllcorner, or xllcenter and yllcenter; GDAL reads origin 0",
            )),
        };
    if center {
        return Ok(GeoTransform::axis_aligned(
            x - 0.5 * dx,
            dx,
            y - 0.5 * dy + ny_f * dy,
            -dy,
        ));
    }
    // "Small hack to compensate from insufficient precision in cellsize" (ParseHeader).
    if nx.is_multiple_of(360)
        && (x + 180.0).abs() < 1e-12
        && dx == dy
        && (dx - 360.0 / nx as f64).abs() < 1e-9
    {
        dy = 360.0 / nx as f64;
        dx = dy;
    }
    Ok(GeoTransform::axis_aligned(x, dx, y + ny_f * dy, -dy))
}

/// `strchr(pszNoData, '.') || strchr(pszNoData, ',') || outside int`.
fn nodata_makes_float(text: &str, value: f64) -> bool {
    text.contains(['.', ',']) || f64::from(i32::MIN) > value || value > f64::from(i32::MAX)
}

pub(super) fn decode(
    data: &[u8],
    prj: Option<&[u8]>,
    limits: &Limits,
) -> Result<Decoded, VortexError> {
    let text = std::str::from_utf8(data).map_err(|_| malformed("the file is not text"))?;
    let (header, start) = parse_header(text)?;
    let nx = dimension(header.ncols.ok_or_else(|| malformed("no ncols"))?, "ncols")?;
    let ny = dimension(header.nrows.ok_or_else(|| malformed("no nrows"))?, "nrows")?;
    let cells = nx
        .checked_mul(ny)
        .filter(|n| *n <= limits.max_cells)
        .ok_or_else(|| {
            refused(
                "grid",
                format!(
                    "{nx} x {ny} is over the limit of {} cells",
                    limits.max_cells
                ),
            )
        })?;
    let transform = transform(&header, nx, ny)?;

    let body = &text[start..];
    // A token and a separator each, but for the last separator; nothing is allocated before this.
    if (body.len() as u64).div_ceil(2) < cells as u64 {
        return Err(malformed(format!(
            "{} bytes of data cannot hold {cells} cells",
            body.len()
        )));
    }

    let mut float = false;
    let nodata = match header.nodata {
        Some(raw) => {
            let value: f64 = raw
                .parse()
                .map_err(|_| refused("nodata", format!("cannot read {raw:?}")))?;
            float = nodata_makes_float(raw, value);
            // ParseHeader moves these to a Float64 band, which Vortex then narrows. Zero is one
            // too, and narrows to itself, as do the cells.
            if float
                && !value.is_infinite()
                && (value.abs() < f64::from(f32::MIN_POSITIVE) || value.abs() > f64::from(f32::MAX))
                && value != 0.0
                && !value.is_nan()
            {
                return Err(refused(
                    "nodata",
                    format!("{raw} does not fit a normal float32"),
                ));
            }
            Some(value as f32)
        }
        None => None,
    };
    if !float {
        float = body.bytes().any(|b| matches!(b, b'.' | b',' | b'e' | b'E'));
    }

    let mut out: Vec<f32> = alloc_zeroed(cells, FORMAT)?;
    let mut tokens = body
        .split(|c: char| c.is_ascii() && is_space(c as u8))
        .filter(|t| !t.is_empty());
    for (index, cell) in out.iter_mut().enumerate() {
        let token = tokens
            .next()
            .ok_or_else(|| malformed(format!("data stops at cell {index} of {cells}")))?;
        if token.len() > MAX_TOKEN {
            return Err(malformed(format!("cell {index}: token too long")));
        }
        *cell = if float {
            let value: f64 = token
                .parse()
                .map_err(|_| refused("cell", format!("cell {index}: cannot read {token:?}")))?;
            if value.is_infinite() || value.abs() > f64::from(f32::MAX) {
                return Err(refused(
                    "cell",
                    format!("cell {index}: {token} does not fit a float32"),
                ));
            }
            value as f32
        } else {
            let value: i32 = token.parse().map_err(|_| {
                refused(
                    "cell",
                    format!("cell {index}: {token:?} is not an integer in an integer grid"),
                )
            })?;
            value as f32
        };
    }
    if tokens.next().is_some() {
        return Err(refused(
            "data",
            format!("more values than the {cells} cells the header states"),
        ));
    }

    let crs = match prj {
        None => CrsResolution::NotInFile,
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) if is_wkt1_crs(text) => {
                CrsResolution::Stated(geokeys::canonical(Crs::Wkt(text.trim().to_string())))
            }
            _ => CrsResolution::Unresolved(
                "the .prj is not a complete WKT1 PROJCS or GEOGCS".to_string(),
            ),
        },
    };
    Ok(Decoded {
        cells: out,
        nx,
        ny,
        transform,
        nodata,
        crs,
        stored_wkt: None,
    })
}

/// Text of one value on the stack: a grid's text must not ask the allocator for every cell.
pub(crate) struct NumberText {
    bytes: [u8; 64],
    len: usize,
}

impl NumberText {
    pub(crate) fn new() -> Self {
        NumberText {
            bytes: [0; 64],
            len: 0,
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    fn strip_trailing_zeros(&mut self) {
        if self.bytes[..self.len].contains(&b'.') {
            while self.len > 0 && self.bytes[self.len - 1] == b'0' {
                self.len -= 1;
            }
            if self.len > 0 && self.bytes[self.len - 1] == b'.' {
                self.len -= 1;
            }
        }
    }
}

impl std::fmt::Write for NumberText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.len + text.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(std::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// C's `%.20g`, which writes every float32 exactly (`AAIGDataset::CreateCopy`). Rust's exponent and
/// fixed formats print exact decimal expansions, correctly rounded, like the UCRT and glibc. They
/// use stack buffers, and the longest form of a finite double here is under 40 bytes.
pub(crate) fn write_g20(value: f64, out: &mut NumberText) -> std::fmt::Result {
    use std::fmt::Write;
    if value.is_nan() {
        return out.write_str("nan");
    }
    if value.is_infinite() {
        return out.write_str(if value < 0.0 { "-inf" } else { "inf" });
    }
    let mut scientific = NumberText::new();
    write!(scientific, "{value:.19e}")?;
    let text = scientific.as_str();
    let (mantissa, exponent) = text.split_once('e').unwrap_or((text, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if !(-4..20).contains(&exponent) {
        out.write_str(mantissa)?;
        out.strip_trailing_zeros();
        let sign = if exponent < 0 { '-' } else { '+' };
        return write!(out, "e{sign}{:02}", exponent.abs());
    }
    write!(out, "{value:.*}", (19 - exponent) as usize)?;
    out.strip_trailing_zeros();
    Ok(())
}

/// [`write_g20`], as a `String`.
pub(crate) fn format_g20(value: f64) -> String {
    let mut text = NumberText::new();
    match write_g20(value, &mut text) {
        Ok(()) => text.as_str().to_string(),
        // Unreachable for a double: the buffer holds every form.
        Err(_) => format!("{value:e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g20_matches_c() {
        // Printed by C's printf("%.20g") under the UCRT, through Vortex's own .asc output.
        assert_eq!(format_g20(-9999.0), "-9999");
        assert_eq!(format_g20(0.0), "0");
        assert_eq!(format_g20(-0.0), "-0");
        assert_eq!(format_g20(1.5), "1.5");
        assert_eq!(format_g20(f64::from(0.1f32)), "0.10000000149011611938");
        assert_eq!(format_g20(1e20), "1e+20");
        assert_eq!(format_g20(1e-5), "1.0000000000000000818e-05");
        assert_eq!(format_g20(0.0001), "0.00010000000000000000479");
        assert_eq!(format_g20(123456789012345678.0), "123456789012345680");
    }
}

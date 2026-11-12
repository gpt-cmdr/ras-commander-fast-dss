//! ESRI BIL (`.bil` with `.hdr`), the format PRISM distributed its grids in until 2025, read as
//! Vortex 1.1.0 reads it: `io/BilDataReader.getDtos` opens the file with GDAL's EHdr driver,
//! translates it to memory and takes the geotransform, the projection, the band as `Float32` and the
//! band's nodata value. Names, units and time windows come from the file name, by
//! `BilDataReader`'s own rules, which differ from `AscDataReader`'s (see [`describe`]).
//!
//! The header is read as GDAL 3.2.1 `frmts/raw/ehdrdataset.cpp` (`EHdrDataset::Open`) reads it:
//! the same keys, case-insensitive; the pixel type from `NBITS` and `PIXELTYPE`, signed when the
//! nodata is negative and no `PIXELTYPE` is given; the byte order from `BYTEORDER` (`M` when
//! absent); `SKIPBYTES`; the transform from `ULXMAP`/`ULYMAP` (cell centres) or the
//! `XLLCORNER`/`XLLCENTER` family, with `XDIM`/`YDIM` or `CELLSIZE`; nodata from `NODATA`, and
//! from `<name>.aux.xml` only where the header states none (`EHdrRasterBand::GetNoDataValue`).
//!
//! **Refused where GDAL would guess, or would read something this reader does not:** more than one
//! band; fewer than 8 bits per cell; 32-bit integers, which `Float32` would round (as the raster reader refuses them);
//! no `NBITS` (GDAL guesses from the file size); no georeference in the header (GDAL then looks
//! for a world file); a header key GDAL does not know, or one given twice (GDAL takes the last);
//! a `PROJECTION` key (worldclim headers, which hold the CRS in the header); a header line GDAL
//! would stop at (over 50 lines or 1,000 characters); `BANDROWBYTES` or `TOTALROWBYTES` that
//! disagree with the layout GDAL uses (GDAL ignores them at 8 bits and over; ESRI does not); data
//! shorter than the grid; an ENVI header; and an `.aux.xml` that states anything but nodata and
//! statistics. `.stx` (statistics) and `.clr` (a colour table) change no cell and are not read.

use super::raster::{
    alloc_zeroed, geokeys, is_wkt1_crs, names, CrsStatus, Limits, ReadOptions, UnknownCrs,
};
use crate::error::VortexError;
use crate::frame::{Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder};
use std::path::Path;

const FORMAT: &str = "BIL";
/// GDAL stops reading a header at its 51st line or a line over 1,000 characters.
const MAX_HEADER_LINES: usize = 50;
const MAX_HEADER_LINE: usize = 1000;
/// Larger than any header, `.prj` or `.aux.xml` a BIL comes with.
pub(crate) const MAX_SIDECAR: u64 = 1 << 16;
/// How deep a PAM `.aux.xml` may nest. GDAL's own are four levels deep (`PAMDataset`,
/// `PAMRasterBand`, `Metadata`, `MDI`); the parser recurses once per level.
const MAX_XML_DEPTH: usize = 16;

fn malformed(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: FORMAT,
        message: message.into(),
    }
}

fn refused(what: &str, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: format!("BIL {what}"),
        reason: reason.into(),
    }
}

/// How the file name was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BilNaming {
    /// `BilDataReader`'s rules, as Vortex 1.1.0 and 0.13.3 apply them.
    Vortex,
    /// PRISM's 2025 names (`prism_<var>_<region>_<res>_<date>`), which `BilDataReader` does not
    /// recognise and leaves without a variable, units or time. Read here by `AscDataReader`'s rule
    /// for the same names, which Vortex applies to the same grids as GeoTIFF. Not Vortex parity.
    Prism2025,
}

/// The files a BIL is made of, as bytes.
#[derive(Debug, Clone, Copy)]
pub struct BilParts<'a> {
    pub data: &'a [u8],
    pub hdr: &'a [u8],
    pub prj: Option<&'a [u8]>,
    /// `<name>.aux.xml`, GDAL's PAM file.
    pub aux_xml: Option<&'a [u8]>,
}

/// One BIL grid, decoded.
#[derive(Debug, Clone)]
pub struct BilFile {
    frame: GridFrame,
    crs_status: CrsStatus,
    naming: BilNaming,
    short_name: String,
}

#[derive(Debug, Default)]
struct Header {
    nrows: Option<i64>,
    ncols: Option<i64>,
    nbands: Option<i64>,
    skip: Option<i64>,
    ulx: Option<f64>,
    uly: Option<f64>,
    yll: Option<f64>,
    corner: bool,
    xdim: Option<f64>,
    ydim: Option<f64>,
    layout: Option<String>,
    nodata: Option<f64>,
    nbits: Option<i64>,
    pixel_type: Option<char>,
    byte_order: Option<char>,
    band_row_bytes: Option<i64>,
    total_row_bytes: Option<i64>,
}

/// `atoi` on a whole token; anything `atoi` would read only part of is refused.
fn integer(key: &str, text: &str) -> Result<i64, VortexError> {
    text.parse::<i64>()
        .map_err(|_| refused("header", format!("{key} {text} is not a whole integer")))
}

/// `CPLAtofM` on a whole token, finite.
fn real(key: &str, text: &str) -> Result<f64, VortexError> {
    let value: f64 = text
        .parse()
        .map_err(|_| refused("header", format!("{key} {text} is not a number")))?;
    if !value.is_finite() || text.contains(['i', 'I', 'n', 'N']) {
        return Err(refused("header", format!("{key} {text} is not finite")));
    }
    Ok(value)
}

fn parse_header(bytes: &[u8]) -> Result<Header, VortexError> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("the .hdr is not text"))?;
    if text
        .trim_start()
        .get(..4)
        .is_some_and(|w| w.eq_ignore_ascii_case("ENVI"))
    {
        return Err(refused(
            "header",
            "it is an ENVI header, which GDAL reads with another driver",
        ));
    }
    let mut h = Header::default();
    let mut seen = std::collections::HashSet::new();
    // EHdrDataset::Open counts every line CPLReadLineL returns, blank ones included, and stops at
    // line 51. A key on line 51 or later is one GDAL never reads, so it is
    // refused; blank lines there change nothing.
    for (index, line) in gdal_lines(text).into_iter().enumerate() {
        let number = index + 1;
        if line.len() > MAX_HEADER_LINE
            || (number > MAX_HEADER_LINES && !line.trim_matches([' ', '\t']).is_empty())
        {
            return Err(refused(
                "header",
                "GDAL stops reading at line 51 or a line over 1,000 characters",
            ));
        }
        if line.is_empty() {
            continue;
        }
        if line.contains('"') {
            return Err(refused("header", "quoted tokens are not read"));
        }
        let tokens: Vec<&str> = line.split([' ', '\t']).filter(|t| !t.is_empty()).collect();
        if tokens.len() < 2 {
            continue;
        }
        let (key, value) = (tokens[0].to_ascii_lowercase(), tokens[1]);
        if tokens.len() > 2 {
            return Err(refused(
                "header",
                format!("{} has more than one value", tokens[0]),
            ));
        }
        // GDAL keeps the last of a repeated key, and its aliases share a slot.
        let slot = match key.as_str() {
            "ulxmap" | "xllcorner" | "xllcenter" => "x",
            "yllcorner" | "yllcenter" => "yll",
            "xdim" | "cellsize" => "xdim",
            "nodata" | "nodata_value" => "nodata",
            "minvalue" | "min_value" => "min",
            "maxvalue" | "max_value" => "max",
            other => other,
        };
        if !seen.insert(slot.to_string()) || (key == "cellsize" && !seen.insert("ydim".into())) {
            return Err(refused(
                "header",
                format!("{} is stated twice, or with an alias", tokens[0]),
            ));
        }
        match key.as_str() {
            "ncols" => h.ncols = Some(integer(&key, value)?),
            "nrows" => h.nrows = Some(integer(&key, value)?),
            "nbands" => h.nbands = Some(integer(&key, value)?),
            "skipbytes" => h.skip = Some(integer(&key, value)?),
            "ulxmap" | "xllcenter" => h.ulx = Some(real(&key, value)?),
            "xllcorner" => {
                h.ulx = Some(real(&key, value)?);
                h.corner = true;
            }
            "ulymap" => h.uly = Some(real(&key, value)?),
            "yllcenter" => h.yll = Some(real(&key, value)?),
            "yllcorner" => {
                h.yll = Some(real(&key, value)?);
                h.corner = true;
            }
            "xdim" => h.xdim = Some(real(&key, value)?),
            "ydim" => h.ydim = Some(real(&key, value)?),
            "cellsize" => {
                let v = real(&key, value)?;
                h.xdim = Some(v);
                h.ydim = Some(v);
            }
            "layout" => h.layout = Some(value.to_ascii_uppercase()),
            "nodata" | "nodata_value" => h.nodata = Some(real(&key, value)?),
            "nbits" => h.nbits = Some(integer(&key, value)?),
            "pixeltype" => h.pixel_type = value.chars().next().map(|c| c.to_ascii_uppercase()),
            "byteorder" => h.byte_order = value.chars().next().map(|c| c.to_ascii_uppercase()),
            "bandrowbytes" => h.band_row_bytes = Some(integer(&key, value)?),
            "totalrowbytes" => h.total_row_bytes = Some(integer(&key, value)?),
            // Statistics, and a gap between bands of a one-band file: no cell changes.
            "minvalue" | "min_value" | "maxvalue" | "max_value" | "bandgapbytes" => {}
            "projection" => {
                return Err(refused(
                    "header",
                    "a PROJECTION key, which puts the CRS in the header; this reader takes a .prj",
                ))
            }
            _ => {
                return Err(refused(
                    "header",
                    format!("key {} is not one GDAL's EHdr driver reads", tokens[0]),
                ))
            }
        }
    }
    Ok(h)
}

/// The cell type, as `EHdrDataset::Open` chooses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    U8,
    I16,
    U16,
    F32,
}

impl Cell {
    fn bytes(self) -> usize {
        match self {
            Cell::U8 => 1,
            Cell::I16 | Cell::U16 => 2,
            Cell::F32 => 4,
        }
    }
}

fn cell_type(h: &Header) -> Result<Cell, VortexError> {
    let mut pixel = h.pixel_type.unwrap_or('N');
    if !matches!(pixel, 'N' | 'F' | 'S' | 'U') {
        return Err(refused("header", format!("PIXELTYPE starting {pixel}")));
    }
    let nbits = h.nbits.ok_or_else(|| {
        refused(
            "header",
            "no NBITS; GDAL would guess the cell size from the file size",
        )
    })?;
    if h.nodata.is_some_and(|v| v < 0.0) && pixel == 'N' {
        pixel = 'S';
    }
    match (nbits, pixel) {
        (16, 'S') => Ok(Cell::I16),
        (16, 'N' | 'U') => Ok(Cell::U16),
        (32, 'F') => Ok(Cell::F32),
        (32, _) => Err(refused(
            "cells",
            "32-bit integers, which Float32 reads would round",
        )),
        (8, 'N' | 'U') => Ok(Cell::U8),
        (8, 'S') => Err(refused(
            "cells",
            "signed bytes, which GDAL 3.2 reads as unsigned",
        )),
        (1..=7, _) => Err(refused("cells", format!("{nbits} bits per cell"))),
        _ => Err(refused(
            "cells",
            format!("NBITS {nbits} with PIXELTYPE {pixel}"),
        )),
    }
}

/// `nodata` as the frame stores it, which must be the same value in `f32`.
fn nodata_f32(value: f64) -> Result<f32, VortexError> {
    let narrow = value as f32;
    if f64::from(narrow) != value {
        return Err(refused(
            "nodata",
            format!("{value} is not a float32 value, which the cells are read as"),
        ));
    }
    Ok(narrow)
}

/// Lines as GDAL 3.2.1's `CPLReadLine3L` (`port/cpl_conv.cpp`) returns them: `\r\n` and `\n\r`
/// each end one line, a lone `\r` or `\n` ends one, an empty line is a line, and a final line end
/// adds none.
pub(crate) fn gdal_lines(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\r' | b'\n' => {
                out.push(&text[start..i]);
                let pair = matches!(
                    (b[i], b.get(i + 1)),
                    (b'\r', Some(b'\n')) | (b'\n', Some(b'\r'))
                );
                i += if pair { 2 } else { 1 };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < b.len() {
        out.push(&text[start..]);
    }
    out
}

/// What a PAM `.aux.xml` states, if it states only nodata and statistics. GDAL applies everything
/// in it, so anything else (an SRS, a geotransform, a mask, scale or offset, categories, another
/// band) is refused rather than ignored.
pub(crate) fn pam_nodata(xml: &[u8]) -> Result<Option<f64>, VortexError> {
    let bad = |why: &str| VortexError::Refused {
        what: ".aux.xml".to_string(),
        reason: format!("{why}; GDAL would apply it and this reader does not"),
    };
    let text = std::str::from_utf8(xml).map_err(|_| bad("it is not UTF-8"))?;
    let text = text.trim_start_matches('\u{feff}');
    let mut p = Xml { text, at: 0 };
    p.skip_prolog();
    let root = p.element(0).map_err(|e| bad(&e))?;
    p.skip_space();
    if p.at != p.text.len() {
        return Err(bad("text follows the document"));
    }
    if root.name != "PAMDataset" || !root.attributes.is_empty() || !root.text.trim().is_empty() {
        return Err(bad("the root is not a plain PAMDataset"));
    }
    let [band] = root.children.as_slice() else {
        return Err(bad("it does not hold exactly one band"));
    };
    if band.name != "PAMRasterBand" || band.attributes != [("band".to_string(), "1".to_string())] {
        return Err(bad("its element is not PAMRasterBand band=\"1\""));
    }
    let mut nodata = None;
    for child in &band.children {
        match child.name.as_str() {
            "NoDataValue" if child.attributes.is_empty() && child.children.is_empty() => {
                if nodata.is_some() {
                    return Err(bad("NoDataValue is stated twice"));
                }
                let t = child.text.trim();
                let v: f64 = t.parse().map_err(|_| bad("NoDataValue is not a number"))?;
                if !v.is_finite() {
                    return Err(bad("NoDataValue is not finite"));
                }
                nodata = Some(v);
            }
            "Metadata" if child.attributes.is_empty() => {
                for item in &child.children {
                    let key_ok = item.name == "MDI"
                        && item.children.is_empty()
                        && item.attributes.len() == 1
                        && item.attributes[0].0 == "key"
                        && item.attributes[0].1.starts_with("STATISTICS_");
                    if !key_ok {
                        return Err(bad("its metadata holds more than STATISTICS_ items"));
                    }
                }
            }
            other => return Err(bad(&format!("it states {other}"))),
        }
    }
    Ok(nodata)
}

struct Element {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<Element>,
    text: String,
}

/// The subset of XML GDAL's PAM files use: elements, double-quoted attributes, text with the five
/// predefined entities, an `<?xml?>` prolog. No comments, CDATA, DTDs or other entities, and no
/// more than `MAX_XML_DEPTH` levels.
struct Xml<'a> {
    text: &'a str,
    at: usize,
}

impl Xml<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn skip_space(&mut self) {
        let n = self.rest().len() - self.rest().trim_start().len();
        self.at += n;
    }

    fn skip_prolog(&mut self) {
        self.skip_space();
        if self.rest().starts_with("<?xml") {
            if let Some(end) = self.rest().find("?>") {
                self.at += end + 2;
            }
        }
        self.skip_space();
    }

    fn name(&mut self) -> Result<String, String> {
        let n = self
            .rest()
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'))
            .unwrap_or(self.rest().len());
        if n == 0 {
            return Err("an element or attribute has no name".into());
        }
        let name = self.rest()[..n].to_string();
        self.at += n;
        Ok(name)
    }

    fn unescape(raw: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = raw;
        while let Some(i) = rest.find('&') {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            let (entity, len) = [
                ("&amp;", '&'),
                ("&lt;", '<'),
                ("&gt;", '>'),
                ("&quot;", '"'),
                ("&apos;", '\''),
            ]
            .iter()
            .find(|(e, _)| tail.starts_with(e))
            .map(|(e, c)| (*c, e.len()))
            .ok_or("an entity other than the five predefined")?;
            out.push(entity);
            rest = &tail[len..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// One element with its children, `depth` levels below the root. Recursive, so the depth is
    /// bounded: a sidecar of `MAX_SIDECAR` bytes can nest far deeper than a thread's stack holds.
    fn element(&mut self, depth: usize) -> Result<Element, String> {
        if depth >= MAX_XML_DEPTH {
            return Err("elements nest too deep".into());
        }
        self.skip_space();
        if !self.rest().starts_with('<')
            || self.rest().starts_with("<!")
            || self.rest().starts_with("<?")
        {
            return Err("expected an element".into());
        }
        self.at += 1;
        let name = self.name()?;
        let mut attributes = Vec::new();
        loop {
            self.skip_space();
            if self.rest().starts_with("/>") {
                self.at += 2;
                return Ok(Element {
                    name,
                    attributes,
                    children: Vec::new(),
                    text: String::new(),
                });
            }
            if self.rest().starts_with('>') {
                self.at += 1;
                break;
            }
            let key = self.name()?;
            self.skip_space();
            if !self.rest().starts_with("=\"") {
                return Err("an attribute is not key=\"value\"".into());
            }
            self.at += 2;
            let end = self.rest().find('"').ok_or("an attribute is not closed")?;
            let value = Self::unescape(&self.rest()[..end])?;
            self.at += end + 1;
            if attributes.iter().any(|(k, _)| *k == key) {
                return Err("an attribute is repeated".into());
            }
            attributes.push((key, value));
        }
        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            let next = self.rest().find('<').ok_or("an element is not closed")?;
            text.push_str(&Self::unescape(&self.rest()[..next])?);
            self.at += next;
            if self.rest().starts_with("</") {
                self.at += 2;
                let close = self.name()?;
                self.skip_space();
                if close != name || !self.rest().starts_with('>') {
                    return Err("an end tag does not match".into());
                }
                self.at += 1;
                break;
            }
            if children.len() >= 256 {
                return Err("an element has too many children".into());
            }
            children.push(self.element(depth + 1)?);
        }
        if !children.is_empty() && !text.trim().is_empty() {
            return Err("an element mixes text and elements".into());
        }
        Ok(Element {
            name,
            attributes,
            children,
            text,
        })
    }
}

/// `BilDataReader`'s file-name rules, on the file's name rather than its whole path (Vortex looks
/// for the daily date and the monthly digits in the whole path). Where they give no variable and
/// the name is PRISM's 2025 form, `AscDataReader`'s rule for that form applies; see [`BilNaming`].
pub(crate) fn describe(file_name: &str) -> Result<(names::Described, BilNaming), VortexError> {
    use names::{in_order, STATUS};
    let name = file_name.to_lowercase();
    const VARIABLES: [(&str, &str); 7] = [
        ("ppt", "precipitation"),
        ("tmean", "mean temperature"),
        ("tmin", "minimum temperature"),
        ("tmax", "maximum temperature"),
        ("tdmean", "mean dewpoint temperature"),
        ("vpdmin", "minimum vapor pressure deficit"),
        ("vpdmax", "maximum vapor pressure deficit"),
    ];
    let (mut daily, mut monthly, mut normal) = (false, false, false);
    let mut short = "";
    for (index, (code, full)) in VARIABLES.iter().enumerate() {
        if in_order(&name, &[&["prism"], &[code], STATUS], true) {
            short = full;
            daily = in_order(&name, &[&["prism"], &[code], STATUS, &["d1", "d2"]], true);
            monthly = in_order(&name, &[&["prism"], &[code], STATUS, &["m3"]], true);
            break;
        }
        // `prism.*ppt.*normal.*`, tested after the ppt status form only.
        if index == 0 && in_order(&name, &[&["prism"], &["ppt"], &["normal"]], true) {
            short = full;
            normal = true;
            break;
        }
    }
    if short.is_empty() && in_order(&name, &[&["prism_"], &["15s", "30s", "25m"]], true) {
        return Ok((names::describe(file_name)?, BilNaming::Prism2025));
    }

    let time = if daily {
        // path.substring(0, path.lastIndexOf('_')), its last eight characters, as yyyyMMdd.
        let head = name
            .rfind('_')
            .map(|at| &name[..at])
            .ok_or_else(|| names::refuse(&name, "no underscore before the date"))?;
        let text = head
            .len()
            .checked_sub(8)
            .and_then(|at| head.get(at..))
            .filter(|t| t.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| names::refuse(&name, "no yyyyMMdd before the last underscore"))?;
        let date = names::time_of(
            &name,
            names::number(&text[..4]),
            names::number(&text[4..6]),
            names::number(&text[6..8]),
            0,
            0,
        )?;
        FrameTime::Period {
            start: names::add_seconds(date, -43_200),
            end: names::add_seconds(date, 43_200),
        }
    } else if monthly {
        let text = names::last_digits(&name, 6)
            .ok_or_else(|| names::refuse(&name, "no six-digit date"))?;
        let (year, month) = (names::number(&text[..4]), names::number(&text[4..6]));
        let start = names::time_of(&name, year, month, 1, 0, 0)?;
        let (ny, nm) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        FrameTime::Period {
            start,
            end: (names::julian(ny, nm, 1), 0),
        }
    } else if normal {
        FrameTime::Period {
            start: (names::julian(1981, 1, 1), 0),
            end: (names::julian(2010, 12, 31), 0),
        }
    } else {
        FrameTime::Invariant
    };

    let units = if name.contains("ppt") {
        "mm"
    } else if ["tmean", "tmin", "tmax", "tdmean"]
        .iter()
        .any(|v| name.contains(v))
    {
        "Degrees C"
    } else if name.contains("vpdmin") || name.contains("vpdmax") {
        "hPa"
    } else {
        ""
    };
    let data_type = infer_data_type(&time, short);
    Ok((
        names::Described {
            short_name: short.to_string(),
            description: (!short.is_empty()).then(|| short.to_string()),
            units: units.to_string(),
            time,
            data_type,
        },
        BilNaming::Vortex,
    ))
}

/// `VortexGrid.inferDataType`: an instant or no interval is instantaneous; otherwise a short name
/// `VortexVariable.fromName` makes precipitation is an accumulation, and anything else an average.
pub(crate) fn infer_data_type(time: &FrameTime, short_name: &str) -> DataType {
    match time {
        FrameTime::Period { .. }
            if crate::units::variable_from_name(short_name)
                == Some(crate::units::Variable::Precipitation) =>
        {
            DataType::PerCum
        }
        FrameTime::Period { .. } => DataType::PerAver,
        _ => DataType::InstVal,
    }
}

/// The name a variable is selected by: the short name, or the file's stem where there is none, as
/// `BilDataReader.getVariables` falls back to it.
pub(crate) fn selection_name(short: &str, file_name: &str) -> String {
    if !short.is_empty() {
        return short.to_string();
    }
    match file_name.rfind('.') {
        Some(dot) => file_name[..dot].to_string(),
        None => file_name.to_string(),
    }
}

/// Find a sibling of `path` with `extension` added to or replacing its own, matched without regard
/// to letter case as GDAL matches siblings (`CSLFindString`, `CPLFormCIFilename`).
fn sibling(
    path: &Path,
    suffix: &str,
    replace: bool,
) -> Result<Option<std::path::PathBuf>, VortexError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = if replace {
        match name.rfind('.') {
            Some(dot) => &name[..dot],
            None => name.as_str(),
        }
    } else {
        name.as_str()
    };
    let wanted = format!("{base}{suffix}");
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut found = None;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let n = entry.file_name().to_string_lossy().into_owned();
        if n.eq_ignore_ascii_case(&wanted) {
            if found.is_some() {
                return Err(refused(
                    "sidecar",
                    format!("more than one file is named {wanted} in some letter case"),
                ));
            }
            found = Some(entry.path());
        }
    }
    Ok(found)
}

fn read_small(path: &Path, what: &str) -> Result<Vec<u8>, VortexError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_SIDECAR {
        return Err(refused(what, format!("{size} bytes")));
    }
    // The stated size is not trusted: the read itself is bounded.
    super::archive::read_file_bounded(path, MAX_SIDECAR, &format!("BIL {what}"))
}

impl BilFile {
    /// Read a `.bil` with its `.hdr`, and its `.prj` and `.aux.xml` where present.
    pub fn open(path: &Path) -> Result<Self, VortexError> {
        Self::open_with(path, ReadOptions::default())
    }

    pub fn open_with(path: &Path, options: ReadOptions) -> Result<Self, VortexError> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(path)?.len();
        if size > options.limits.max_input_bytes {
            return Err(refused(
                "file",
                format!(
                    "{size} bytes is over the limit of {}",
                    options.limits.max_input_bytes
                ),
            ));
        }
        let hdr = sibling(path, ".hdr", true)?
            .ok_or_else(|| refused("file", format!("{file_name} has no .hdr beside it")))?;
        let hdr = read_small(&hdr, ".hdr")?;
        let prj = match sibling(path, ".prj", true)? {
            Some(p) => Some(read_small(&p, ".prj")?),
            None => None,
        };
        let aux = match sibling(path, ".aux.xml", false)? {
            Some(p) => Some(read_small(&p, ".aux.xml")?),
            None => None,
        };
        // As `raster::applied_sidecar`: these change what GDAL reads, and are not read here.
        for (suffix, replace, why) in [
            (
                ".aux",
                false,
                "GDAL lets it override nodata, CRS and georeference",
            ),
            (
                ".aux",
                true,
                "GDAL lets it override nodata, CRS and georeference",
            ),
            (
                ".msk",
                false,
                "GDAL takes the mask from it instead of from nodata",
            ),
        ] {
            if let Some(found) = sibling(path, suffix, replace)? {
                return Err(VortexError::Refused {
                    what: format!("BIL with {}", found.display()),
                    reason: format!("{why}, and this reader does not read it"),
                });
            }
        }
        let data = super::archive::read_file_bounded(
            path,
            options.limits.max_input_bytes,
            &format!("BIL {file_name}"),
        )?;
        Self::from_parts(
            file_name,
            BilParts {
                data: &data,
                hdr: &hdr,
                prj: prj.as_deref(),
                aux_xml: aux.as_deref(),
            },
            options,
        )
    }

    /// Decode from bytes. `file_name` names the variable, units and time, as the file's name does.
    pub fn from_parts(
        file_name: impl Into<String>,
        parts: BilParts<'_>,
        options: ReadOptions,
    ) -> Result<Self, VortexError> {
        let file_name = file_name.into();
        let limits: Limits = options.limits;
        if parts.data.len() as u64 > limits.max_input_bytes {
            return Err(refused(
                "file",
                format!(
                    "{} bytes is over the limit of {}",
                    parts.data.len(),
                    limits.max_input_bytes
                ),
            ));
        }
        // The sidecars are held to the cap the file and ZIP paths read them under, so a caller's
        // slices cannot make the parsers allocate past it.
        for (what, bytes) in [
            (".hdr", Some(parts.hdr)),
            (".prj", parts.prj),
            (".aux.xml", parts.aux_xml),
        ] {
            if let Some(bytes) = bytes {
                if bytes.len() as u64 > MAX_SIDECAR {
                    return Err(refused(
                        what,
                        format!("{} bytes is over the limit of {MAX_SIDECAR}", bytes.len()),
                    ));
                }
            }
        }
        let h = parse_header(parts.hdr)?;
        let (Some(nrows), Some(ncols)) = (h.nrows, h.ncols) else {
            return Err(refused("header", "NROWS or NCOLS is missing"));
        };
        if nrows <= 0 || ncols <= 0 || nrows > i64::from(i32::MAX) || ncols > i64::from(i32::MAX) {
            return Err(refused("header", format!("{ncols} by {nrows} cells")));
        }
        let (nx, ny) = (ncols as usize, nrows as usize);
        let cells = nx
            .checked_mul(ny)
            .filter(|c| *c <= limits.max_cells)
            .ok_or_else(|| {
                refused(
                    "grid",
                    format!(
                        "{nx} by {ny} cells is over the limit of {}",
                        limits.max_cells
                    ),
                )
            })?;
        if h.nbands.unwrap_or(1) != 1 {
            return Err(refused(
                "header",
                format!("{} bands; one is read", h.nbands.unwrap_or(1)),
            ));
        }
        match h.layout.as_deref() {
            None | Some("BIL") | Some("BIP") | Some("BSQ") => {}
            Some(other) => {
                return Err(refused("header", format!("LAYOUT {other}")));
            }
        }
        let little = match h.byte_order.unwrap_or('M') {
            'I' | 'L' => true,
            'M' => false,
            other => return Err(refused("header", format!("BYTEORDER starting {other}"))),
        };
        let cell = cell_type(&h)?;
        let row_bytes = (nx * cell.bytes()) as i64;
        for (key, value) in [
            ("BANDROWBYTES", h.band_row_bytes),
            ("TOTALROWBYTES", h.total_row_bytes),
        ] {
            if let Some(v) = value {
                if v != row_bytes {
                    return Err(refused(
                        "header",
                        format!("{key} {v}, where GDAL reads rows of {row_bytes} bytes"),
                    ));
                }
            }
        }
        let skip = h.skip.unwrap_or(0);
        if skip < 0 {
            return Err(refused("header", format!("SKIPBYTES {skip}")));
        }
        let skip = skip as usize;
        let need = cells
            .checked_mul(cell.bytes())
            .and_then(|n| n.checked_add(skip))
            .ok_or_else(|| refused("grid", "its size overflows"))?;
        if parts.data.len() < need {
            return Err(refused(
                "data",
                format!("{} bytes, and the header needs {need}", parts.data.len()),
            ));
        }

        // EHdrDataset::Open: the transform.
        let (mut ulx, mut uly) = (h.ulx.unwrap_or(0.5), h.uly.unwrap_or(0.5));
        let (xdim, ydim) = (h.xdim.unwrap_or(1.0), h.ydim.unwrap_or(1.0));
        if let Some(yll) = h.yll {
            if yll == -123.456 {
                return Err(refused(
                    "header",
                    "a lower-left y of -123.456, which GDAL ignores",
                ));
            }
            uly = if h.corner {
                yll + nrows as f64 * ydim
            } else {
                yll + (nrows - 1) as f64 * ydim
            };
        }
        if ulx == 0.5 && uly == 0.5 && xdim == 1.0 && ydim == 1.0 {
            return Err(refused(
                "header",
                "no georeference; GDAL would look for a world file",
            ));
        }
        if !(xdim > 0.0 && ydim > 0.0) {
            return Err(refused("header", format!("cell size {xdim} by {ydim}")));
        }
        if !h.corner {
            ulx -= xdim * 0.5;
            uly += ydim * 0.5;
        }
        let transform = GeoTransform::axis_aligned(ulx, xdim, uly, -ydim);

        let nodata = match h.nodata {
            Some(v) => Some(v),
            None => match parts.aux_xml {
                Some(xml) => pam_nodata(xml)?,
                None => None,
            },
        };
        if let (Some(_), Some(xml)) = (h.nodata, parts.aux_xml) {
            // Checked even when the header's value wins, so a PAM file stating more is refused.
            pam_nodata(xml)?;
        }
        let nodata = nodata.map(nodata_f32).transpose()?;

        let mut out: Vec<f32> = alloc_zeroed(cells, FORMAT)?;
        let body = &parts.data[skip..need];
        match cell {
            Cell::F32 => {
                for (o, c) in out.iter_mut().zip(body.as_chunks::<4>().0) {
                    let b = *c;
                    *o = f32::from_bits(if little {
                        u32::from_le_bytes(b)
                    } else {
                        u32::from_be_bytes(b)
                    });
                }
            }
            Cell::I16 => {
                for (o, c) in out.iter_mut().zip(body.as_chunks::<2>().0) {
                    let b = *c;
                    *o = f32::from(if little {
                        i16::from_le_bytes(b)
                    } else {
                        i16::from_be_bytes(b)
                    });
                }
            }
            Cell::U16 => {
                for (o, c) in out.iter_mut().zip(body.as_chunks::<2>().0) {
                    let b = *c;
                    *o = f32::from(if little {
                        u16::from_le_bytes(b)
                    } else {
                        u16::from_be_bytes(b)
                    });
                }
            }
            Cell::U8 => {
                for (o, c) in out.iter_mut().zip(body.iter()) {
                    *o = f32::from(*c);
                }
            }
        }

        let resolution = match parts.prj {
            None => None,
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) if is_wkt1_crs(text) => {
                    Some(Ok(geokeys::canonical(Crs::Wkt(text.trim().to_string()))))
                }
                _ => Some(Err(
                    "the .prj is not a complete WKT1 PROJCS or GEOGCS".to_string()
                )),
            },
        };
        let (crs, crs_status) = match resolution {
            None => (Crs::Undefined, CrsStatus::NotInFile),
            Some(Ok(crs)) => (crs, CrsStatus::Stated),
            Some(Err(reason)) => match options.unknown_crs {
                UnknownCrs::Refuse => {
                    return Err(VortexError::Refused {
                        what: "BIL CRS".to_string(),
                        reason,
                    })
                }
                UnknownCrs::Undefined => (Crs::Undefined, CrsStatus::Unresolved { reason }),
            },
        };

        let (meta, naming) = describe(&file_name)?;
        let variable = selection_name(&meta.short_name, &file_name);
        let frame = GridFrame::new(GridFrameParts {
            cells: out,
            nx,
            ny,
            row_order: RowOrder::NorthToSouth,
            transform,
            crs,
            nodata,
            time: meta.time,
            data_type: meta.data_type,
            units: meta.units,
            variable,
            description: meta.description,
        })?;
        Ok(BilFile {
            frame,
            crs_status,
            naming,
            short_name: meta.short_name,
        })
    }

    pub fn frame(&self) -> &GridFrame {
        &self.frame
    }

    pub fn into_frame(self) -> GridFrame {
        self.frame
    }

    pub fn crs_status(&self) -> &CrsStatus {
        &self.crs_status
    }

    pub fn naming(&self) -> BilNaming {
        self.naming
    }

    /// The name records are named from, `VortexGrid.shortName`. Empty where the file name states
    /// no variable; the frame's variable is then the file's stem.
    pub fn short_name(&self) -> &str {
        &self.short_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::raster::names::julian;

    const HDR: &str = "BYTEORDER      I\nLAYOUT         BIL\nNROWS          2\nNCOLS          3\n\
NBANDS         1\nNBITS          32\nBANDROWBYTES   12\nTOTALROWBYTES  12\nPIXELTYPE      FLOAT\n\
ULXMAP         -125\nULYMAP         49.9166666666687\nXDIM           0.04166666666667\n\
YDIM           0.04166666666667\nNODATA         -9999\n";

    fn cells(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn read(name: &str, hdr: &str, data: &[u8]) -> Result<BilFile, VortexError> {
        BilFile::from_parts(
            name,
            BilParts {
                data,
                hdr: hdr.as_bytes(),
                prj: None,
                aux_xml: None,
            },
            ReadOptions::default(),
        )
    }

    #[test]
    fn prism_header_reads_as_gdal_does() {
        let data = cells(&[1.0, -9999.0, 2.5, 0.0, 3.0, 4.0]);
        let f = read("PRISM_ppt_stable_4kmD2_20200101_bil.bil", HDR, &data).unwrap();
        let p = f.frame().parts();
        assert_eq!((p.nx, p.ny), (3, 2));
        assert_eq!(p.transform.origin_x, -125.0 - 0.04166666666667 * 0.5);
        assert_eq!(
            p.transform.origin_y,
            49.9166666666687 + 0.04166666666667 * 0.5
        );
        assert_eq!(p.transform.dy, -0.04166666666667);
        assert_eq!(p.nodata, Some(-9999.0));
        assert_eq!(p.cells[1], -9999.0);
        assert_eq!(p.variable, "precipitation");
        assert_eq!(p.units, "mm");
        assert_eq!(p.data_type, DataType::PerCum);
        let day = julian(2020, 1, 1);
        assert_eq!(
            p.time,
            FrameTime::Period {
                start: (day - 1, 43_200),
                end: (day, 43_200)
            }
        );
        assert_eq!(f.naming(), BilNaming::Vortex);
    }

    #[test]
    fn names_follow_bil_data_reader() {
        let d = |n: &str| describe(n).unwrap().0;
        // The tmin daily form is `(d1|d2)` here, not AscDataReader's `d2.*(d1|d2)`.
        assert!(matches!(
            d("PRISM_tmin_stable_4kmD1_19990101_bil.bil").time,
            FrameTime::Period { .. }
        ));
        assert_eq!(
            d("PRISM_tmin_stable_4kmD1_19990101_bil.bil").units,
            "Degrees C"
        );
        let m = d("PRISM_tmean_stable_4kmM3_201901_bil.bil");
        assert_eq!(
            m.time,
            FrameTime::Period {
                start: (julian(2019, 1, 1), 0),
                end: (julian(2019, 2, 1), 0)
            }
        );
        assert_eq!(m.data_type, DataType::PerAver);
        // M2 is not the monthly form: no time.
        assert_eq!(
            d("PRISM_ppt_stable_4kmM2_199901_bil.bil").time,
            FrameTime::Invariant
        );
        let n = d("PRISM_ppt_30yr_normal_4kmM2_annual_bil.bil");
        assert_eq!(n.short_name, "precipitation");
        assert_eq!(
            n.time,
            FrameTime::Period {
                start: (julian(1981, 1, 1), 0),
                end: (julian(2010, 12, 31), 0)
            }
        );
        // Only ppt has a normal form.
        assert_eq!(
            d("PRISM_tmean_30yr_normal_4kmM2_annual_bil.bil").short_name,
            ""
        );
        assert_eq!(
            d("PRISM_tmean_30yr_normal_4kmM2_annual_bil.bil").units,
            "Degrees C"
        );
        let (new, naming) = describe("prism_ppt_us_25m_20200102.bil").unwrap();
        assert_eq!(naming, BilNaming::Prism2025);
        assert_eq!(new.short_name, "precipitation");
        assert_eq!(selection_name("", "elevation.bil"), "elevation");
        assert!(describe("PRISM_ppt_stable_4kmD2_2020013x_bil.bil").is_err());
    }

    #[test]
    fn refusals() {
        let data = cells(&[0.0; 6]);
        for (what, hdr) in [
            ("no nbits", HDR.replace("NBITS          32\n", "")),
            (
                "int32",
                HDR.replace("PIXELTYPE      FLOAT", "PIXELTYPE      SIGNEDINT"),
            ),
            (
                "two bands",
                HDR.replace("NBANDS         1", "NBANDS         2"),
            ),
            ("unknown key", format!("{HDR}FOO 1\n")),
            ("repeat", format!("{HDR}NROWS 2\n")),
            (
                "row bytes",
                HDR.replace("BANDROWBYTES   12", "BANDROWBYTES   16"),
            ),
            ("projection", format!("{HDR}PROJECTION GEOGRAPHIC\n")),
            (
                "no georef",
                HDR.replace("ULXMAP         -125\n", "")
                    .replace("ULYMAP         49.9166666666687\n", "")
                    .replace("XDIM           0.04166666666667\n", "")
                    .replace("YDIM           0.04166666666667\n", ""),
            ),
            ("envi", format!("ENVI\n{HDR}")),
        ] {
            assert!(read("x.bil", &hdr, &data).is_err(), "{what}");
        }
        assert!(read("x.bil", HDR, &data[..20]).is_err());
    }

    #[test]
    fn big_endian_int16_with_negative_nodata_is_signed() {
        let hdr = "NROWS 1\nNCOLS 2\nNBITS 16\nULXMAP 0\nULYMAP 0\nXDIM 1\nYDIM 1\nNODATA -1\n";
        let f = read("x.bil", hdr, &[0xff, 0xff, 0x00, 0x05]).unwrap();
        assert_eq!(f.frame().parts().cells, vec![-1.0, 5.0]);
        assert_eq!(f.short_name(), "");
        assert_eq!(f.frame().parts().variable, "x");
    }

    #[test]
    fn pam_accepts_only_nodata_and_statistics() {
        let ok = br#"<PAMDataset>
  <PAMRasterBand band="1">
    <NoDataValue>-9.99900000000000E+03</NoDataValue>
    <Metadata>
      <MDI key="STATISTICS_MINIMUM">0</MDI>
    </Metadata>
  </PAMRasterBand>
</PAMDataset>"#;
        assert_eq!(pam_nodata(ok).unwrap(), Some(-9999.0));
        for bad in [
            &br#"<PAMDataset><SRS>GEOGCS</SRS><PAMRasterBand band="1"/></PAMDataset>"#[..],
            br#"<PAMDataset><PAMRasterBand band="1"><Offset>1</Offset></PAMRasterBand></PAMDataset>"#,
            br#"<PAMDataset><PAMRasterBand band="2"/></PAMDataset>"#,
            br#"<PAMDataset><PAMRasterBand band="1"><Metadata domain="x"/></PAMRasterBand></PAMDataset>"#,
            br#"<PAMDataset><PAMRasterBand band="1"><!-- c --></PAMRasterBand></PAMDataset>"#,
        ] {
            assert!(pam_nodata(bad).is_err());
        }
    }

    /// A sidecar is at most `MAX_SIDECAR` bytes, which holds over 21,000 nested elements. The
    /// parser recurses once per level, so the depth is bounded and a deeper document is refused,
    /// not left to overflow the stack.
    #[test]
    fn pam_nesting_is_bounded() {
        let deep = |levels: usize| {
            let mut xml = "<a>".repeat(levels);
            xml.push_str(&"</a>".repeat(levels));
            xml.into_bytes()
        };
        let most = (MAX_SIDECAR as usize) / 7;
        assert!(deep(most).len() as u64 <= MAX_SIDECAR);
        for levels in [MAX_XML_DEPTH + 1, 1000, most] {
            assert!(pam_nodata(&deep(levels)).is_err(), "{levels} levels");
        }
        // An opened element that never closes, as deep as a sidecar can hold.
        let open = "<a>".repeat(MAX_SIDECAR as usize / 3).into_bytes();
        assert!(pam_nodata(&open).is_err());
        // GDAL's own PAM files are four levels deep, well inside the bound.
        let ok = br#"<PAMDataset><PAMRasterBand band="1"><Metadata><MDI key="STATISTICS_MEAN">1</MDI></Metadata></PAMRasterBand></PAMDataset>"#;
        assert_eq!(pam_nodata(ok).unwrap(), None);
    }

    #[test]
    fn header_nodata_wins_and_pam_fills_in() {
        let data = cells(&[0.0; 6]);
        let pam = br#"<PAMDataset><PAMRasterBand band="1"><NoDataValue>-1</NoDataValue></PAMRasterBand></PAMDataset>"#;
        let opts = ReadOptions::default();
        let with = |hdr: &str| {
            BilFile::from_parts(
                "x.bil",
                BilParts {
                    data: &data,
                    hdr: hdr.as_bytes(),
                    prj: None,
                    aux_xml: Some(pam),
                },
                opts,
            )
            .unwrap()
            .frame()
            .parts()
            .nodata
        };
        assert_eq!(with(HDR), Some(-9999.0));
        assert_eq!(with(&HDR.replace("NODATA         -9999\n", "")), Some(-1.0));
    }
}

//! Polygon zones from a shapefile: `.shp`, `.shx`, `.dbf`, and `.prj` when the caller states no CRS.
//!
//! Sources, GDAL 3.2.1: `ogr/ogrsf_frmts/shape/shape2ogr.cpp` `SHPReadOGRObject`,
//! `SHPReadOGRFeature`, `ogrshapelayer.cpp`, `dbfopen.c`; shapelib's record layout. Written here and
//! not through the `shapefile` crate, because every count in these files comes from the file, and
//! each allocation here is bounded by the file's real size and made fallibly. `catch_unwind` cannot
//! catch an allocation abort.

use super::geometry::{organize_rings, try_vec, Part, Pt};
use super::{refused, ZoneLimits, ZoneRecord};
use crate::error::VortexError;
use std::io::Read;
use std::path::{Path, PathBuf};

// Whole file, bounded and fallible.
pub(super) fn read_bounded(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>, VortexError> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size > limit {
        return Err(refused(
            what,
            format!("it is {size} bytes, over the limit of {limit}"),
        ));
    }
    let mut bytes = try_vec::<u8>(size as usize, what)?;
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn be_i32(b: &[u8], at: usize) -> i32 {
    i32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le_i32(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le_f64(b: &[u8], at: usize) -> f64 {
    f64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

/// The neighbour of a `.shp` with this extension, in either case.
fn sibling(path: &Path, extension: &str) -> PathBuf {
    let lower = path.with_extension(extension);
    if lower.exists() {
        return lower;
    }
    path.with_extension(extension.to_ascii_uppercase())
}

struct Dbf {
    bytes: Vec<u8>,
    header_len: usize,
    record_len: usize,
    records: usize,
    utf8: bool,
    offset: usize,
    length: usize,
    decimals: usize,
    kind: u8,
}

fn open_dbf(path: &Path, field: &str, limits: &ZoneLimits, utf8: bool) -> Result<Dbf, VortexError> {
    let what = format!("zones {}", path.display());
    let bytes = read_bounded(path, limits.max_file_bytes, &what)?;
    if bytes.len() < 33 {
        return Err(refused(&what, "its .dbf is shorter than a header"));
    }
    let records = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let header_len = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let record_len = u16::from_le_bytes([bytes[10], bytes[11]]) as usize;
    if header_len < 33 || header_len > bytes.len() || record_len < 1 {
        return Err(refused(&what, "its .dbf header is not consistent"));
    }
    if records
        .checked_mul(record_len)
        .and_then(|n| n.checked_add(header_len))
        .is_none_or(|end| end > bytes.len())
    {
        return Err(refused(&what, "its .dbf is shorter than its records"));
    }
    // Field descriptors: 32 bytes each, ending at 0x0D. `OGRFeatureDefn::GetFieldIndex` is
    // case-insensitive, and the first match wins.
    let mut at = 32;
    let mut offset = 1;
    let mut found = None;
    let mut names = Vec::new();
    while at + 32 <= header_len && bytes[at] != 0x0D {
        let raw = &bytes[at..at + 11];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(11);
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();
        let kind = bytes[at + 11];
        let length = bytes[at + 16] as usize;
        let decimals = bytes[at + 17] as usize;
        if found.is_none() && name.eq_ignore_ascii_case(field) {
            found = Some((offset, length, decimals, kind));
        }
        names.push(name);
        offset += length;
        at += 32;
    }
    if offset > record_len {
        return Err(refused(
            &what,
            "its .dbf fields are longer than its records",
        ));
    }
    let Some((offset, length, decimals, kind)) = found else {
        return Err(refused(
            &what,
            format!("it has no field {field:?}; it has {names:?}"),
        ));
    };
    Ok(Dbf {
        bytes,
        header_len,
        record_len,
        records,
        utf8,
        offset,
        length,
        decimals,
        kind,
    })
}

impl Dbf {
    /// `OGRFeature::GetFieldAsString` for one record. `Ok(None)` is a null field.
    fn id(&self, record: usize, what: &str) -> Result<Option<String>, VortexError> {
        let start = self.header_len + record * self.record_len;
        if self.bytes[start] == b'*' {
            return Err(refused(
                what,
                format!("its .dbf marks record {record} deleted"),
            ));
        }
        let raw = &self.bytes[start + self.offset..start + self.offset + self.length];
        // shapelib's TRIM_DBF_WHITESPACE: leading and trailing spaces.
        let trim_at = raw
            .iter()
            .position(|&b| b != b' ' && b != 0)
            .unwrap_or(raw.len());
        let trim_end = raw
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map_or(trim_at, |i| i + 1);
        let trimmed = &raw[trim_at..trim_end];
        if trimmed.is_empty() {
            return Ok(None);
        }
        let text = || -> String {
            if self.utf8 {
                String::from_utf8_lossy(trimmed).into_owned()
            } else {
                // OGR's default for a .dbf with no code page, and every byte maps.
                trimmed.iter().map(|&b| b as char).collect()
            }
        };
        match self.kind {
            b'C' | b'L' => Ok(Some(text())),
            b'N' | b'F' => {
                let t = text();
                // OGRShapeLayer: width under 19 with no decimals is an integer, printed with %d or
                // %lld, and anything else is a real, printed with %.*f.
                if self.decimals == 0 && self.length < 19 {
                    let v: i64 = t.parse().map_err(|_| {
                        refused(
                            what,
                            format!("record {record} holds {t:?} in a number field"),
                        )
                    })?;
                    Ok(Some(v.to_string()))
                } else {
                    let v: f64 = t.parse().map_err(|_| {
                        refused(
                            what,
                            format!("record {record} holds {t:?} in a number field"),
                        )
                    })?;
                    Ok(Some(format!("{:.*}", self.decimals, v)))
                }
            }
            other => Err(refused(
                what,
                format!(
                    "field type {:?} is not used as a zone name here; use a text or number field",
                    other as char
                ),
            )),
        }
    }
}

/// One shape. `Ok(vec![])` is a null shape.
fn parse_shape(
    content: &[u8],
    what: &str,
    points: &mut usize,
    limits: &ZoneLimits,
) -> Result<Vec<Part>, VortexError> {
    let short = || refused(what, "a shape record is shorter than its counts say");
    if content.len() < 4 {
        return Err(short());
    }
    let kind = le_i32(content, 0);
    match kind {
        0 => Ok(Vec::new()),
        1 | 11 | 21 => {
            if content.len() < 20 {
                return Err(short());
            }
            *points += 1;
            Ok(vec![Part::Point(Pt {
                x: le_f64(content, 4),
                y: le_f64(content, 12),
            })])
        }
        8 | 18 | 28 => {
            if content.len() < 40 {
                return Err(short());
            }
            let n = le_i32(content, 36);
            if n < 0
                || (n as usize)
                    .checked_mul(16)
                    .and_then(|b| b.checked_add(40))
                    .is_none_or(|e| e > content.len())
            {
                return Err(short());
            }
            let n = n as usize;
            *points += n;
            check_points(*points, limits, what)?;
            let mut parts = try_vec(n, what)?;
            for i in 0..n {
                parts.push(Part::Point(Pt {
                    x: le_f64(content, 40 + 16 * i),
                    y: le_f64(content, 48 + 16 * i),
                }));
            }
            Ok(parts)
        }
        3 | 13 | 23 | 5 | 15 | 25 => {
            if content.len() < 44 {
                return Err(short());
            }
            let n_parts = le_i32(content, 36);
            let n_points = le_i32(content, 40);
            if n_parts < 0 || n_points < 0 {
                return Err(short());
            }
            let (n_parts, n_points) = (n_parts as usize, n_points as usize);
            let points_at = n_parts
                .checked_mul(4)
                .and_then(|b| b.checked_add(44))
                .ok_or_else(short)?;
            if n_points
                .checked_mul(16)
                .and_then(|b| b.checked_add(points_at))
                .is_none_or(|e| e > content.len())
            {
                return Err(short());
            }
            if n_parts == 0 {
                return Ok(Vec::new());
            }
            *points += n_points;
            check_points(*points, limits, what)?;
            let start = |i: usize| le_i32(content, 44 + 4 * i);
            let mut rings: Vec<Vec<Pt>> = try_vec(n_parts, what)?;
            for i in 0..n_parts {
                let from = start(i);
                let to = if i + 1 < n_parts {
                    start(i + 1)
                } else {
                    n_points as i32
                };
                if from < 0 || to < from || to as usize > n_points {
                    return Err(refused(
                        what,
                        "a shape's part offsets are not ascending within its points",
                    ));
                }
                let (from, to) = (from as usize, to as usize);
                let mut ring = try_vec(to - from, what)?;
                for k in from..to {
                    ring.push(Pt {
                        x: le_f64(content, points_at + 16 * k),
                        y: le_f64(content, points_at + 16 * k + 8),
                    });
                }
                rings.push(ring);
            }
            if matches!(kind, 3 | 13 | 23) {
                Ok(rings.into_iter().map(Part::Line).collect())
            } else {
                Ok(organize_rings(rings)
                    .into_iter()
                    .map(Part::Polygon)
                    .collect())
            }
        }
        other => Err(refused(
            what,
            format!("shape type {other} is not read here"),
        )),
    }
}

fn check_points(points: usize, limits: &ZoneLimits, what: &str) -> Result<(), VortexError> {
    if points > limits.max_points {
        return Err(refused(
            what,
            format!("it holds over {} points, the limit", limits.max_points),
        ));
    }
    Ok(())
}

/// Every feature in `.shx` order, as `GetFeature(i)` reads them. Offsets come from the `.shx`: it is
/// the index OGR uses, and one that disagrees with the `.shp` is refused rather than followed.
pub(super) fn read(
    path: &Path,
    field: &str,
    limits: &ZoneLimits,
) -> Result<Vec<ZoneRecord>, VortexError> {
    let what = format!("zones {}", path.display());
    let shp = read_bounded(path, limits.max_file_bytes, &what)?;
    let shx_path = sibling(path, "shx");
    if !shx_path.exists() {
        return Err(refused(
            &what,
            "it has no .shx, so its features cannot be indexed as OGR indexes them",
        ));
    }
    let shx = read_bounded(&shx_path, limits.max_file_bytes, &what)?;
    let dbf_path = sibling(path, "dbf");
    if !dbf_path.exists() {
        return Err(refused(
            &what,
            format!("it has no .dbf, so it has no field {field:?}"),
        ));
    }
    let utf8 = {
        let cpg = sibling(path, "cpg");
        cpg.exists() && {
            let text = read_bounded(&cpg, 1 << 16, &what)?;
            let t = String::from_utf8_lossy(&text).trim().to_ascii_uppercase();
            t == "UTF-8" || t == "UTF8" || t == "65001"
        }
    };
    let dbf = open_dbf(&dbf_path, field, limits, utf8)?;
    for (name, file) in [("shp", &shp), ("shx", &shx)] {
        if file.len() < 100 {
            return Err(refused(
                &what,
                format!("its .{name} is shorter than a header"),
            ));
        }
        if be_i32(file, 0) != 9994 || le_i32(file, 28) != 1000 {
            return Err(refused(&what, format!("its .{name} is not a shapefile")));
        }
    }
    let count = (shx.len() - 100) / 8;
    if count != dbf.records {
        return Err(refused(
            &what,
            format!(
                "its .shx has {count} records and its .dbf has {}",
                dbf.records
            ),
        ));
    }
    if count > limits.max_features {
        return Err(refused(
            &what,
            format!(
                "it holds {count} features, over the limit of {}",
                limits.max_features
            ),
        ));
    }
    let mut out = try_vec(count, &what)?;
    let mut points = 0usize;
    for i in 0..count {
        let index = 100 + 8 * i;
        let offset = be_i32(&shx, index);
        let length = be_i32(&shx, index + 4);
        if offset < 50 || length < 2 {
            return Err(refused(
                &what,
                format!("its .shx has an impossible entry for record {i}"),
            ));
        }
        let at = offset as usize * 2;
        let bytes = length as usize * 2;
        let Some(content) = at
            .checked_add(8)
            .and_then(|s| s.checked_add(bytes))
            .and_then(|e| shp.get(at + 8..e))
        else {
            return Err(refused(
                &what,
                format!("record {i} is beyond the end of its .shp"),
            ));
        };
        if be_i32(&shp, at + 4) != length {
            return Err(refused(
                &what,
                format!("record {i}'s length differs between its .shx and .shp"),
            ));
        }
        let parts = parse_shape(content, &what, &mut points, limits)?;
        out.push(ZoneRecord {
            id: dbf.id(i, &what)?,
            parts,
        });
    }
    Ok(out)
}

/// The text of a `.prj` beside the file, when there is one.
pub(super) fn prj_text(path: &Path) -> Result<Option<String>, VortexError> {
    let prj = sibling(path, "prj");
    if !prj.exists() {
        return Ok(None);
    }
    let what = format!("zones {}", path.display());
    let bytes = read_bounded(&prj, 1 << 20, &what)?;
    let text = String::from_utf8(bytes).map_err(|_| refused(&what, "its .prj is not text"))?;
    Ok(Some(text))
}

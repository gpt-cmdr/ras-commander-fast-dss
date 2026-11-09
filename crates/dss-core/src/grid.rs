//! Version-neutral spatial grids.
//!
//! Sources: heclib `zStructSpatialGrid.h`, `zspatialGridRetrieve.c`; Java heclib
//! `GridInfoFlat`, `HrapInfoFlat`, `AlbersInfoFlat`, `SpecifiedGridInfoFlat`
//! (`flatten`/`unflatten`) and `DataCompression.uncompress` in HEC Monolith 3.3.27.
//! Legacy Java layout rules were checked with `javap -p -c` and with grids read
//! through HEC Monolith.

use std::fmt;

/// Missing value produced by heclib Java `DataCompression.uncompress`.
pub const GRID_UNDEFINED: f32 = -f32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridType {
    Undefined,
    Hrap,
    Albers,
    Specified,
}

impl GridType {
    /// `headers/zdataTypeDescriptions.h`: even types have time, odd types do not.
    pub fn from_record_type(record_type: i32) -> Result<Self, GridError> {
        match record_type {
            400 | 401 => Ok(Self::Undefined),
            410 | 411 => Ok(Self::Hrap),
            420 | 421 => Ok(Self::Albers),
            430 | 431 => Ok(Self::Specified),
            _ => Err(GridError::UnsupportedRecordType { record_type }),
        }
    }
}

/// Albers projection parameters. Legacy grids: as stored by Java heclib
/// `AlbersInfoFlat.flatten` (`units` is the trimmed display text). Modern (-100)
/// grids: parsed from the stored WKT as Monolith does (`albers_from_wkt`), so
/// `units` is the last `UNIT["..."]` name (for the SHG WKT, `Meter`).
#[derive(Debug, Clone, PartialEq)]
pub struct AlbersProjection {
    pub datum: i32,
    pub units: String,
    pub first_standard_parallel: f32,
    pub second_standard_parallel: f32,
    pub central_meridian: f32,
    pub latitude_of_projection_origin: f32,
    pub false_easting: f32,
    pub false_northing: f32,
}

/// Limits checked before reading or allocating grid payloads. Independent of file size.
#[derive(Debug, Clone, Copy)]
pub struct GridReadOptions {
    pub max_cells: usize,
    pub max_header_bytes: usize,
    pub max_compressed_bytes: usize,
    /// Disable Java heclib's legacy ZLIB missing-value normalization. Useful for
    /// byte-level audits; modern -100 grids always preserve their stored bits.
    pub preserve_legacy_missing: bool,
}

impl Default for GridReadOptions {
    fn default() -> Self {
        Self {
            max_cells: 16_777_216,
            max_header_bytes: 1_048_576,
            max_compressed_bytes: 134_217_728,
            preserve_legacy_missing: false,
        }
    }
}

/// One physical grid record. Cell order is unchanged: x varies fastest, lower rows
/// first (`GridData.getValue`, index = y * nx + x). No implicit north-up flip.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub pathname: Vec<u8>,
    pub record_type: i32,
    pub grid_type: GridType,
    /// Raw internal-header int 0: -100 for modern, positive byte size for legacy.
    pub struct_version: i32,
    pub version: i32,
    pub info_size: Option<i32>,
    pub grid_info_size: Option<i32>,
    pub data_units: String,
    /// `zStructSpatialGrid.h:dataType`: 0 PER-AVER, 1 PER-CUM, 2 INST-VAL, 3 INST-CUM.
    pub data_type: i32,
    pub data_source: String,
    pub lower_left_cell_x: i32,
    pub lower_left_cell_y: i32,
    pub nx: usize,
    pub ny: usize,
    pub cell_size: f32,
    pub x_coord_of_grid_cell_zero: f32,
    pub y_coord_of_grid_cell_zero: f32,
    pub srs_name: String,
    pub srs_definition_type: i32,
    /// WKT as Monolith's `GridInfo.getSpatialReferenceSystem` reports it. Albers
    /// grids (legacy and -100) get the SHG WKT or "" (see `albers_srs`).
    pub srs_definition: String,
    /// Modern (-100) grids only: the SRS definition string exactly as stored in the
    /// record (heclib `zStructSpatialGrid.srsDefinition`), which can differ from
    /// `srs_definition` for Albers grids. `None` for legacy layouts.
    pub stored_srs_definition: Option<String>,
    pub albers: Option<AlbersProjection>,
    pub null_value: f32,
    pub time_zone_id: String,
    pub time_zone_raw_offset: i32,
    pub is_interval: bool,
    pub is_time_stamped: bool,
    /// Whole seconds since 1899-12-31, with no time zone conversion.
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    /// Legacy header HecTime values in minutes; absent for -100.
    pub stored_start_time: Option<i32>,
    pub stored_end_time: Option<i32>,
    pub compression_method: i32,
    pub sizeof_compressed_elements: usize,
    pub compression_scale_factor: f32,
    pub compression_base: f32,
    pub storage_data_type: i32,
    /// Statistics as stored, including sentinel and NaN payload bits.
    pub min_data_value: Option<f32>,
    pub max_data_value: Option<f32>,
    pub mean_data_value: Option<f32>,
    pub range_limit_table: Vec<f32>,
    pub number_equal_or_exceeding_range_limit: Vec<i32>,
    /// Decoded values, with legacy missing values represented by GRID_UNDEFINED
    /// as in Java heclib. `GridReadOptions::preserve_legacy_missing` exposes raw
    /// legacy ZLIB bits instead. Modern -100 grids keep all stored f32 bits.
    pub data: Vec<f32>,
}

impl Grid {
    /// Missingness without changing stored bits: NaN, `GRID_UNDEFINED`, -901, -902,
    /// and the grid's `null_value` **for specified grids only** (as Monolith).
    ///
    /// heclib's C header documents `_nullValue` as "the value used in the grid's data
    /// array to represent a 'no data' cell" (`zStructSpatialGrid.h:74`), but Monolith
    /// carries it only to and from a `SpecifiedGridInfo`: `GridStructContainer`'s
    /// `(GridInfo)` constructor (offsets 432-440 in `javap -c`, Monolith 3.3.27) and
    /// `returnGridInfo` (offsets 422-430). An HRAP or Albers grid written by Monolith
    /// to DSS7 therefore stores the Java default 0.0 there, and Monolith's readers treat
    /// those cells as data. Legacy HRAP/Albers grids have no stored null value
    /// (`null_value` is `GRID_UNDEFINED`), so they are unchanged.
    pub fn is_missing(&self, value: f32) -> bool {
        value.is_nan()
            || (self.grid_type == GridType::Specified && value == self.null_value)
            || value == GRID_UNDEFINED
            || value == -901.0
            || value == -902.0
    }

    /// Outer cell edges in the SRS, calculated in f64 to avoid integer overflow.
    /// Source: `zStructSpatialGrid.h` origin definition (lower-left corner of cell 0,0).
    pub fn extent(&self) -> [f64; 4] {
        let size = f64::from(self.cell_size);
        let x =
            f64::from(self.x_coord_of_grid_cell_zero) + f64::from(self.lower_left_cell_x) * size;
        let y =
            f64::from(self.y_coord_of_grid_cell_zero) + f64::from(self.lower_left_cell_y) * size;
        [x, y, x + self.nx as f64 * size, y + self.ny as f64 * size]
    }
}

#[derive(Debug)]
pub enum GridError {
    Dss6(crate::dss6::Dss6Error),
    Dss7(crate::dss7::Dss7Error),
    NotFound,
    UnsupportedRecordType {
        record_type: i32,
    },
    UnsupportedStructVersion {
        version: i32,
    },
    UnsupportedCompression {
        method: i32,
    },
    UnsupportedStorageType {
        storage_type: i32,
    },
    PasswordProtected,
    LimitExceeded {
        field: &'static str,
        count: u64,
        limit: usize,
    },
    Corrupt {
        detail: String,
    },
}

impl fmt::Display for GridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dss6(e) => e.fmt(f),
            Self::Dss7(e) => e.fmt(f),
            Self::NotFound => write!(f, "grid record not found"),
            Self::UnsupportedRecordType { record_type } => {
                write!(f, "unsupported grid record type {record_type}")
            }
            Self::UnsupportedStructVersion { version } => {
                write!(f, "unsupported grid header version {version}")
            }
            Self::UnsupportedCompression { method } => {
                write!(f, "unsupported grid compression {method}")
            }
            Self::UnsupportedStorageType { storage_type } => {
                write!(f, "unsupported grid storage type {storage_type}")
            }
            Self::PasswordProtected => write!(f, "grid record is password protected"),
            Self::LimitExceeded {
                field,
                count,
                limit,
            } => write!(f, "grid {field} {count} exceeds limit {limit}"),
            Self::Corrupt { detail } => write!(f, "corrupt grid: {detail}"),
        }
    }
}
impl std::error::Error for GridError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Dss6(e) => Some(e),
            Self::Dss7(e) => Some(e),
            _ => None,
        }
    }
}
impl From<crate::dss6::Dss6Error> for GridError {
    fn from(e: crate::dss6::Dss6Error) -> Self {
        Self::Dss6(e)
    }
}
impl From<crate::dss7::Dss7Error> for GridError {
    fn from(e: crate::dss7::Dss7Error) -> Self {
        Self::Dss7(e)
    }
}

pub(crate) fn bad(detail: impl Into<String>) -> GridError {
    GridError::Corrupt {
        detail: detail.into(),
    }
}
pub(crate) fn bounded(n: i64, limit: usize, field: &'static str) -> Result<usize, GridError> {
    let n = usize::try_from(n).map_err(|_| bad(format!("negative/oversized {field}")))?;
    if n > limit {
        return Err(GridError::LimitExceeded {
            field,
            count: n as u64,
            limit,
        });
    }
    Ok(n)
}
pub(crate) fn cells(nx: i32, ny: i32, opts: &GridReadOptions) -> Result<usize, GridError> {
    if nx <= 0 || ny <= 0 {
        return Err(bad("non-positive grid dimensions"));
    }
    let n = i64::from(nx) * i64::from(ny);
    bounded(n, opts.max_cells.min(usize::MAX / 4), "cells")
}

pub(crate) fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes([v[0], v[1], v[2], v[3]]))
        .collect()
}
pub(crate) fn ints(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(4)
        .map(|v| i32::from_le_bytes([v[0], v[1], v[2], v[3]]))
        .collect()
}
pub(crate) fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&v| v == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end()
        .to_string()
}

/// Java heclib `GridInfoFlat.unflatten`: fixed 63-int base (20 range slots).
/// Positive int 0 is a byte count, NOT a struct revision. DSS7 can store it too
/// (`zspatialGridRetrieveVersion` returns 0 for any nonnegative first int).
pub(crate) fn legacy_header(
    path: &[u8],
    rt: i32,
    h: &[i32],
    opts: &GridReadOptions,
) -> Result<Grid, GridError> {
    let kind = GridType::from_record_type(rt)?;
    if h.len() < 63 {
        return Err(bad("legacy grid header shorter than 63 ints"));
    }
    let size = bounded(i64::from(h[0]), opts.max_header_bytes, "header bytes")?;
    // SpecifiedGridInfoFlat constructs _infoFlatSize before filling variable
    // strings: it can retain 296 although KINIHE includes the longer strings.
    // unflatten uses the actual transfer length, never this stale size as a stride.
    // Nor as a bound. `GridInfoFlat.unflatten` (Monolith 3.3.27, offsets 0-18)
    // stores int 0 in `_infoFlatSize` and reads only the array it was given;
    // `SpecifiedGridInfoFlat.unflatten` checks each count against `arraylength`. So a
    // byte count larger than the stored header (pydsstools `put_grid0` writes 764 into
    // a 183-int header) is accepted; every read below is bounded by `h.len()`.
    if size % 4 != 0 || size < 252 || h[1] != rt {
        return Err(bad("legacy header size/type mismatch"));
    }
    cells(h[12], h[13], opts)?;
    let nr = bounded(i64::from(h[22]), 20, "ranges")?;
    let f = |i: usize| f32::from_bits(h[i] as u32);
    let bytes: Vec<u8> = h.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut grid = Grid {
        pathname: path.to_vec(),
        record_type: rt,
        grid_type: kind,
        struct_version: h[0],
        version: 1,
        info_size: Some(h[2]),
        grid_info_size: Some(h[3]),
        data_units: text(&bytes[24..36]),
        data_type: h[9],
        data_source: String::new(),
        lower_left_cell_x: h[10],
        lower_left_cell_y: h[11],
        nx: h[12] as usize,
        ny: h[13] as usize,
        cell_size: f(14),
        x_coord_of_grid_cell_zero: 0.0,
        y_coord_of_grid_cell_zero: 0.0,
        srs_name: String::new(),
        srs_definition_type: 0,
        srs_definition: String::new(),
        stored_srs_definition: None,
        albers: None,
        null_value: GRID_UNDEFINED,
        time_zone_id: String::new(),
        time_zone_raw_offset: 0,
        is_interval: matches!(h[9], 0 | 1),
        is_time_stamped: rt % 2 == 0,
        // An unset HecTime is stored as -2147483647 (`UNDEFINED_TIME`,
        // `headers/heclibDate.h`); Monolith writes it for grids without times and
        // reports no time for it (`GridInfo.getStartTime` returns "").
        start_time: defined_minutes(h[4]).then_some(i64::from(h[4]) * 60),
        end_time: defined_minutes(h[5]).then_some(i64::from(h[5]) * 60),
        stored_start_time: Some(h[4]),
        stored_end_time: Some(h[5]),
        compression_method: h[15],
        sizeof_compressed_elements: bounded(
            i64::from(h[16]),
            opts.max_compressed_bytes,
            "compressed bytes",
        )?,
        compression_scale_factor: f(17),
        compression_base: f(18),
        storage_data_type: 0,
        max_data_value: Some(f(19)),
        min_data_value: Some(f(20)),
        mean_data_value: Some(f(21)),
        range_limit_table: (23..23 + nr).map(f).collect(),
        number_equal_or_exceeding_range_limit: h[43..43 + nr].to_vec(),
        data: Vec::new(),
    };
    // Java heclib HrapInfoFlat / AlbersInfoFlat append 3 / 12 ints at base offset 63.
    match kind {
        GridType::Hrap => {
            if h.len() < 66 {
                return Err(bad("truncated HRAP header"));
            }
            grid.data_source = text(&bytes[252..264]);
            grid.srs_name = "HRAP".into();
            grid.srs_definition = HRAP_WKT.into();
        }
        GridType::Albers => {
            if h.len() < 75 {
                return Err(bad("truncated Albers header"));
            }
            grid.albers = Some(AlbersProjection {
                datum: h[63],
                units: text(&bytes[256..268]),
                first_standard_parallel: f(67),
                second_standard_parallel: f(68),
                central_meridian: f(69),
                latitude_of_projection_origin: f(70),
                false_easting: f(71),
                false_northing: f(72),
            });
            grid.x_coord_of_grid_cell_zero = f(73);
            grid.y_coord_of_grid_cell_zero = f(74);
            grid.srs_name = "ALBERS".into();
            // AlbersInfo.convertToGridInfo: Heclib.holString(_projectionUnits, 12)
            // yields the bytes before the first NUL, with blanks kept (checked against
            // Monolith 3.3.27). `albers.units` stays the trimmed display text.
            let units = &bytes[256..268];
            let units = &units[..units.iter().position(|&v| v == 0).unwrap_or(12)];
            grid.srs_definition = albers_srs(&AlbersSrsInputs {
                datum: h[63],
                units_are_meters: is_meters(units),
                parameters: [f(67), f(68), f(69), f(70), f(71), f(72)],
                x_coord_of_grid_cell_zero: f(73),
                y_coord_of_grid_cell_zero: f(74),
            });
        }
        GridType::Specified => {
            // SpecifiedGridInfoFlat.unflatten: lengths are int counts, not bytes.
            let mut c = HeaderCursor { h, pos: 63 };
            grid.version = c.int()?;
            grid.srs_name = c.string()?;
            grid.srs_definition_type = c.int()?;
            grid.srs_definition = c.string()?;
            grid.x_coord_of_grid_cell_zero = c.float()?;
            grid.y_coord_of_grid_cell_zero = c.float()?;
            grid.null_value = c.float()?;
            grid.time_zone_id = c.string()?;
            grid.time_zone_raw_offset = c.int()?;
            grid.is_interval = c.int()? != 0;
            grid.is_time_stamped = c.int()? != 0;
        }
        GridType::Undefined => {}
    }
    if !grid.is_time_stamped {
        grid.start_time = None;
        grid.end_time = None;
    }
    validate_geometry(&grid)?;
    Ok(grid)
}

/// True when a legacy header time word holds a time (neither `i32::MIN` nor HEC's
/// `UNDEFINED_TIME` -2147483647).
fn defined_minutes(v: i32) -> bool {
    v != i32::MIN && v != -2_147_483_647
}

struct HeaderCursor<'a> {
    h: &'a [i32],
    pos: usize,
}
impl HeaderCursor<'_> {
    fn int(&mut self) -> Result<i32, GridError> {
        let v = *self
            .h
            .get(self.pos)
            .ok_or_else(|| bad("truncated specified header"))?;
        self.pos += 1;
        Ok(v)
    }
    fn float(&mut self) -> Result<f32, GridError> {
        Ok(f32::from_bits(self.int()? as u32))
    }
    fn string(&mut self) -> Result<String, GridError> {
        let count = bounded(i64::from(self.int()?), self.h.len(), "string words")?;
        let end = self
            .pos
            .checked_add(count)
            .ok_or_else(|| bad("string length overflow"))?;
        let words = self
            .h
            .get(self.pos..end)
            .ok_or_else(|| bad("truncated specified string"))?;
        self.pos = end;
        Ok(text(
            &words
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        ))
    }
}

pub(crate) fn validate_geometry(g: &Grid) -> Result<(), GridError> {
    if !g.cell_size.is_finite()
        || g.cell_size <= 0.0
        || !g.x_coord_of_grid_cell_zero.is_finite()
        || !g.y_coord_of_grid_cell_zero.is_finite()
    {
        return Err(bad("non-finite origin or non-positive cell size"));
    }
    Ok(())
}

/// Decode with a bounded output. Source: Java heclib `DataCompression.uncompress`:
/// 0xxxx = literal, 10xxxx = zero run, 11xxxx = missing run. The scale arithmetic
/// is double precision followed by f32 rounding, NOT f32 division.
pub(crate) fn decode_data(
    g: &Grid,
    bytes: &[u8],
    legacy: bool,
    opts: &GridReadOptions,
) -> Result<Vec<f32>, GridError> {
    let n =
        g.nx.checked_mul(g.ny)
            .ok_or_else(|| bad("cell count overflow"))?;
    let stream = bytes
        .get(..g.sizeof_compressed_elements)
        .ok_or_else(|| bad("compressed byte count exceeds data area"))?;
    match g.compression_method {
        1 => {
            let want = n.checked_mul(4).ok_or_else(|| bad("cell bytes overflow"))?;
            Ok(floats(
                bytes
                    .get(..want)
                    .ok_or_else(|| bad("truncated uncompressed grid"))?,
            ))
        }
        26 => {
            let raw = inflate_grid(stream, n, !legacy)?;
            let mut values = floats(&raw);
            if legacy && !opts.preserve_legacy_missing {
                // DataCompression.zuncompress normalizes <= -1e38; then
                // GriddedData.retrieveGriddedData normalizes the specified null
                // or (for HRAP/Albers) values outside stored min/max.
                let min = g.min_data_value.unwrap_or(f32::NEG_INFINITY);
                let max = g.max_data_value.unwrap_or(f32::INFINITY);
                for value in &mut values {
                    if *value <= -1.0e38
                        || (g.grid_type == GridType::Specified && *value == g.null_value)
                        || (g.grid_type != GridType::Specified && (*value < min || *value > max))
                    {
                        *value = GRID_UNDEFINED;
                    }
                }
            }
            Ok(values)
        }
        101001 => {
            // GriddedData.retrieveGriddedData swaps the two shorts in each int
            // on Windows before DataCompression.uncompress. The final half-word
            // can be padding; swap BEFORE applying sizeofCompressedElements.
            let swapped: Vec<_> = bytes
                .chunks_exact(4)
                .flat_map(|b| [b[2], b[3], b[0], b[1]])
                .collect();
            let stream = swapped
                .get(..g.sizeof_compressed_elements)
                .ok_or_else(|| bad("truncated PRECIP_2_BYTE words"))?;
            if stream.len() % 2 != 0 {
                return Err(bad("odd PRECIP_2_BYTE byte count"));
            }
            if !g.compression_scale_factor.is_finite()
                || g.compression_scale_factor <= 0.0
                || !g.compression_base.is_finite()
            {
                return Err(bad("invalid PRECIP_2_BYTE scale/base"));
            }
            let mut out = Vec::new();
            out.try_reserve_exact(n)
                .map_err(|_| bad("cannot allocate cells"))?;
            // zreadx returns shorts in logical little-endian word order.
            for b in stream.chunks_exact(2) {
                let v = u16::from_le_bytes([b[0], b[1]]);
                let (count, value) = if v & 0x8000 == 0 {
                    (
                        1,
                        (f64::from(v) / f64::from(g.compression_scale_factor)
                            + f64::from(g.compression_base)) as f32,
                    )
                } else {
                    // A zero-length run is valid: DataCompression.compress ends a run of
                    // exactly 16000*k cells with one, and uncompress adds no cells.
                    let count = usize::from(v & 0x3fff);
                    let value = if v & 0xc000 == 0xc000 {
                        GRID_UNDEFINED
                    } else {
                        (f64::from(g.compression_base)) as f32
                    };
                    (count, value)
                };
                if count > n - out.len() {
                    return Err(bad("PRECIP_2_BYTE run exceeds dimensions"));
                }
                out.resize(out.len() + count, value);
            }
            if out.len() != n {
                return Err(bad("PRECIP_2_BYTE stream underfills dimensions"));
            }
            Ok(out)
        }
        method => Err(GridError::UnsupportedCompression { method }),
    }
}

fn inflate_grid(stream: &[u8], cells: usize, modern: bool) -> Result<Vec<u8>, GridError> {
    use miniz_oxide::inflate::stream::{inflate, InflateState};
    use miniz_oxide::{DataFormat, MZFlush, MZStatus};
    let size = cells
        .checked_mul(4)
        .ok_or_else(|| bad("cell byte count overflow"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .map_err(|_| bad("cannot allocate inflated cells"))?;
    out.resize(size, 0);
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    let first = inflate(&mut state, stream, &mut out, MZFlush::None);
    if first.bytes_written != size {
        return Err(bad("zlib grid length/checksum/stream error"));
    }
    if first.status == Ok(MZStatus::StreamEnd) {
        if first.bytes_consumed != stream.len() {
            return Err(bad("trailing zlib bytes"));
        }
        return Ok(out);
    }
    // zspatialGridStore.c compresses ceil(n/2)*2 floats for -100, adding one
    // alignment float when n is odd. Retain at most nx*ny*4 output bytes; consume
    // only this known padding into a fixed scratch buffer and require stream end.
    let mut pad = [0u8; 4];
    let pad_limit = if modern && cells % 2 == 1 { 4 } else { 0 };
    let rest = inflate(
        &mut state,
        &stream[first.bytes_consumed..],
        &mut pad[..pad_limit],
        MZFlush::Finish,
    );
    if rest.status != Ok(MZStatus::StreamEnd)
        || first.bytes_consumed + rest.bytes_consumed != stream.len()
        || rest.bytes_written != pad_limit
    {
        return Err(bad(
            "zlib grid exceeds dimensions or has invalid checksum/stream",
        ));
    }
    Ok(out)
}

/// Modern grids keep their time in pathname D/E (`GriddedData.retrieveGriddedContainer`).
pub(crate) fn pathname_time(part: &[u8]) -> Option<i64> {
    let colon = part.iter().position(|&v| v == b':')?;
    let julian = crate::dss7::interval::parse_date(&part[..colon])?;
    let time = part.get(colon + 1..)?;
    if time.len() != 4 || !time.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let hour = i64::from(time[0] - b'0') * 10 + i64::from(time[1] - b'0');
    let minute = i64::from(time[2] - b'0') * 10 + i64::from(time[3] - b'0');
    if hour > 24 || minute > 59 || (hour == 24 && minute != 0) {
        return None;
    }
    Some(julian * 86400 + hour * 3600 + minute * 60)
}

// zStructSpatialGrid.h HRAP_SRC_DEFINITION (HEC spelling retained).
pub(crate) const HRAP_WKT: &str = "PROJCS[\"Stereographic_CONUS_HRAP\",GEOGCS[\"GCS_Sphere_LFM\",DATUM[\"D_Sphere_LFM\",SPHEROID[\"Shpere_LFM\",6371200.0,0.0]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Stereographic_North_Pole\"],PARAMETER[\"False_Easting\",1909762.5],PARAMETER[\"False_Northing\",7624762.5],PARAMETER[\"Central_Meridian\",-105.0],PARAMETER[\"Standard_Parallel_1\",60.0],UNIT[\"Meter\",1.0]]";

/// The fields Monolith 3.3.27 `AlbersInfo.getSpatialReferenceSystem` reads.
/// `parameters`: first/second standard parallel, central meridian, latitude of
/// projection origin, false easting, false northing.
pub(crate) struct AlbersSrsInputs {
    pub datum: i32,
    pub units_are_meters: bool,
    pub parameters: [f32; 6],
    pub x_coord_of_grid_cell_zero: f32,
    pub y_coord_of_grid_cell_zero: f32,
}

/// `AlbersInfo.isMeters`: `String.toLowerCase()` then an exact `equals` against
/// "meters", "meter", "m", "metres" or "metre"; no trimming. The candidates are
/// ASCII, so only ASCII input can match and ASCII lower-casing is exact.
pub(crate) fn is_meters(units: &[u8]) -> bool {
    let u = units.to_ascii_lowercase();
    [&b"meters"[..], b"meter", b"m", b"metres", b"metre"].contains(&u.as_slice())
}

/// `AlbersInfo.getSpatialReferenceSystem`: the SHG WKT when datum == 2 (NAD83),
/// the units are metres (`is_meters`), the six projection parameters equal
/// 29.5, 45.5, -96.0, 23.0, 0.0, 0.0 and the grid-cell-zero origin is 0/0 (Java
/// `fcmpl`/`dcmpl`: NaN never matches, -0.0 matches 0.0); otherwise "". No other
/// WKT is produced; the parameters of non-SHG legacy grids stay in `Grid.albers`
/// (and the stored string of -100 grids in `Grid.stored_srs_definition`).
pub(crate) fn albers_srs(a: &AlbersSrsInputs) -> String {
    if a.datum == 2
        && a.units_are_meters
        && a.parameters == [29.5, 45.5, -96.0, 23.0, 0.0, 0.0]
        && a.x_coord_of_grid_cell_zero == 0.0
        && a.y_coord_of_grid_cell_zero == 0.0
    {
        SHG_WKT.into()
    } else {
        String::new()
    }
}

/// Modern (-100) Albers: Monolith `GridStructContainer.returnGridInfo` builds an
/// `AlbersInfo` and fills it from the stored WKT with
/// `setAlbersSpatialReferenceInfo` (regular expressions, emulated exactly here);
/// the origin comes from the record geometry. Fields the WKT does not supply keep
/// the values that method leaves: datum 0, parameters 0.0, units "".
pub(crate) fn albers_srs_from_wkt(wkt: &str, x0: f32, y0: f32) -> String {
    let a = albers_from_wkt(wkt);
    albers_srs(&AlbersSrsInputs {
        datum: a.datum,
        units_are_meters: is_meters(a.units.as_bytes()),
        parameters: [
            a.first_standard_parallel,
            a.second_standard_parallel,
            a.central_meridian,
            a.latitude_of_projection_origin,
            a.false_easting,
            a.false_northing,
        ],
        x_coord_of_grid_cell_zero: x0,
        y_coord_of_grid_cell_zero: y0,
    })
}

/// Modern (-100) Albers projection parameters as Monolith reports them.
/// `GridStructContainer.returnGridInfo` (offsets 288-326 in `javap -c` of Monolith
/// 3.3.27) creates `new AlbersInfo()` and calls `setAlbersSpatialReferenceInfo(info,
/// _srsDefinition)`, which sets, in order: the datum from
/// `DATUM\["D_North_American_19(\d\d)` (offsets 0-99: datum 0, then 27 -> 1,
/// 83 -> 2; no match leaves the constructor's 0); each of the six parameters from
/// `getParameter` (offsets 102-157; no match or a parse failure gives 0.0); and the
/// units from the last `UNIT\["(.*?)"` match (offsets 160-193; no match leaves the
/// constructor's empty string). Same emulation as `albers_srs_from_wkt`.
pub(crate) fn albers_from_wkt(wkt: &str) -> AlbersProjection {
    let p = |name| wkt_parameter(wkt, name);
    AlbersProjection {
        datum: wkt_datum(wkt),
        units: wkt_last_unit(wkt).unwrap_or("").to_string(),
        first_standard_parallel: p("Standard_Parallel_1"),
        second_standard_parallel: p("Standard_Parallel_2"),
        central_meridian: p("Central_Meridian"),
        latitude_of_projection_origin: p("Latitude_Of_Origin"),
        false_easting: p("False_Easting"),
        false_northing: p("False_Northing"),
    }
}

/// `DATUM\["D_North_American_19(\d\d)` (first match): 27 -> 1, 83 -> 2, any other
/// two digits -> 0; no match -> 0 (the `AlbersInfo()` default).
fn wkt_datum(wkt: &str) -> i32 {
    const KEY: &str = "DATUM[\"D_North_American_19";
    let b = wkt.as_bytes();
    let mut from = 0;
    while let Some(i) = wkt[from..].find(KEY).map(|i| from + i) {
        let j = i + KEY.len();
        if b.len() >= j + 2 && b[j].is_ascii_digit() && b[j + 1].is_ascii_digit() {
            return match &b[j..j + 2] {
                b"27" => 1,
                b"83" => 2,
                _ => 0,
            };
        }
        from = i + 1;
    }
    0
}

/// `getParameter`: first match of `PARAMETER\["<name>",(-?\d*\.*\d+)`, parsed with
/// `Float.parseFloat`; no match or a parse failure (for example two dots) -> 0.0.
fn wkt_parameter(wkt: &str, name: &str) -> f32 {
    let key = format!("PARAMETER[\"{name}\",");
    let b = wkt.as_bytes();
    let digits = |k: usize| b[k..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut from = 0;
    while let Some(i) = wkt[from..].find(&key).map(|i| from + i) {
        let start = i + key.len();
        let j = start + usize::from(b.get(start) == Some(&b'-'));
        let a = digits(j);
        let dots = b[j + a..].iter().take_while(|&&c| c == b'.').count();
        let c = digits(j + a + dots);
        // Greedy `\d*\.*\d+` with backtracking: digits.dots.digits when the second
        // digit run is present, else the first digit run alone (needs >= 1 digit).
        let end = if dots > 0 && c > 0 {
            j + a + dots + c
        } else if a > 0 {
            j + a
        } else {
            from = i + 1;
            continue;
        };
        return wkt[start..end].parse::<f32>().unwrap_or(0.0);
    }
    0.0
}

/// `UNIT\["(.*?)"` applied with `find()` until it fails; the last match wins. `.`
/// excludes Java's line terminators.
fn wkt_last_unit(wkt: &str) -> Option<&str> {
    const KEY: &str = "UNIT[\"";
    let is_eol = |c: char| matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}');
    let mut last = None;
    let mut from = 0;
    while let Some(i) = wkt[from..].find(KEY).map(|i| from + i) {
        let start = i + KEY.len();
        match wkt[start..].find(|c: char| c == '"' || is_eol(c)) {
            Some(q) if wkt[start + q..].starts_with('"') => {
                last = Some(&wkt[start..start + q]);
                from = start + q + 1;
            }
            _ => from = i + 1,
        }
    }
    last
}

// zStructSpatialGrid.h:SHG_SRC_DEFINITION, spelling retained; identical to the
// constant returned by Monolith AlbersInfo.getSpatialReferenceSystem.
const SHG_WKT: &str = "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]";

#[cfg(test)]
mod albers_srs_tests {
    use super::*;

    #[test]
    fn is_meters_matches_albers_info_spellings_only() {
        for u in [
            "Meters", "METERS", "meter", "M", "m", "metres", "Metre", "metre",
        ] {
            assert!(is_meters(u.as_bytes()), "{u}");
        }
        for u in [
            "", "Meters  ", " Meters", "Meter s", "Feet", "km", "Meters\0",
        ] {
            assert!(!is_meters(u.as_bytes()), "{u:?}");
        }
    }

    #[test]
    fn wkt_parameter_follows_java_regex() {
        let w = |v: &str| format!("PARAMETER[\"X\",{v}]");
        assert_eq!(wkt_parameter(&w("-96.0"), "X"), -96.0);
        assert_eq!(wkt_parameter(&w("-96"), "X"), -96.0);
        assert_eq!(wkt_parameter(&w(".5"), "X"), 0.5);
        assert_eq!(wkt_parameter(&w("29.5e3"), "X"), 29.5);
        assert_eq!(wkt_parameter(&w("7."), "X"), 7.0);
        assert_eq!(wkt_parameter(&w("2..5"), "X"), 0.0); // parseFloat fails -> 0
        assert_eq!(wkt_parameter(&w(" 5.0"), "X"), 0.0); // no match -> 0
        assert_eq!(wkt_parameter(&w("-x"), "X"), 0.0);
        assert_eq!(wkt_parameter("", "X"), 0.0);
        // First matching occurrence wins; a non-matching earlier one is skipped.
        let two = format!("{}{}{}", w("x"), w("3.0"), w("4.0"));
        assert_eq!(wkt_parameter(&two, "X"), 3.0);
    }

    #[test]
    fn wkt_datum_and_units_follow_java_regex() {
        assert_eq!(wkt_datum("DATUM[\"D_North_American_1983\""), 2);
        assert_eq!(wkt_datum("DATUM[\"D_North_American_1983_HARN\""), 2);
        assert_eq!(wkt_datum("DATUM[\"D_North_American_1927\""), 1);
        assert_eq!(wkt_datum("DATUM[\"D_North_American_1999\""), 0);
        assert_eq!(
            wkt_datum("DATUM[\"D_North_American_19x\"DATUM[\"D_North_American_1983"),
            2
        );
        assert_eq!(wkt_datum("DATUM[\"D_NAD_1983\""), 0);
        assert_eq!(
            wkt_last_unit("UNIT[\"Degree\",1],UNIT[\"Meter\",1]"),
            Some("Meter")
        );
        assert_eq!(
            wkt_last_unit("UNIT[\"Degree\",1],UNIT[\"Me\nter\",1]"),
            Some("Degree")
        );
        assert_eq!(wkt_last_unit("UNIT[\"\",1]"), Some(""));
        assert_eq!(wkt_last_unit("UNIT[\"Meter"), None);
        assert_eq!(wkt_last_unit("TEST"), None);
    }

    #[test]
    fn shg_from_stored_wkt_needs_zero_origin() {
        assert_eq!(albers_srs_from_wkt(SHG_WKT, 0.0, 0.0), SHG_WKT);
        assert_eq!(albers_srs_from_wkt(SHG_WKT, -0.0, 0.0), SHG_WKT);
        assert_eq!(albers_srs_from_wkt(SHG_WKT, 10.0, 0.0), "");
        assert_eq!(albers_srs_from_wkt(SHG_WKT, 0.0, f32::NAN), "");
        assert_eq!(albers_srs_from_wkt("TEST", 0.0, 0.0), "");
    }
}

#[cfg(test)]
mod p60_legacy_header_tests {
    use super::*;

    /// A minimal legacy specified (430) header: 63 base ints, then the
    /// `SpecifiedGridInfoFlat` tail with empty strings, 1 x 1 cells.
    fn specified(byte_count: i32) -> Vec<i32> {
        let mut h = vec![0i32; 63];
        h[0] = byte_count;
        h[1] = 430;
        h[4] = UNDEFINED_TIME_MINUTES;
        h[5] = UNDEFINED_TIME_MINUTES;
        h[9] = 2;
        h[12] = 1;
        h[13] = 1;
        h[14] = 1.0f32.to_bits() as i32;
        // version, srsName (0 ints), srsDefinitionType, srsDefinition (0 ints), x0, y0,
        // null, timeZoneId (0 ints), raw offset, isInterval, isTimeStamped.
        h.extend([2, 0, 0, 0, 0, 0, (-9999.0f32).to_bits() as i32, 0, 0, 0, 1]);
        h
    }
    const UNDEFINED_TIME_MINUTES: i32 = -2_147_483_647;

    /// Java `GridInfoFlat.unflatten` never bounds the parse by int 0; a byte
    /// count beyond the stored header (pydsstools `put_grid0`: 764 in 183 ints) reads.
    #[test]
    fn byte_count_beyond_stored_header_reads() {
        let opts = GridReadOptions::default();
        for count in [296, 764, 4 * 74 + 4, 1_048_576] {
            let h = specified(count);
            let g = legacy_header(b"/A/B/C///F/", 430, &h, &opts).unwrap();
            assert_eq!((g.struct_version, g.nx, g.ny), (count, 1, 1));
            assert_eq!(g.null_value, -9999.0);
        }
    }

    /// Still bounded: oversized or malformed counts and truncated tails are typed errors.
    #[test]
    fn byte_count_and_tail_still_checked() {
        let opts = GridReadOptions::default();
        let h = specified(1_048_580);
        assert!(matches!(
            legacy_header(b"/A/B/C///F/", 430, &h, &opts),
            Err(GridError::LimitExceeded { .. })
        ));
        for count in [250, 765] {
            let h = specified(count);
            assert!(
                legacy_header(b"/A/B/C///F/", 430, &h, &opts).is_err(),
                "{count}"
            );
        }
        // A string count that runs past the stored ints: error, no panic.
        let mut h = specified(764);
        h[64] = 1_000;
        assert!(legacy_header(b"/A/B/C///F/", 430, &h, &opts).is_err());
        for n in 63..h.len() {
            let _ = legacy_header(b"/A/B/C///F/", 430, &specified(764)[..n], &opts);
        }
    }
}

//! Grid writer, version-neutral part (packet P24): input, validation, statistics and
//! compression shared by the DSS7 (`dss7::write::write_grid`) and DSS6
//! (`dss6::write::write_grid6`) grid writers. Specification:
//! `packets/P24_grid_writer.md`.
//!
//! Sources (tag `7-IU-15` of `hec-dss`, paths relative to `heclib/`, and HEC Monolith
//! 3.3.27 classes read with `javap -p -c`):
//!
//! - DSS7 layout: `heclib_c/src/Public/zspatialGridStore.c:zspatialGridStore()`, fed the
//!   way Monolith feeds it (`hec.heclib.grid.GriddedData.storeGriddedContainer` ->
//!   `GridStructContainer(GridInfo)` -> `javaHeclib/src/Hec_zgridStore.c`).
//! - DSS6 layout: `GriddedData.storeGriddedData` (DSS6 branch) -> `GridInfoFlat.flatten`
//!   and the HRAP, Albers and specified subclasses -> `javaHeclib/src/Hec_zwritx.c` ->
//!   `heclib_c/src/DssInterface/v6and7/zwritex.c` -> `heclib_f/src/zwritex6.f`.
//! - Statistics: `GridData.updateStatistics` (Monolith; heclib's C store takes them from
//!   the caller and computes nothing). Range table defaults: `GridInfo.<init>`.
//! - PRECIP_2_BYTE: `DataCompression.compress` and the automatic scale/base choice in
//!   `GriddedData.storeGriddedData` (offsets 390-528).
//!
//! Cells are `f32`, x varying fastest, the southern row first (`GridData.getValue`,
//! index `y * nx + x`); the lower-left cell index is that of the lower-left **corner**
//! of cell (0, 0) in units of the cell size from the cell-zero origin
//! (`zStructSpatialGrid.h`). Values arrive as `f32` (the stored width); nothing is
//! truncated or wrapped: what
//! cannot be stored exactly is refused ([`GridWriteError`]).

use crate::dss7::interval::ymd_to_julian;

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
use std::fmt;

/// HEC's grid no-data value, `GridInfo.GRID_NODATA_VALUE` (-FLT_MAX).
pub const GRID_NODATA: f32 = -f32::MAX;

/// `GridInfo.<init>` range limits (13 ranges; Monolith 3.3.27 offsets 148-263).
pub const DEFAULT_RANGE_LIMITS: [f32; 13] = [
    -f32::MAX,
    0.0,
    1.0e-7,
    5.0,
    10.0,
    20.0,
    50.0,
    100.0,
    200.0,
    500.0,
    1000.0,
    2000.0,
    5000.0,
];

/// `GridData.UNDEFINED_STORAGE`: the null value `GridData.updateStatistics` excludes for
/// HRAP and Albers grids, and the value `GriddedData.storeGriddedData` stores in DSS6
/// cells that were -FLT_MAX (offsets 81-133).
pub const UNDEFINED_STORAGE: f32 = -1.0e30;

/// Largest cell count the writer accepts: the reader's default `max_cells`
/// ([`crate::GridReadOptions`]), so every written grid reads back with defaults.
pub const MAX_WRITE_CELLS: usize = 16_777_216;

/// Longest DSS7 grid string (units, data source, SRS name, SRS definition, time zone).
pub const MAX_GRID_STRING_BYTES: usize = 65_536;

/// Compression methods (`zStructSpatialGrid.h` `CompressionMethod`; `GridInfo`
/// constants).
pub const NO_COMPRESSION: i32 = 1;
pub const ZLIB_COMPRESSION: i32 = 26;
pub const PRECIP_2_BYTE: i32 = 101_001;

/// Errors of the grid writers before any file is touched. They map to
/// [`crate::dss7::write::WriteError`] `BadPathname` / `BadInput` / `Unsupported`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridWriteError {
    BadPathname(String),
    BadInput(String),
    Unsupported(String),
}

impl fmt::Display for GridWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GridWriteError::BadPathname(m) => write!(f, "invalid pathname: {m}"),
            GridWriteError::BadInput(m) => write!(f, "invalid input: {m}"),
            GridWriteError::Unsupported(m) => write!(f, "not supported by the writer: {m}"),
        }
    }
}

impl std::error::Error for GridWriteError {}

fn bad(m: impl Into<String>) -> GridWriteError {
    GridWriteError::BadInput(m.into())
}

fn bad_path(m: impl Into<String>) -> GridWriteError {
    GridWriteError::BadPathname(m.into())
}

/// Albers equal-area projection parameters (`AlbersInfo.setProjectionInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct AlbersParameters {
    /// `GridInfo` datum code: 0 undefined, 1 NAD27, 2 NAD83.
    pub datum: i32,
    /// Projection units (`METERS` for SHG). At most 12 bytes on DSS6.
    pub units: Vec<u8>,
    pub first_standard_parallel: f32,
    pub second_standard_parallel: f32,
    pub central_meridian: f32,
    pub latitude_of_projection_origin: f32,
    pub false_easting: f32,
    pub false_northing: f32,
}

impl AlbersParameters {
    /// HEC's Standard Hydrologic Grid (SHG): NAD83, metres, 29.5/45.5/-96/23, no false
    /// origin (`zStructSpatialGrid.h` `SHG_SRC_DEFINITION`).
    pub fn shg() -> Self {
        AlbersParameters {
            datum: 2,
            units: b"METERS".to_vec(),
            first_standard_parallel: 29.5,
            second_standard_parallel: 45.5,
            central_meridian: -96.0,
            latitude_of_projection_origin: 23.0,
            false_easting: 0.0,
            false_northing: 0.0,
        }
    }
}

/// A specified grid's spatial reference and metadata (`SpecifiedGridInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct SpecifiedReference {
    pub srs_name: Vec<u8>,
    /// Usually WKT (`srs_definition_type` 0).
    pub srs_definition: Vec<u8>,
    pub srs_definition_type: i32,
    /// No-data value in the cells (`SpecifiedGridInfo._nullValue`, default -FLT_MAX).
    pub null_value: f32,
    /// `SpecifiedGridInfo` default `UTC`.
    pub time_zone_id: Vec<u8>,
    /// Milliseconds from UTC.
    pub time_zone_raw_offset: i32,
    /// Period data (true for PER-AVER / PER-CUM).
    pub is_interval: bool,
}

/// The grid's projection and its type-specific metadata.
#[derive(Debug, Clone, PartialEq)]
pub enum GridProjection {
    /// HRAP (410/411); `data_source` at most 12 bytes on DSS6.
    Hrap { data_source: Vec<u8> },
    /// Albers (420/421).
    Albers(AlbersParameters),
    /// Specified spatial reference (430/431).
    Specified(SpecifiedReference),
}

impl GridProjection {
    /// Record type of the time-stamped form (`zdataTypeDescriptions.h`).
    pub fn base_record_type(&self) -> i32 {
        match self {
            GridProjection::Hrap { .. } => 410,
            GridProjection::Albers(_) => 420,
            GridProjection::Specified(_) => 430,
        }
    }

    /// `GridInfo._version`: 1, specified grids 2 (`SpecifiedGridInfo.<init>`).
    pub fn grid_info_version(&self) -> i32 {
        match self {
            GridProjection::Specified(_) => 2,
            _ => 1,
        }
    }

    /// Value `GridData.updateStatistics` excludes from the statistics, and that DSS6
    /// stores for -FLT_MAX cells: the specified null value, else -1.0E30.
    pub fn statistics_null(&self) -> f32 {
        match self {
            GridProjection::Specified(s) => s.null_value,
            _ => UNDEFINED_STORAGE,
        }
    }
}

/// Cell compression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridCompression {
    /// ZLIB deflate (26): DSS7 (as Monolith always writes) and DSS6.
    Zlib,
    /// No compression (1): DSS7 only (`zspatialGridStore.c` `NO_COMPRESSION`); Monolith's
    /// DSS6 store has no uncompressed path.
    Uncompressed,
    /// HEC's PRECIP_2_BYTE run-length code (101001): DSS6 only; heclib's DSS7 store refuses
    /// it ("Unsupported Compression"). Quantized by design (ordinary values are rounded
    /// to the nearest 1/scale step; accepted): values are stored as
    /// `round((v - base) * scale)` in 15 bits. `None` for both lets the writer choose them
    /// as Monolith does.
    Precip2Byte {
        scale_factor: Option<f32>,
        base: Option<f32>,
    },
}

/// HEC grid data type (`zStructSpatialGrid.h` `dataType`).
pub const PER_AVER: i32 = 0;
pub const PER_CUM: i32 = 1;
pub const INST_VAL: i32 = 2;
pub const INST_CUM: i32 = 3;

/// One grid record to store.
#[derive(Debug, Clone)]
pub struct GridInput<'a> {
    /// `/A/B/C/D/E/F/`. Time-stamped grids: D `DDMMMYYYY:HHMM` (period start, or the
    /// instant), E the period end in the same form (required for PER-AVER / PER-CUM, and
    /// later than D), or empty for an instantaneous grid; INST-VAL / INST-CUM grids may
    /// also leave both empty (no times, as Monolith stores them). Time-invariant grids:
    /// D and E empty.
    pub pathname: &'a [u8],
    pub projection: GridProjection,
    /// Time-stamped (410/420/430) when true. When false (empty D and E, no times) the
    /// record type is what HEC writes and Monolith reads back: DSS7 specified grids 431,
    /// every other case 410/420/430 (see [`plan_grid`], P60).
    pub time_stamped: bool,
    pub data: &'a [f32],
    pub nx: usize,
    pub ny: usize,
    pub lower_left_cell_x: i32,
    pub lower_left_cell_y: i32,
    pub cell_size: f32,
    /// Coordinates of the lower-left corner of cell (0, 0). Albers and specified grids;
    /// must be 0 for HRAP grids, which have a fixed origin.
    pub x_coord_of_grid_cell_zero: f32,
    pub y_coord_of_grid_cell_zero: f32,
    pub units: &'a [u8],
    /// [`PER_AVER`], [`PER_CUM`], [`INST_VAL`] or [`INST_CUM`].
    pub data_type: i32,
    pub compression: GridCompression,
}

/// Statistics as `GridData.updateStatistics` computes them.
#[derive(Debug, Clone, PartialEq)]
pub struct GridStatistics {
    pub min: f32,
    pub max: f32,
    pub mean: f32,
    pub range_limits: Vec<f32>,
    pub range_counts: Vec<i32>,
}

/// `GridData.updateStatistics` (Monolith 3.3.27 offsets 9-461), bit for bit:
///
/// - initial max -2.0E38, min 2.0E38, mean -FLT_MAX (kept when no cell counts);
/// - cells equal to -FLT_MAX or to `null` are excluded (JVM `fcmpl`, so NaN cells count,
///   and make the mean NaN, but never become the min or max);
/// - the loop runs x outer, y inner, summing in `float`; mean = sum / count in `float`;
/// - every cell, excluded or not, counts toward each range limit it equals or exceeds
///   (NaN toward none).
pub fn grid_statistics(data: &[f32], nx: usize, ny: usize, null: f32) -> GridStatistics {
    let ranges = DEFAULT_RANGE_LIMITS;
    let mut counts = vec![0i32; ranges.len()];
    let (mut max, mut min, mut sum, mut n) = (-2.0e38f32, 2.0e38f32, 0.0f32, 0i64);
    for x in 0..nx {
        for y in 0..ny {
            let i = y * nx + x;
            let Some(&v) = data.get(i) else { continue };
            if v != -f32::MAX && v != null {
                n += 1;
                sum += v;
                if v > max {
                    max = v;
                }
                if v < min {
                    min = v;
                }
            }
            for (c, &lim) in counts.iter_mut().zip(ranges.iter()) {
                if v >= lim {
                    *c += 1;
                }
            }
        }
    }
    let mean = if n > 0 { sum / n as f32 } else { -f32::MAX };
    GridStatistics {
        min,
        max,
        mean,
        range_limits: ranges.to_vec(),
        range_counts: counts,
    }
}

/// A grid time in a D or E part: `DDMMMYYYY:HHMM` exactly, as Monolith writes grid
/// pathnames (`HecTime.getXMLDateTime`-style `ddMMMyyyy:HHmm`). Returns (julian,
/// seconds past midnight); `2400` gives 86400 seconds of the same day.
pub fn parse_grid_time(part: &[u8]) -> Option<(i64, i64)> {
    if part.len() != 14 || part[9] != b':' {
        return None;
    }
    let digits = |s: &[u8]| -> Option<i64> {
        if s.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(s).ok()?.parse().ok()
        } else {
            None
        }
    };
    let day = digits(&part[0..2])?;
    let month = MONTH_NAMES
        .iter()
        .position(|m| m.as_bytes().eq_ignore_ascii_case(&part[2..5]))? as i64
        + 1;
    let year = digits(&part[5..9])?;
    let hour = digits(&part[10..12])?;
    let minute = digits(&part[12..14])?;
    if year < 1 || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    if minute > 59 || hour > 24 || (hour == 24 && minute != 0) {
        return None;
    }
    Some((ymd_to_julian(year, month, day), hour * 3600 + minute * 60))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A validated input.
#[derive(Debug, Clone)]
pub struct GridPlan {
    /// Pathname as stored (DSS6: upper case).
    pub pathname: Vec<u8>,
    pub record_type: i32,
    /// D part time (julian, seconds), E part time (julian, seconds; the D time when E is
    /// empty). `None` for time-invariant grids.
    pub start: Option<(i64, i64)>,
    pub end: Option<(i64, i64)>,
    pub stats: GridStatistics,
}

impl GridPlan {
    /// Seconds since 1899-12-31 00:00 of a (julian, seconds) pair.
    pub fn seconds(t: (i64, i64)) -> i64 {
        t.0 * 86_400 + t.1
    }
}

fn check_text(what: &str, s: &[u8], max: usize) -> Result<(), GridWriteError> {
    if s.len() > max {
        return Err(bad(format!(
            "{what} is {} bytes; the writer stores at most {max}",
            s.len()
        )));
    }
    if let Some(&b) = s
        .iter()
        .find(|&&b| b == 0 || b > 126 || (b < 32 && b != b'\n'))
    {
        return Err(bad(format!(
            "{what} byte 0x{b:02x} is not printable ASCII (HEC stores NUL-terminated text)"
        )));
    }
    Ok(())
}

/// Validate an input for either version (`version` 6 or 7) and compute its statistics.
/// Nothing is written; every refusal of the write happens here or in the version
/// layer's preflight.
pub fn plan_grid(input: &GridInput, version: u8) -> Result<GridPlan, GridWriteError> {
    // -- pathname ---------------------------------------------------------------------
    let p = input.pathname;
    if p.first() != Some(&b'/') || p.last() != Some(&b'/') {
        return Err(bad_path("pathname must start and end with '/'"));
    }
    let slashes = p.iter().filter(|&&b| b == b'/').count();
    if slashes != 7 {
        return Err(bad_path(format!(
            "pathname must have exactly 7 slashes (found {slashes})"
        )));
    }
    if let Some(&b) = p.iter().find(|&&b| !(32..=126).contains(&b)) {
        return Err(bad_path(format!(
            "pathname byte 0x{b:02x} is outside printable ASCII (HEC would drop it)"
        )));
    }
    if p.len() > 392 {
        return Err(bad_path(format!(
            "pathname is {} bytes (lookups hash at most 392)",
            p.len()
        )));
    }
    let stored: Vec<u8> = if version == 6 {
        // GriddedData.storeGriddedData passes pathname().toUpperCase() to zwritx.
        p.to_ascii_uppercase()
    } else {
        p.to_vec()
    };
    let parts: Vec<&[u8]> = stored[1..stored.len() - 1].split(|&b| b == b'/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.len() > 128 {
            return Err(bad_path(format!(
                "part {} is {} bytes (HEC limit 128)",
                (b'A' + i as u8) as char,
                part.len()
            )));
        }
    }
    if parts[5].len() >= 2 && parts[5][..2].eq_ignore_ascii_case(b"C:") {
        return Err(bad_path(
            "collection pathnames (F part C:...) are not written",
        ));
    }
    let (d, e) = (parts[3], parts[4]);
    let period = matches!(input.data_type, PER_AVER | PER_CUM);
    let (start, end) = if input.time_stamped && d.is_empty() && e.is_empty() {
        // Monolith stores a time-stamped type (410/420/430) without times when the
        // pathname has none (GriddedData.storeGriddedData with an unset GridInfo time:
        // DSS6 start/end UNDEFINED_TIME, DSS7 first/last date 0; HEC parameter grids).
        // A period needs its bounds, so PER-AVER / PER-CUM are refused here.
        if period {
            return Err(bad_path(
                "PER-AVER and PER-CUM grids need D and E parts (the period start and end)",
            ));
        }
        (None, None)
    } else if input.time_stamped {
        let s = parse_grid_time(d).ok_or_else(|| {
            bad_path(format!(
                "D part {:?} is not a grid time DDMMMYYYY:HHMM (time-stamped grid)",
                String::from_utf8_lossy(d)
            ))
        })?;
        let t = if e.is_empty() {
            None
        } else {
            Some(parse_grid_time(e).ok_or_else(|| {
                bad_path(format!(
                    "E part {:?} is neither empty nor a grid time DDMMMYYYY:HHMM",
                    String::from_utf8_lossy(e)
                ))
            })?)
        };
        match t {
            None if period => {
                return Err(bad_path(
                    "PER-AVER and PER-CUM grids need an E part (the period end)",
                ))
            }
            Some(t) if period && GridPlan::seconds(t) <= GridPlan::seconds(s) => {
                return Err(bad_path(format!(
                    "the period end (E part) must be later than its start (D part) for \
                     PER-AVER and PER-CUM grids; got {:?} to {:?}",
                    String::from_utf8_lossy(d),
                    String::from_utf8_lossy(e)
                )))
            }
            Some(t) if GridPlan::seconds(t) < GridPlan::seconds(s) => {
                return Err(bad_path("the E part time is earlier than the D part time"))
            }
            _ => {}
        }
        (Some(s), Some(t.unwrap_or(s)))
    } else {
        if !d.is_empty() || !e.is_empty() {
            return Err(bad_path(
                "time-invariant grids (record types 4x1) must have empty D and E parts",
            ));
        }
        (None, None)
    };
    // -- geometry ---------------------------------------------------------------------
    let (nx, ny) = (input.nx, input.ny);
    if nx == 0 || ny == 0 {
        return Err(bad("nx and ny must be at least 1"));
    }
    let n = nx
        .checked_mul(ny)
        .filter(|&n| n <= MAX_WRITE_CELLS)
        .ok_or_else(|| bad(format!("at most {MAX_WRITE_CELLS} cells per grid")))?;
    if i32::try_from(nx).is_err() || i32::try_from(ny).is_err() {
        return Err(bad("nx and ny must fit in 32 bits"));
    }
    if input.data.len() != n {
        return Err(bad(format!(
            "{} cells given for a {nx} x {ny} grid",
            input.data.len()
        )));
    }
    if !input.cell_size.is_finite() || input.cell_size <= 0.0 {
        return Err(bad("cell_size must be finite and positive"));
    }
    let (x0, y0) = (
        input.x_coord_of_grid_cell_zero,
        input.y_coord_of_grid_cell_zero,
    );
    if !x0.is_finite() || !y0.is_finite() {
        return Err(bad("the cell-zero origin must be finite"));
    }
    // The far edge of the grid in cell indexes must fit HEC's 32-bit cell indexes.
    for (ll, count, axis) in [
        (input.lower_left_cell_x, nx, "x"),
        (input.lower_left_cell_y, ny, "y"),
    ] {
        if i64::from(ll) + count as i64 > i64::from(i32::MAX) {
            return Err(bad(format!(
                "lower-left cell {axis} index plus the cell count exceeds 32 bits"
            )));
        }
    }
    if !(PER_AVER..=INST_CUM).contains(&input.data_type) {
        return Err(bad(format!(
            "data_type {} is not 0 (PER-AVER), 1 (PER-CUM), 2 (INST-VAL) or 3 (INST-CUM)",
            input.data_type
        )));
    }
    // -- strings and projection ----------------------------------------------------------
    let six = version == 6;
    let short = if six { 12 } else { MAX_GRID_STRING_BYTES };
    check_text("units", input.units, short)?;
    if input.units.contains(&b'\n') {
        return Err(bad("units must be one line"));
    }
    match &input.projection {
        GridProjection::Hrap { data_source } => {
            check_text("HRAP data_source", data_source, short)?;
            if x0 != 0.0 || y0 != 0.0 {
                return Err(bad(
                    "HRAP grids have a fixed origin; the cell-zero coordinates must be 0",
                ));
            }
        }
        GridProjection::Albers(a) => {
            check_text("Albers units", &a.units, short)?;
            // Monolith appends the units to the DSS7 WKT unescaped
            // (GridStructContainer.getAlbersSpatialReferenceSystem offsets 75-133,
            // StringBuilder.append); a quote or bracket would end the UNIT clause early.
            if let Some(&b) = a
                .units
                .iter()
                .find(|&&b| matches!(b, b'"' | b'[' | b']' | b','))
            {
                return Err(bad(format!(
                    "Albers units contain '{}', which would break the WKT HEC builds from them",
                    b as char
                )));
            }
            if !(0..=2).contains(&a.datum) {
                return Err(bad(
                    "Albers datum must be 0 (undefined), 1 (NAD27) or 2 (NAD83)",
                ));
            }
            for (v, what) in [
                (a.first_standard_parallel, "first standard parallel"),
                (a.second_standard_parallel, "second standard parallel"),
                (a.central_meridian, "central meridian"),
                (a.latitude_of_projection_origin, "latitude of origin"),
                (a.false_easting, "false easting"),
                (a.false_northing, "false northing"),
            ] {
                if !v.is_finite() {
                    return Err(bad(format!("Albers {what} must be finite")));
                }
            }
        }
        GridProjection::Specified(s) => {
            check_text("srs_name", &s.srs_name, MAX_GRID_STRING_BYTES)?;
            check_text("srs_definition", &s.srs_definition, MAX_GRID_STRING_BYTES)?;
            check_text("time_zone_id", &s.time_zone_id, MAX_GRID_STRING_BYTES)?;
            if !s.null_value.is_finite() {
                return Err(bad(
                    "the null value must be finite (HEC compares it with ==; NaN never \
                     matches and infinity is not a float32 HEC writes)",
                ));
            }
        }
    }
    // DSS6 stores the D/E times as HEC minutes in an int (GridInfoFlat ints 4/5,
    // HecTime.value()); -2147483647 is the unset value. Later dates would wrap.
    if version == 6 {
        for t in [start, end].into_iter().flatten() {
            let m = t.0 * 1440 + t.1 / 60;
            if m <= -2_147_483_647 || m > i64::from(i32::MAX) {
                return Err(bad_path(
                    "the grid time is outside the DSS6 range (HEC minutes in 32 bits: years \
                     up to 5982)",
                ));
            }
        }
    }
    // -- compression ---------------------------------------------------------------------
    match (input.compression, version) {
        (GridCompression::Precip2Byte { .. }, 7) => {
            return Err(GridWriteError::Unsupported(
                "PRECIP_2_BYTE is a DSS6 compression; heclib's DSS7 grid store refuses it \
                 (zspatialGridStore.c: \"Unsupported Compression\"); use ZLIB"
                    .into(),
            ))
        }
        (GridCompression::Uncompressed, 6) => {
            return Err(GridWriteError::Unsupported(
                "DSS6 grids are stored ZLIB or PRECIP_2_BYTE compressed (Monolith's DSS6 \
                 store has no uncompressed path)"
                    .into(),
            ))
        }
        (GridCompression::Precip2Byte { scale_factor, base }, _) => {
            if scale_factor.is_some() != base.is_some() {
                return Err(bad(
                    "give both the PRECIP_2_BYTE scale factor and base, or neither",
                ));
            }
            if let (Some(s), Some(b)) = (scale_factor, base) {
                if !s.is_finite() || s <= 0.0 || !b.is_finite() {
                    return Err(bad(
                        "the PRECIP_2_BYTE scale factor must be finite and positive and the \
                         base finite",
                    ));
                }
            }
        }
        _ => {}
    }
    let base = input.projection.base_record_type();
    let stats = grid_statistics(input.data, nx, ny, input.projection.statistics_null());
    // P60: HEC's DSS6 grid store writes only 410/420/430. Monolith
    // `GriddedData.storeGriddedData` passes `GridInfo.getGridType()` to `Heclib.zwritx`
    // (offsets 637-742 and 995-1003) and returns -1 for any other type, so a 4x1 type
    // never reaches a DSS6 file (P60 probe: 411/421/431 refused, status -1). A DSS6
    // time-invariant grid is therefore the time-stamped type with empty D and E and
    // undefined times (Monolith's own `/ELEVATION///` grid in `p24_hec_grid6.dss` is 420);
    // specified grids keep `isTimeStamped` 0 in the header.
    // DSS7 stores the type it is given (`zspatialGridStore.c` line 93
    // `ztransfer->dataType = gridStruct->_type`; Monolith stores 411/421/431 only when
    // `GridInfo.setGridType` asks, else 410/420/430). But Monolith cannot read 411 or 421
    // back, its own included: `GridStructContainer.returnGridInfo` (offsets 0-72) builds a
    // GridInfo only for 420, 410, 431 and 430, and the read ends in a NullPointerException
    // (P60 probe). So DSS7 HRAP and Albers time-invariant grids are written as 410/420,
    // equal to Monolith's default record (their header `isTimeStamped` is 0 either way,
    // `GridStructContainer(GridInfo)` offsets 221-336); DSS7 specified grids keep 431.
    let specified = matches!(input.projection, GridProjection::Specified(_));
    let record_type = if input.time_stamped || version == 6 || !specified {
        base
    } else {
        base + 1
    };
    Ok(GridPlan {
        pathname: stored,
        record_type,
        start,
        end,
        stats,
    })
}

// -------------------------------------------------------------------------------------
// ZLIB
// -------------------------------------------------------------------------------------

/// zlib stream of `bytes` (RFC 1950). heclib's DSS7 store uses `Z_DEFAULT_COMPRESSION`
/// (`Public/zlibcompress.c:compress_zlib()` line 29), Monolith's DSS6 store level 9
/// (`DataCompression.jCompress`, `Deflater.setLevel(9)`). The engine uses miniz_oxide at
/// the same levels: the streams differ from zlib's byte for byte, the inflated cells do
/// not.
pub fn zlib(bytes: &[u8], level: u8) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(bytes, level)
}

// -------------------------------------------------------------------------------------
// PRECIP_2_BYTE (DSS6)
// -------------------------------------------------------------------------------------

/// Scale factor and base Monolith chooses when the caller gives neither
/// (`GriddedData.storeGriddedData` offsets 425-528): scale 100, base 0; a minimum below
/// the base lowers the base to `floor(min)`; when `(max - base) * scale` exceeds 32768 the
/// base becomes `floor(min)`, and if it still does the scale becomes
/// `ceil(32767 / (max - min))`.
pub fn precip_auto_scale_base(stats: &GridStatistics) -> (f32, f32) {
    let (min, max) = (stats.min, stats.max);
    let mut scale = 100.0f32;
    let mut base = 0.0f32;
    if min < base {
        base = f64::from(min).floor() as f32;
    }
    let range = ((max - base) * scale) as i32;
    if range > 32_768 {
        base = f64::from(min).floor() as f32;
        let range = ((max - base) * scale) as i32;
        if range > 32_768 {
            scale = (32_767.0 / f64::from(max - min)).ceil() as f32;
        }
    }
    (scale, base)
}

/// `DataCompression.compress` quantization of one cell: -1 is missing.
fn quantize(v: f32, scale: f32, base: f32) -> i32 {
    if v < base {
        return -1;
    }
    // (v - base) * scale in float, then + 0.5 in double, then d2i (truncation, NaN -> 0).
    let q = (f64::from((v - base) * scale) + 0.5) as i32;
    if q > 32_768 {
        -1
    } else {
        i32::from(q as i16)
    }
}

/// `DataCompression.compress` (Monolith 3.3.27 offsets 0-388): quantize, then run-length
/// code zero and missing runs (0x8000 | n and 0xC000 | n, runs split at 16000; a run of
/// exactly 16000·k cells ends with a zero-length run, as HEC writes it).
pub fn precip_2_byte_compress(data: &[f32], scale: f32, base: f32) -> Vec<i16> {
    let q: Vec<i32> = data.iter().map(|&v| quantize(v, scale, base)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < q.len() {
        let code = q[i];
        if code == 0 || code == -1 {
            let flag: u16 = if code == 0 { 0x8000 } else { 0xC000 };
            let mut run = 0usize;
            while i < q.len() && q[i] == code {
                run += 1;
                i += 1;
            }
            if run < 16_000 {
                out.push((flag | run as u16) as i16);
            } else {
                while run >= 16_000 {
                    out.push((flag | 16_000) as i16);
                    run -= 16_000;
                }
                out.push((flag | run as u16) as i16);
            }
        } else {
            out.push(code as i16);
            i += 1;
        }
    }
    out
}

/// Check that PRECIP_2_BYTE keeps every cell's missing state: missing cells (-FLT_MAX,
/// and a specified grid's null value) must code as missing and every other cell as a
/// value. Monolith drops values silently here (for example when its automatic scale
/// leaves the maximum above 32767 steps); the writer refuses instead.
pub fn check_precip_2_byte(
    data: &[f32],
    null: Option<f32>,
    scale: f32,
    base: f32,
) -> Result<(), GridWriteError> {
    for (i, &v) in data.iter().enumerate() {
        let missing = v == -f32::MAX || Some(v) == null;
        if v.is_nan() {
            return Err(bad(format!(
                "cell {i} is NaN; PRECIP_2_BYTE would store it as 0 (use NaN-free data or \
                 ZLIB)"
            )));
        }
        // The cell as the DSS6 store compresses it (-FLT_MAX becomes the null value).
        let stored = if v == -f32::MAX {
            null.unwrap_or(UNDEFINED_STORAGE)
        } else {
            v
        };
        let code = quantize(stored, scale, base);
        if missing && code != -1 {
            return Err(bad(format!(
                "missing cell {i} would be stored as a value by PRECIP_2_BYTE (null value \
                 at or above the base {base})"
            )));
        }
        if !missing && (code == -1 || code < 0) {
            return Err(bad(format!(
                "cell {i} ({v}) is outside the PRECIP_2_BYTE range for scale {scale} and \
                 base {base} (below the base, or more than 32767 steps above it): HEC would \
                 store it as missing; give a scale factor and base that fit, or use ZLIB"
            )));
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------------------
// Java Float.toString (for the Albers WKT Monolith builds for DSS7)
// -------------------------------------------------------------------------------------

/// `Float.toString(f)` of Java: decimal notation for 1e-3 <= |f| < 1e7 with at least one
/// fraction digit, else `d.dddE±n`; the shortest digits that round-trip. (JDK 17's
/// `FloatingDecimal` can print one digit more than the shortest for a few values; the
/// engine prints the shortest, which parses to the same float.)
pub fn java_float_to_string(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    // Shortest round-trip digits from Rust's formatter, as d.ddd e x.
    let sci = format!("{:e}", f.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if f < 0.0 { "-" } else { "" };
    let a = f.abs();
    if (1.0e-3..1.0e7).contains(&a) {
        let s = if exp >= 0 {
            let e = exp as usize;
            let (int, frac) = if digits.len() > e + 1 {
                (digits[..=e].to_string(), digits[e + 1..].to_string())
            } else {
                (
                    format!("{digits}{}", "0".repeat(e + 1 - digits.len())),
                    String::new(),
                )
            };
            format!("{int}.{}", if frac.is_empty() { "0" } else { &frac })
        } else {
            format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
        };
        format!("{sign}{s}")
    } else {
        let frac = if digits.len() > 1 { &digits[1..] } else { "0" };
        format!("{sign}{}.{frac}E{exp}", &digits[..1])
    }
}

/// The SRS definition Monolith stores for an Albers grid in DSS7:
/// `GridStructContainer.getAlbersSpatialReferenceSystem` (offsets 0-247): the SHG WKT when
/// `AlbersInfo.getSpatialReferenceSystem` reports it (NAD83, metres, SHG parameters, origin
/// 0/0; [`crate::grid`] `albers_srs`), else a WKT built from the parameters with Java's
/// `Float.toString`: datum NAD83 / NAD27 / `UNDEFINED`, and the linear unit
/// `UNIT["<units>",1.0]` for `meters` (any case) or `UNIT["<units>",0.0]` otherwise.
pub fn albers_wkt(a: &AlbersParameters, x0: f32, y0: f32) -> String {
    let shg = crate::grid::albers_srs(&crate::grid::AlbersSrsInputs {
        datum: a.datum,
        units_are_meters: crate::grid::is_meters(&a.units),
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
    });
    if !shg.is_empty() {
        return shg;
    }
    let datum = match a.datum {
        2 => "North_American_1983",
        1 => "North_American_1927",
        _ => "UNDEFINED",
    };
    let units = String::from_utf8_lossy(&a.units);
    let unit = if units.eq_ignore_ascii_case("meters") {
        format!("UNIT[\"{units}\",1.0]]")
    } else {
        format!("UNIT[\"{units}\",0.0]]")
    };
    let j = java_float_to_string;
    format!(
        "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"GCS_{datum}\",\
         DATUM[\"D_{datum}\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],\
         PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],\
         PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",{}],\
         PARAMETER[\"False_Northing\",{}],PARAMETER[\"Central_Meridian\",{}],\
         PARAMETER[\"Standard_Parallel_1\",{}],PARAMETER[\"Standard_Parallel_2\",{}],\
         PARAMETER[\"Latitude_Of_Origin\",{}],{unit}",
        j(a.false_easting),
        j(a.false_northing),
        j(a.central_meridian),
        j(a.first_standard_parallel),
        j(a.second_standard_parallel),
        j(a.latitude_of_projection_origin),
    )
}

/// The HRAP WKT (`zStructSpatialGrid.h` `HRAP_SRC_DEFINITION`, HEC's spelling).
pub fn hrap_wkt() -> &'static str {
    crate::grid::HRAP_WKT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_float_strings() {
        for (v, s) in [
            (29.5f32, "29.5"),
            (-96.0, "-96.0"),
            (0.0, "0.0"),
            (1000.0, "1000.0"),
            (-250.0, "-250.0"),
            (0.001, "0.001"),
            (1.0e7, "1.0E7"),
            (1.5e-4, "1.5E-4"),
            (123456.7, "123456.7"),
            (9999999.0, "9999999.0"),
            (0.1, "0.1"),
            (-89.5, "-89.5"),
        ] {
            assert_eq!(java_float_to_string(v), s, "{v}");
        }
    }

    #[test]
    fn grid_times() {
        // HEC julian 0 is 1899-12-31, so 2020-01-01 is day 43830.
        assert_eq!(parse_grid_time(b"01JAN2020:0600"), Some((43_830, 21_600)));
        assert_eq!(parse_grid_time(b"31dec2019:2400"), Some((43_829, 86_400)));
        assert_eq!(parse_grid_time(b"29FEB2019:0000"), None);
        assert_eq!(parse_grid_time(b"01JAN2020:2401"), None);
        assert_eq!(parse_grid_time(b"01JAN2020 0600"), None);
        assert_eq!(parse_grid_time(b"1JAN2020:0600"), None);
    }

    #[test]
    fn statistics_follow_update_statistics() {
        let nan = f32::NAN;
        let d = [1.0, -f32::MAX, 3.0, UNDEFINED_STORAGE, 0.0, 6.0];
        let s = grid_statistics(&d, 3, 2, UNDEFINED_STORAGE);
        assert_eq!((s.min, s.max), (0.0, 6.0));
        assert_eq!(s.mean, 10.0 / 4.0);
        assert_eq!(s.range_counts[0], 6); // every cell is >= -FLT_MAX
        assert_eq!(s.range_counts[1], 4);
        let s = grid_statistics(&[nan, 2.0], 2, 1, UNDEFINED_STORAGE);
        assert!(s.mean.is_nan());
        assert_eq!((s.min, s.max), (2.0, 2.0));
        assert_eq!(s.range_counts[0], 1);
        let s = grid_statistics(&[-f32::MAX; 4], 2, 2, UNDEFINED_STORAGE);
        assert_eq!((s.min, s.max, s.mean), (2.0e38, -2.0e38, -f32::MAX));
    }

    #[test]
    fn precip_runs_and_quantization() {
        let mut d = vec![0.0f32; 16_001];
        d[0] = 2.0;
        let c = precip_2_byte_compress(&d, 100.0, 0.0);
        assert_eq!(c, vec![200, (0x8000u16 | 16_000) as i16, 0x8000u16 as i16]);
        let c = precip_2_byte_compress(&[-1.0e30, -1.0e30, 0.994], 100.0, 0.0);
        assert_eq!(c, vec![(0xC000u16 | 2) as i16, 99]);
        // Monolith's automatic scale can leave the maximum out of range: refused.
        let d = [0.0f32, 870.71];
        let st = grid_statistics(&d, 2, 1, UNDEFINED_STORAGE);
        let (s, b) = precip_auto_scale_base(&st);
        assert_eq!((s, b), (38.0, 0.0));
        assert!(check_precip_2_byte(&d, None, s, b).is_err());
    }
}

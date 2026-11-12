//! Frames to DSS grid records, through the P24 writer and nothing else.
//!
//! Sources, Vortex 1.1.0: `io/DssDataWriter.write`, `updatePathname`, `util/DssUtil.getGridInfo`.
//! Monolith: `hec.heclib.grid.GriddedData.setGriddedTimeWindow` and `setGridTime`, whose results
//! were observed by calling them through HEC Monolith. Lock, atomicity and crash behaviour
//! are the writer's: `dss_core::dss7::write::write_grid` and `dss6::write::write_grid6`.
//!
//! Time parts: a period's D part is its start, with midnight as `0000`, and its E part is its end,
//! with midnight as `2400` of the previous day. An INST-VAL record has only a D part, which is the
//! end, also with midnight as `2400` of the previous day.
//!
//! Observed behaviour we did not implement, each on Vortex 1.1.0:
//!
//! - **Origin rounding.** `getGridInfo` stores `round(origin / cell)`, so an origin 1.25 cells from
//!   zero moves a quarter of a cell with no message. This sink refuses an origin not on whole cells
//!   unless [`Align::Vortex`] is stated.
//! - **Unequal spacing.** `getGridInfo` stores the mean of dx and dy as one cell size. This sink
//!   refuses unequal spacing unless [`Align::Vortex`] is stated.
//! - **Seconds in a window.** The pathname keeps minutes, so 00:00:30 to 01:00:30 is stored as 0000
//!   to 0100. This sink refuses a time that is not a whole minute.
//! - **`partD`, `partE`.** Vortex accepts them and the time window replaces them. This API has no
//!   such arguments.
//! - **DSS6 precipitation precision.** Monolith 3.3.28, through Vortex 0.13.3, stores most PER-CUM grids
//!   in DSS6 as PRECIP_2_BYTE, which rounds each cell to 1/100 mm. This sink stores ZLIB, which keeps
//!   the warp's bits; the P24 writer chooses PRECIP_2_BYTE only when asked. The comparison reproduces
//!   Monolith's rounding from these cells, so each differing cell is explained.

use crate::crs::{CrsDef, Projection};
use crate::error::VortexError;
use crate::frame::{Crs, DataType, FrameTime, GeoTransform, GridFrameParts, NamedCrs};
use crate::units::{CPartSource, Conversion, RecordMeaning, Variable};
use crate::warp::Align;
use dss_core::dss7::write::{AlbersParameters, GridProjection, SpecifiedReference};
use dss_core::dss7::write::{FaultPoint, SyncMode, WriteError, WriteOptions};
use dss_core::grid_write::{self, GridCompression, GridInput, GRID_NODATA};
use std::path::Path;

/// `GridInfo` subclass a frame becomes, from `DssUtil.getGridInfo`.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkCrs {
    Albers(AlbersParameters),
    Specified { name: String, definition: String },
}

/// The name of a WKT root node, and of the `GEOGCS` it holds. Only names, so a text search will do;
/// the CRS itself has been parsed by then.
fn wkt_names(text: &str) -> (String, String, bool) {
    let quoted = |after: &str| -> String {
        after
            .split_once('"')
            .and_then(|(_, rest)| rest.split_once('"'))
            .map_or(String::new(), |(name, _)| name.to_string())
    };
    let trimmed = text.trim_start();
    let projected =
        trimmed.len() >= 6 && trimmed[..6.min(trimmed.len())].eq_ignore_ascii_case("PROJCS");
    let root = quoted(trimmed);
    let geogcs = text
        .find("GEOGCS")
        .map_or(String::new(), |at| quoted(&text[at..]));
    (root, geogcs, projected)
}

// `SpatialReference.GetLinearUnitsName`: the projected CRS's own UNIT, the last one in WKT1.
fn linear_units_name(text: &str) -> String {
    text.rfind("UNIT[")
        .and_then(|at| {
            let rest = &text[at..];
            let (_, rest) = rest.split_once('"')?;
            rest.split_once('"').map(|(name, _)| name.to_string())
        })
        // GDAL's ESRI import names this unit `metre` (`AAIGDataset::CommonOpen` calls `importFromESRI`,
        // and `Resampler` calls `MorphFromESRI`): p41_v1_1_0.prj says `Meter`, its DSS6 record `metre`.
        .map(|name| {
            if name == "Meter" {
                "metre".into()
            } else {
                name
            }
        })
        .unwrap_or_default()
}

impl SinkCrs {
    /// `getGridInfo`: a CRS whose name holds `albers` is an Albers record, with the NAD83 datum if
    /// its `GEOGCS` name holds `83`. Anything else is a specified grid holding the text.
    pub fn of_frame(crs: &Crs) -> Result<SinkCrs, VortexError> {
        match crs {
            Crs::Named(NamedCrs::Shg) => Ok(SinkCrs::Albers(shg_parameters())),
            Crs::Named(NamedCrs::Hrap) => SinkCrs::of_frame(&Crs::Wkt(CrsDef::hrap().to_wkt())),
            Crs::Wkt(text) => {
                let def = CrsDef::parse_wkt(text)?;
                let (root, geogcs, _) = wkt_names(text);
                if root.to_lowercase().contains("albers") {
                    let Projection::Albers {
                        lat_1,
                        lat_2,
                        lat_0,
                        lon_0,
                        x_0,
                        y_0,
                    } = def.projection
                    else {
                        return Err(VortexError::Refused {
                            what: format!("CRS {root}"),
                            reason: "its name says Albers and its projection is not".into(),
                        });
                    };
                    Ok(SinkCrs::Albers(AlbersParameters {
                        datum: if geogcs.contains("83") { 2 } else { 0 },
                        units: linear_units_name(text).into_bytes(),
                        first_standard_parallel: lat_1 as f32,
                        second_standard_parallel: lat_2 as f32,
                        central_meridian: lon_0 as f32,
                        latitude_of_projection_origin: lat_0 as f32,
                        false_easting: x_0 as f32,
                        false_northing: y_0 as f32,
                    }))
                } else {
                    Ok(SinkCrs::Specified {
                        name: root,
                        definition: text.clone(),
                    })
                }
            }
            Crs::Proj(_) => Err(VortexError::Unsupported {
                what: "PROJ string as a DSS grid's CRS".into(),
            }),
            Crs::Undefined => Err(VortexError::Refused {
                what: "DSS grid".into(),
                reason: "the frame states no CRS".into(),
            }),
        }
    }

    fn projection(&self) -> GridProjection {
        match self {
            SinkCrs::Albers(a) => GridProjection::Albers(a.clone()),
            SinkCrs::Specified { name, definition } => {
                GridProjection::Specified(SpecifiedReference {
                    srs_name: name.clone().into_bytes(),
                    srs_definition: definition.clone().into_bytes(),
                    srs_definition_type: 0,
                    null_value: GRID_NODATA,
                    time_zone_id: b"UTC".to_vec(),
                    time_zone_raw_offset: 0,
                    // `hec.heclib.grid.SpecifiedGridInfo`'s constructor sets `_isInterval` true, and
                    // neither `DssUtil.getGridInfo` nor `setGridTimes` changes it, whatever the type.
                    is_interval: true,
                })
            }
        }
    }
}

// `WktFactory.getShg`, as `getGridInfo` reads it. Units: `GetLinearUnitsName` on that WKT. Only DSS6
// stores them; Vortex 0.13.3's DSS6 records hold `metre`.
fn shg_parameters() -> AlbersParameters {
    AlbersParameters {
        units: b"metre".to_vec(),
        ..AlbersParameters::shg()
    }
}

/// The pathname parts a caller may state. D and E always come from the frame's window.
#[derive(Debug, Clone, Default)]
pub struct Parts {
    pub a: String,
    pub b: String,
    pub f: String,
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

fn refuse_time(reason: String) -> VortexError {
    VortexError::Refused {
        what: "time window".into(),
        reason,
    }
}

fn stamp((julian, seconds): (i64, i64)) -> Result<String, VortexError> {
    if seconds % 60 != 0 {
        return Err(refuse_time(format!(
            "{seconds} s past midnight is not a whole minute, and a pathname stores minutes. \
             Observed behaviour we did not implement: Vortex drops the seconds"
        )));
    }
    let (y, m, d) = dss_core::time::julian_to_ymd(julian);
    if !(1..=9999).contains(&y) {
        return Err(refuse_time(format!("year {y} does not fit a pathname")));
    }
    let hhmm = seconds / 60;
    Ok(format!(
        "{d:02}{}{y:04}:{:02}{:02}",
        MONTHS[(m - 1) as usize],
        hhmm / 60,
        hhmm % 60
    ))
}

// `HecTime.showTimeAsBeginningOfDay(true)`: 86400 s is 0000 of the next day.
fn start_stamp((julian, seconds): (i64, i64)) -> Result<String, VortexError> {
    if seconds == 86_400 {
        return stamp((julian + 1, 0));
    }
    stamp((julian, seconds))
}

// `HecTime.showTimeAsBeginningOfDay(false)`: midnight is 2400 of the previous day.
fn end_stamp((julian, seconds): (i64, i64)) -> Result<String, VortexError> {
    if seconds == 0 {
        return stamp((julian - 1, 86_400));
    }
    stamp((julian, seconds))
}

/// The D and E parts for a window and data type (`DssDataWriter.write`, its private `write`).
pub fn time_parts(time: &FrameTime, data_type: DataType) -> Result<(String, String), VortexError> {
    let (start, end) = match *time {
        FrameTime::Invariant => return Ok((String::new(), String::new())),
        FrameTime::Instant(t) => (t, t),
        FrameTime::Period { start, end } => (start, end),
    };
    if data_type == DataType::InstVal {
        return Ok((end_stamp(end)?, String::new()));
    }
    Ok((start_stamp(start)?, end_stamp(end)?))
}

/// One record a run will write, decided before any cell is read.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedRecord {
    pub pathname: String,
    pub meaning: RecordMeaning,
}

/// Build a record's pathname (`updatePathname`). `description` gives the F part for
/// `PRECIPITATION-FREQUENCY` when none is stated (`DssDataWriter.write`).
pub fn plan_record(
    parts: &Parts,
    meaning: RecordMeaning,
    derived_c_part: &str,
    description: Option<&str>,
    time: &FrameTime,
) -> Result<PlannedRecord, VortexError> {
    let (d, e) = time_parts(time, meaning.data_type)?;
    let f = if derived_c_part == Variable::PrecipitationFrequency.dss_c_part() && parts.f.is_empty()
    {
        description.unwrap_or_default()
    } else {
        parts.f.as_str()
    };
    let pathname = format!("/{}/{}/{}/{d}/{e}/{f}/", parts.a, parts.b, meaning.c_part);
    Ok(PlannedRecord { pathname, meaning })
}

/// `Math.round`: floor of `x + 1/2`, without the rounding that sum could add.
fn java_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// Cell size and lower-left cell, from `getGridInfo`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub cell_size: f32,
    pub lower_left_x: i32,
    pub lower_left_y: i32,
}

/// Where a grid goes, or a refusal under [`Align::Refuse`]. Georeference only.
pub fn placement(
    transform: &GeoTransform,
    ny: usize,
    align: Align,
) -> Result<Placement, VortexError> {
    let refuse = |reason: String| VortexError::Refused {
        what: "grid placement".into(),
        reason,
    };
    let t = transform;
    if t.row_rotation != 0.0 || t.column_rotation != 0.0 {
        return Err(refuse("a rotated grid has no DSS form".into()));
    }
    // DSS stores columns west to east from the lower-left cell, and this path does not reorder them.
    // Taking the spacing's size with the source's corner would move and mirror the grid.
    if t.dx.is_nan() || t.dx <= 0.0 || t.dy == 0.0 || t.dy.is_nan() {
        return Err(refuse(format!(
            "dx {} and dy {} cannot be stored as they are: columns must run west to east. State a \
             cell size or envelope to warp the grid, which places it correctly. Observed behaviour \
             we did not implement: Vortex 1.1.0 and 0.13.3 fail on negative dx with negative dy, \
             and their reader reorders negative dx with positive dy",
            t.dx, t.dy
        )));
    }
    let lly = if t.dy < 0.0 {
        t.origin_y + t.dy * ny as f64
    } else {
        t.origin_y
    };
    let cell = ((t.dx.abs() + t.dy.abs()) / 2.0) as f32;
    if !cell.is_finite() || cell <= 0.0 {
        return Err(refuse(format!(
            "cell size {cell} is not positive and finite"
        )));
    }
    let (rx, ry) = (t.origin_x / f64::from(cell), lly / f64::from(cell));
    let (x, y) = (java_round(rx), java_round(ry));
    if align == Align::Refuse {
        if t.dx.abs() as f32 != t.dy.abs() as f32 {
            return Err(refuse(format!(
                "dx {} and dy {} differ. Observed behaviour we did not implement: Vortex stores \
                 their mean as one cell size. Ask for align=\"vortex\"",
                t.dx, t.dy
            )));
        }
        // The same bound as the Python writer's `origin=` check (`_write_grid.py`).
        if (rx - x).abs() > 1e-6 || (ry - y).abs() > 1e-6 {
            return Err(refuse(format!(
                "the lower-left corner ({}, {lly}) is not on whole cells of {cell}. Observed \
                 behaviour we did not implement: Vortex rounds it to whole cells, moving the grid. \
                 State an envelope and a cell size, or ask for align=\"vortex\"",
                t.origin_x
            )));
        }
    }
    let index = |v: f64| {
        if v >= f64::from(i32::MIN) && v <= f64::from(i32::MAX) {
            Ok(v as i32)
        } else {
            Err(refuse(format!("cell index {v} does not fit a DSS grid")))
        }
    };
    Ok(Placement {
        cell_size: cell,
        lower_left_x: index(x)?,
        lower_left_y: index(y)?,
    })
}

/// Whether a valid result cannot be stored as one, because a reader would not give it back: HEC's
/// marker (`heclib_c/src/headers/zdssVals.h`, `UNDEFINED_FLOAT`), or -901 and -902, which this
/// library's reader also takes as missing (`dss_core::grid::Grid::is_missing`).
pub fn reader_takes_as_missing(value: f32) -> bool {
    value == GRID_NODATA || value == -901.0 || value == -902.0
}

/// Convert units, flip to DSS's south-up order and turn every missing cell to HEC's marker, in place.
///
/// A cell is missing if, **before conversion**, it equals the frame's marker or is NaN or infinite
/// (`DssDataWriter.write`, `Double.compare` on the marker's bits). So a valid cell converted to a
/// value equal to the source's marker stays valid, and the record's statistics count it.
///
/// A valid cell that cannot be stored is refused, not lost: one converted out of `f32`, or one for
/// which [`reader_takes_as_missing`] holds. If the source means missing by that value, it can state
/// it as its nodata. Every writer of grids in this crate comes through here: the importer, its unit
/// conversions, and each grid tool.
pub fn to_stored_cells(
    parts: &mut GridFrameParts,
    conversion: Conversion,
) -> Result<(), VortexError> {
    let nodata = parts.nodata.unwrap_or(f32::NAN);
    for value in parts.cells.iter_mut() {
        if value.is_nan() || value.is_infinite() || value.to_bits() == nodata.to_bits() {
            *value = GRID_NODATA;
            continue;
        }
        let source = *value;
        let converted = conversion.convert(source);
        if !converted.is_finite() || reader_takes_as_missing(converted) {
            return Err(VortexError::Refused {
                what: "cell value".into(),
                reason: format!(
                    "the valid cell {source} becomes {converted}, which a DSS grid cannot hold as a \
                     value: readers would take as missing what was written as a value. If it means \
                     missing, state it as the source's nodata"
                ),
            });
        }
        *value = converted;
    }
    if parts.transform.dy < 0.0 {
        flip_rows(&mut parts.cells, parts.nx, parts.ny);
    }
    Ok(())
}

fn flip_rows(cells: &mut [f32], nx: usize, ny: usize) {
    for row in 0..ny / 2 {
        let (top, bottom) = cells.split_at_mut((ny - 1 - row) * nx);
        top[row * nx..(row + 1) * nx].swap_with_slice(&mut bottom[..nx]);
    }
}

/// How to write.
#[derive(Debug, Clone)]
pub struct SinkOptions {
    /// The file's DSS version, 6 or 7.
    pub version: u8,
    /// The flush policy of this one record. A run that stores several records passes
    /// [`record_sync`] of its own policy and ends with [`end_sync`].
    pub sync: SyncMode,
    pub allow_undetectable_readers: bool,
    /// Crash simulation, for tests.
    pub fault: Option<FaultPoint>,
}

/// The flush policy each record of a multi-record run gets: [`SyncMode::End`] means one
/// flush at the end of the run ([`end_sync`]), so its records are written without one.
pub fn record_sync(run: SyncMode) -> SyncMode {
    run.batched().0
}

/// The end of a multi-record run: in [`SyncMode::End`], flush `output` once
/// (one flush at the end of each public write call). It runs after a failed run too, since
/// the records acknowledged before the failure stay in the file. A flush failure after a
/// successful run is reported through `failed`, which receives what the run returned.
pub fn end_sync<T, E>(
    run: SyncMode,
    output: &Path,
    result: Result<T, E>,
    failed: impl FnOnce(WriteError, T) -> E,
) -> Result<T, E> {
    if !run.batched().1 {
        return result;
    }
    let flushed = dss_core::dss7::write::sync_path(output);
    match (result, flushed) {
        (Ok(done), Err(e)) => Err(failed(e.into(), done)),
        (result, _) => result,
    }
}

/// What one write did.
#[derive(Debug, Clone, PartialEq)]
pub struct WrittenRecord {
    /// As stored; DSS6 is upper case.
    pub pathname: String,
    pub record_type: i32,
    /// `added`, `replaced` or `unchanged`.
    pub outcome: String,
    pub data_type: DataType,
    pub units: String,
    pub c_part_source: CPartSource,
    pub nx: usize,
    pub ny: usize,
    pub lower_left_x: i32,
    pub lower_left_y: i32,
    pub cell_size: f32,
    pub readers_undetected: bool,
}

fn input<'a>(
    planned: &'a PlannedRecord,
    crs: &SinkCrs,
    place: Placement,
    cells: &'a [f32],
    nx: usize,
    ny: usize,
) -> GridInput<'a> {
    GridInput {
        pathname: planned.pathname.as_bytes(),
        projection: crs.projection(),
        // HEC parameter grids keep the time-stamped types with empty D and E (`getGridInfo`).
        time_stamped: true,
        data: cells,
        nx,
        ny,
        lower_left_cell_x: place.lower_left_x,
        lower_left_cell_y: place.lower_left_y,
        cell_size: place.cell_size,
        x_coord_of_grid_cell_zero: 0.0,
        y_coord_of_grid_cell_zero: 0.0,
        units: planned.meaning.units.as_bytes(),
        data_type: planned.meaning.data_type.dss_code(),
        // As Monolith writes DSS7; DSS6 was checked against Monolith too.
        compression: GridCompression::Zlib,
    }
}

/// Run the writer's checks on a planned record, with one stand-in cell, before anything is written.
/// Returns the pathname as it will be stored.
pub fn check_record(
    planned: &PlannedRecord,
    crs: &SinkCrs,
    version: u8,
) -> Result<Vec<u8>, WriteError> {
    let place = Placement {
        cell_size: 1.0,
        lower_left_x: 0,
        lower_left_y: 0,
    };
    let cell = [0.0f32];
    let i = input(planned, crs, place, &cell, 1, 1);
    Ok(grid_write::plan_grid(&i, version)?.pathname)
}

/// Write one frame's stored cells. The writer refuses a locked file and writes data before
/// pointers, so a failed write leaves the previous record and every other record intact.
#[allow(clippy::too_many_arguments)]
pub fn write_record(
    path: &Path,
    planned: &PlannedRecord,
    crs: &SinkCrs,
    place: Placement,
    cells: &[f32],
    nx: usize,
    ny: usize,
    options: &SinkOptions,
) -> Result<WrittenRecord, WriteError> {
    let i = input(planned, crs, place, cells, nx, ny);
    let mut write = WriteOptions {
        sync: options.sync,
        fault: options.fault,
        ..WriteOptions::default()
    };
    write.lock_policy.allow_undetectable_readers = options.allow_undetectable_readers;
    let (plan, report) = if options.version == 6 {
        (
            dss_core::dss6::write::validate_grid6_input(&i)?,
            dss_core::dss6::write::write_grid6(path, &i, &write)?,
        )
    } else {
        (
            dss_core::dss7::write::validate_grid_input(&i)?,
            dss_core::dss7::write::write_grid(path, &i, &write)?,
        )
    };
    Ok(WrittenRecord {
        pathname: String::from_utf8_lossy(&plan.pathname).into_owned(),
        record_type: plan.record_type,
        outcome: report
            .blocks
            .first()
            .map_or("unchanged", |b| b.outcome.as_str())
            .to_string(),
        data_type: planned.meaning.data_type,
        units: planned.meaning.units.clone(),
        c_part_source: planned.meaning.c_part_source,
        nx,
        ny,
        lower_left_x: place.lower_left_x,
        lower_left_y: place.lower_left_y,
        cell_size: place.cell_size,
        readers_undetected: report.readers_undetected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn julian(y: i64, m: u32, d: u32) -> i64 {
        dss_core::time::ymd_to_julian(y, m, d)
    }

    #[test]
    fn midnight_is_0000_at_a_start_and_2400_at_an_end() {
        let june15 = julian(2024, 6, 15);
        let hour = FrameTime::Period {
            start: (june15, 0),
            end: (june15, 3600),
        };
        let (d, e) = time_parts(&hour, DataType::PerCum).unwrap();
        assert_eq!(
            (d.as_str(), e.as_str()),
            ("15JUN2024:0000", "15JUN2024:0100")
        );
        let last = FrameTime::Period {
            start: (june15 - 1, 23 * 3600),
            end: (june15, 0),
        };
        let (d, e) = time_parts(&last, DataType::PerCum).unwrap();
        assert_eq!(
            (d.as_str(), e.as_str()),
            ("14JUN2024:2300", "14JUN2024:2400")
        );
        let at_midnight = FrameTime::Instant((june15, 0));
        let (d, e) = time_parts(&at_midnight, DataType::InstVal).unwrap();
        assert_eq!((d.as_str(), e.as_str()), ("14JUN2024:2400", ""));
    }

    #[test]
    fn seconds_in_a_window_are_refused() {
        let t = FrameTime::Instant((45000, 30));
        assert!(matches!(
            time_parts(&t, DataType::InstVal),
            Err(VortexError::Refused { .. })
        ));
    }

    #[test]
    fn origin_rounding_is_refused_unless_stated() {
        let t = GeoTransform::axis_aligned(2500.0, 2000.0, 6700.0, -2000.0);
        assert!(placement(&t, 2, Align::Refuse).is_err());
        let p = placement(&t, 2, Align::Vortex).unwrap();
        assert_eq!((p.lower_left_x, p.lower_left_y), (1, 1));
    }

    #[test]
    fn unequal_spacing_is_refused_unless_stated() {
        let t = GeoTransform::axis_aligned(2000.0, 2000.0, 6000.0, -3000.0);
        assert!(placement(&t, 2, Align::Refuse).is_err());
        let p = placement(&t, 2, Align::Vortex).unwrap();
        assert_eq!(
            (p.cell_size, p.lower_left_x, p.lower_left_y),
            (2500.0, 1, 0)
        );
    }

    fn stored(cells: Vec<f32>, conversion: Conversion) -> Result<Vec<f32>, VortexError> {
        let mut parts = GridFrameParts {
            nx: cells.len(),
            ny: 1,
            cells,
            row_order: crate::frame::RowOrder::NorthToSouth,
            transform: GeoTransform::axis_aligned(0.0, 1.0, 1.0, -1.0),
            crs: Crs::Named(NamedCrs::Shg),
            nodata: Some(-9999.0),
            data_type: DataType::InstVal,
            time: FrameTime::Invariant,
            units: "MM".into(),
            variable: "v".into(),
            description: None,
        };
        to_stored_cells(&mut parts, conversion)?;
        Ok(parts.cells)
    }

    // The importer's route: a value the reader would lose is refused unchanged, and when a unit
    // conversion lands on it. Both failed before this rule named -901 and -902.
    #[test]
    fn a_value_the_reader_would_lose_is_refused_with_or_without_a_conversion() {
        for value in [-901.0f32, -902.0, GRID_NODATA] {
            let e = stored(vec![1.0, value], Conversion::None).unwrap_err();
            assert!(e.to_string().contains("take as missing"), "{e}");
        }
        // Kelvin: `value + -273.15f32`, as `Conversion::convert` adds it.
        for target in [-901.0f32, -902.0] {
            let kelvin = target - -273.15f32;
            assert_eq!(kelvin + -273.15f32, target, "the case must land exactly");
            let e = stored(vec![kelvin], Conversion::KelvinToCelsius).unwrap_err();
            assert!(e.to_string().contains("take as missing"), "{e}");
        }
        // Next to them, and the source's own marker: stored as they are.
        let cells = stored(vec![-900.0, -903.0, -9999.0], Conversion::None).unwrap();
        assert_eq!(cells, [-900.0, -903.0, GRID_NODATA]);
    }

    #[test]
    fn flip_keeps_every_row() {
        let mut cells = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        flip_rows(&mut cells, 2, 3);
        assert_eq!(cells, [5.0, 6.0, 3.0, 4.0, 1.0, 2.0]);
    }

    #[test]
    fn a_specified_grid_says_interval_whatever_its_type() {
        let c = SinkCrs::Specified {
            name: "n".into(),
            definition: "d".into(),
        };
        // `SpecifiedGridInfo`'s constructor. Vortex's records hold true on INST-VAL too.
        let GridProjection::Specified(s) = c.projection() else {
            panic!("not specified");
        };
        assert!(s.is_interval);
    }

    #[test]
    fn gdal_names_an_esri_metre_metre() {
        assert_eq!(
            linear_units_name("UNIT[\"a\",1],UNIT[\"Meter\",1.0]]"),
            "metre"
        );
        assert_eq!(linear_units_name("UNIT[\"Foot_US\",0.3]]"), "Foot_US");
    }

    #[test]
    fn crs_names_decide_the_record_type() {
        let geographic = CrsDef::sphere_lonlat(6371229.0).to_wkt();
        match SinkCrs::of_frame(&Crs::Wkt(geographic.clone())).unwrap() {
            SinkCrs::Specified { definition, .. } => assert_eq!(definition, geographic),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            SinkCrs::of_frame(&Crs::Named(NamedCrs::Shg)).unwrap(),
            SinkCrs::Albers(_)
        ));
    }
}

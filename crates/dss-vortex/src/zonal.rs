//! Grid to point: zonal statistics over polygon zones, written as DSS time series.
//!
//! Replaces HEC-Vortex's grid-to-point tool. Sources, Vortex 1.1.0 (0.13.3's zonal code is
//! byte-identical): `convert/GridToPointConverter`, `geo/ZonalStatisticsCalculator`, `geo/Grid`,
//! `geo/GridCell`, `io/DssDataWriter` and its `VortexPoints`, `io/DssDataReader.dssToDto`,
//! `util/DssUtil.getEPart`. GDAL 3.2.1 for the mask (see [`rasterize`]). No Java or GDAL runs here;
//! they are used only as test oracles.
//!
//! A run reads every frame and computes every statistic before it touches the output, so anything
//! refused leaves the file as it was. Each record then goes through the P20, P21 or P22 writer, which
//! holds HEC's lock and leaves no torn block.
//!
//! **A failure while writing proves nothing about what the file holds.** [`GridToPointError::completed`]
//! lists the records the writer acknowledged. [`GridToPointError::in_doubt`] names the series being
//! written when it failed: it may hold none, some or all of its new blocks, and a reader may already
//! see them. So a failure does not mean an existing file or series is unchanged: a replacement can
//! leave a changed series readable with nothing in `completed`. Read the file, or run again.
//!
//! **A rerun updates a window, and does not replace a series.** A record's blocks are merged with what
//! the pathname already holds: values at stamps this run does not reach stay. [`PointRecord::outcome`]
//! says what happened. Both Vortex versions behave the same way on a rerun (observed).
//!
//! **An output that is also an input is refused,** by the file itself rather than its name, so a hard
//! link is caught ([`crate::file_identity`]). [`GridToPointRequest::allow_in_place`] permits the grids'
//! own file: its grid records keep their values, and the file's bytes change. The check does not close
//! the race with a process that replaces a file between the check and the write.
//!
//! **Statistics.** Sorted ascending in Java's total order, summed with the JDK's compensated
//! `DoubleStream.sum`, in doubles. A zone with no valid cell gets the missing marker in every
//! statistic, never 0. As in Vortex, a median needs 2 valid cells, and the quartiles and `-PCT>Q1`
//! need 4, and are missing below that. Q1 is element `(n+1)/4` and Q3 element `3(n+1)/4` of the
//! sorted values, with integer division. Records store float32, as HEC's writer does from Vortex's
//! doubles.
//!
//! **Observed behaviour we did not implement.** [`Compat::Vortex`] restores the ones marked (*). Each
//! was run on Vortex 1.1.0 and 0.13.3 unless it says "from its source".
//!
//! - **Other missing markers (*).** Vortex excludes only cells exactly equal to its grid's marker,
//!   `-FLT_MAX` for DSS, so a NaN, -901 or -902 enters the statistics as a number. This library
//!   excludes what its grid reader calls missing, and a specified grid's null value too.
//! - **A missing statistic from a source that is not DSS (*).** Vortex stores the source's marker as
//!   the value: NaN on a Stage IV file. This library stores `-FLT_MAX`.
//! - **A period that is not the spacing (*).** With uniform starts, Vortex stamps a PER-CUM or PER-AVER
//!   record one spacing after each start, whatever the frame's period. A 1-hour total every 3 hours
//!   becomes a `3Hour` record. This library refuses a series whose period differs from its spacing.
//! - **Intervals with no DSS name.** `getEPart` gives `0` for a spacing it does not list, and labels
//!   5760 minutes `3Days`. HEC's writer refuses those, and Vortex finishes with no records, having
//!   only logged it. This library refuses the run. `4Days`, `6Days`, `1Month` for 30 days and `1Year`
//!   for 365 are refused the same way; those were not run.
//! - **A zone with an empty name.** Vortex writes a record with an empty B part. Refused.
//! - **A feature with no geometry.** Vortex fails on the whole run. Here that zone has no cells, and
//!   its statistics are missing.
//! - **No field by that name, or no `.prj`.** Vortex fails with a null reference. Refused, naming what
//!   is wrong.
//! - **Ends out of step with starts (*), from its source.** Vortex sorts starts and ends apart, so with
//!   nested periods a value can be stamped with another frame's end. Refused.
//! - **Percent overflow (*), from its source.** `100 * count` is a Java `int`, so over 21,474,836 cells
//!   it wraps.
//! - **A grid shifted between frames, from its source.** `compareSpatiallyEquivalent` ignores the
//!   origin, so Vortex keeps the old masks. This library rebuilds them.
//! - **A pathname written twice, from its source.** Two zones or variables reaching one pathname
//!   overwrite each other there. Refused.
//! - **A reprojection that fails on a point, from OGR's source.** OGR leaves the geometry untransformed
//!   and Vortex goes on. Refused.
//! - **A statistic a reader takes as missing (*).** A valid statistic or accumulated total whose final
//!   float32 is -901, -902 or `-FLT_MAX` is stored by Vortex as it stands, and readers then lose it.
//!   This library refuses the run, naming the zone, statistic and time, because the stored value does
//!   not mean what was computed. Under [`Compat::Vortex`] it is stored, and counted in
//!   [`GridToPointOutcome::reserved_values`].
//!
//! The optional area-weighted mean ([`Statistic::AreaWeightedMean`]) is not Vortex's and is never
//! the default: it weighs cells by the share of their area inside the zone, while every other
//! statistic takes a cell whole if its centre is inside.

#![allow(clippy::result_large_err)]

mod geojson;
mod geometry;
mod rasterize;
mod shapefile;

pub use geometry::{Part, Pt};

use crate::crs::{CrsDef, DatumPolicy, Transformer};
use crate::error::VortexError;
use crate::file_identity;
use crate::frame::{Crs, FrameTime};
use crate::import::{self, in_metres, scale_false_origin, GridSource, OpenOptions};
use crate::sink_dss::reader_takes_as_missing;
use crate::units::{variable_from_name, Variable};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    self as write7, FaultPoint, ItsInput, RtsInput, RtsValues, WriteError, WriteOptions,
};
use dss_core::dss7::Dss7File;
use dss_core::grid_write::parse_grid_time;
use dss_core::lock::{self, LockPolicy};
use dss_core::{DssVersion, FileSignature, Grid, GridReadOptions, GridType, GRID_UNDEFINED};
use geometry::{try_vec, Envelope};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub(crate) fn refused(what: &str, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.to_string(),
        reason: reason.into(),
    }
}

/// HEC's missing marker in a record (`zisMissingFloat`), and Vortex's `Heclib.UNDEFINED_FLOAT`.
pub const MISSING: f64 = -3.4028234663852886e38;

/// Bounds on a run. Each is checked before the allocation it guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneLimits {
    /// Largest zone file, held in memory while it is parsed.
    pub max_file_bytes: u64,
    /// Most points in one zone file. 2^27 is 2 GiB of coordinates.
    pub max_points: usize,
    /// Most features in one zone file.
    pub max_features: usize,
    /// Most (cell, zone) pairs held in masks at once. 2^30 is 4 GiB of indices.
    pub max_mask_entries: u64,
    /// Most statistic rows, frames times zones. 2^27 is 8 GiB of statistics.
    pub max_rows: u64,
    /// Rasterizing costs about points times rows. 2^38 is several minutes of a small hostile file's
    /// worth; a county set on the largest sampled grid needs about 2^30.
    pub max_scan_work: u64,
    /// Most cells in one grid.
    pub max_cells: usize,
}

impl Default for ZoneLimits {
    fn default() -> Self {
        ZoneLimits {
            max_file_bytes: 1 << 31,
            max_points: 1 << 27,
            max_features: 1 << 20,
            max_mask_entries: 1 << 30,
            max_rows: 1 << 27,
            max_scan_work: 1 << 38,
            max_cells: 1 << 26,
        }
    }
}

/// One feature as read: what `GetFieldAsString` gave (`None` for a null field), and its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneRecord {
    pub id: Option<String>,
    pub parts: Vec<Part>,
}

/// One zone: every feature sharing a name, in file order, in the zone file's own CRS.
#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    pub id: String,
    pub parts: Vec<Part>,
}

/// A zone file, read and grouped by name.
#[derive(Debug, Clone)]
pub struct ZoneSet {
    pub zones: Vec<Zone>,
    pub crs: CrsDef,
}

impl ZoneSet {
    /// Read a shapefile or GeoJSON file by its content. `crs` states a CRS for a file that does not:
    /// a shapefile with no `.prj`, or a GeoJSON `crs` member this library does not name. It is
    /// refused where the file already states one. A CRS is never guessed.
    pub fn read(
        path: &Path,
        field: &str,
        crs: Option<&Crs>,
        limits: &ZoneLimits,
    ) -> Result<ZoneSet, VortexError> {
        let what = format!("zones {}", path.display());
        if field.is_empty() {
            return Err(refused(&what, "no field was named"));
        }
        let mut head = [0u8; 4];
        {
            use std::io::Read;
            let mut f = std::fs::File::open(path)?;
            let mut n = 0;
            while n < 4 {
                let k = f.read(&mut head[n..])?;
                if k == 0 {
                    break;
                }
                n += k;
            }
        }
        let stated_by_caller = crs
            .map(|c| match c {
                Crs::Wkt(text) => CrsDef::parse_zone_wkt(text),
                named => CrsDef::from_frame(named),
            })
            .transpose()?;
        let is_shp = head == [0x00, 0x00, 0x27, 0x0A];
        let (records, def, factor) = if is_shp {
            let records = shapefile::read(path, field, limits)?;
            match (shapefile::prj_text(path)?, stated_by_caller) {
                (Some(_), Some(_)) => {
                    return Err(refused(
                        &what,
                        "it has a .prj, and a CRS was also stated. Remove one",
                    ))
                }
                (Some(text), None) => {
                    let (metric, factor) = in_metres(text.trim());
                    let mut def = CrsDef::parse_zone_wkt(&metric)?;
                    if factor != 1.0 {
                        scale_false_origin(&mut def, factor);
                    }
                    (records, def, factor)
                }
                (None, Some(def)) => (records, def, 1.0),
                (None, None) => return Err(refused(
                    &what,
                    "it has no .prj, so the CRS of its coordinates is not known. State one with zones_crs",
                )),
            }
        } else {
            let bytes = shapefile::read_bounded(path, limits.max_file_bytes, &what)?;
            let (records, stated) = geojson::read(&bytes, field, &what, limits)?;
            let def = match (stated, stated_by_caller) {
                (geojson::StatedCrs::Wgs84, None) => CrsDef::parse_wkt(WGS84_WKT)?,
                (geojson::StatedCrs::Wgs84, Some(_)) => {
                    return Err(refused(
                        &what,
                        "GeoJSON with no crs member, or one naming WGS84, is longitude and latitude \
                         on WGS84 (RFC 7946), and a CRS was also stated. Remove one",
                    ))
                }
                (geojson::StatedCrs::Other(_), Some(def)) => def,
                (geojson::StatedCrs::Other(name), None) => {
                    return Err(refused(
                        &what,
                        format!("its crs member names {name:?}, which is not read here. State the CRS with zones_crs"),
                    ))
                }
            };
            (records, def, 1.0)
        };
        let mut order: Vec<Zone> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, mut record) in records.into_iter().enumerate() {
            let id = record.id.take().filter(|s| !s.is_empty()).ok_or_else(|| {
                refused(
                    &what,
                    format!(
                        "feature {i} has an empty {field:?}. A zone's name is its pathname's B part. \
                         Observed behaviour we did not implement: Vortex 1.1.0 writes a record with \
                         an empty B part"
                    ),
                )
            })?;
            if factor != 1.0 {
                // `pj_inv` starts with `x * to_meter`.
                for part in &mut record.parts {
                    part.points_mut(|p| {
                        p.x *= factor;
                        p.y *= factor;
                        Ok(())
                    })?;
                }
            }
            let at = *index.entry(id.clone()).or_insert_with(|| {
                order.push(Zone {
                    id,
                    parts: Vec::new(),
                });
                order.len() - 1
            });
            order[at].parts.append(&mut record.parts);
        }
        if order.is_empty() {
            return Err(refused(&what, "it holds no features"));
        }
        Ok(ZoneSet {
            zones: order,
            crs: def,
        })
    }
}

const WGS84_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";

/// One zone's cells on one grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneMask {
    pub id: String,
    /// Cell indices, ascending, with row 0 first in the grid's own row order.
    pub cells: Vec<u32>,
    /// No cell centre was inside, so the cell holding the middle of the extent was taken.
    pub centroid_fallback: bool,
}

/// A grid's placement, as `GDALGeoTransform`.
#[derive(Debug, Clone, PartialEq)]
pub struct GridPlacement {
    pub nx: usize,
    pub ny: usize,
    pub transform: [f64; 6],
    pub crs: CrsDef,
}

fn check_placement(g: &GridPlacement) -> Result<(), VortexError> {
    let t = &g.transform;
    if t[2] != 0.0 || t[4] != 0.0 {
        return Err(refused("grid", "a rotated grid has no reference result"));
    }
    if !t.iter().all(|v| v.is_finite()) || t[1] == 0.0 || t[5] == 0.0 {
        return Err(refused(
            "grid",
            "its transform is not finite, or has no spacing",
        ));
    }
    if g.nx == 0 || g.ny == 0 || g.nx > i32::MAX as usize || g.ny > i32::MAX as usize {
        return Err(refused("grid", "its size is empty or out of range"));
    }
    Ok(())
}

// Reproject every zone into the grid's CRS, as `geometry.Transform` does before rasterizing.
fn transformed(
    zones: &ZoneSet,
    grid: &CrsDef,
    policy: DatumPolicy,
) -> Result<Vec<Zone>, VortexError> {
    let transformer = Transformer::new(&zones.crs, grid, policy)?;
    let mut out = try_vec(zones.zones.len(), "zones")?;
    for zone in &zones.zones {
        let mut copy = zone.clone();
        for part in &mut copy.parts {
            part.points_mut(|p| {
                let (x, y) = transformer.transform(p.x, p.y).map_err(|_| {
                    refused(
                        &format!("zone {}", zone.id),
                        format!(
                            "the point ({}, {}) has no position in the grid's CRS. Observed behaviour we \
                             did not implement: OGR then leaves the geometry in its own CRS",
                            p.x, p.y
                        ),
                    )
                })?;
                if !x.is_finite() || !y.is_finite() {
                    return Err(refused(&format!("zone {}", zone.id), "a point has no finite position in the grid's CRS"));
                }
                p.x = x;
                p.y = y;
                Ok(())
            })?;
        }
        out.push(copy);
    }
    Ok(out)
}

/// Each zone's cells: `createZoneMasks`. Masks are identical to GDAL 3.2.1's `RasterizeLayer` with its
/// defaults in the cases compared, or fall back to the middle of the extent.
pub fn zone_masks(
    zones: &ZoneSet,
    grid: &GridPlacement,
    policy: DatumPolicy,
    limits: &ZoneLimits,
) -> Result<Vec<ZoneMask>, VortexError> {
    check_placement(grid)?;
    let cells = grid
        .nx
        .checked_mul(grid.ny)
        .filter(|&c| c <= limits.max_cells)
        .ok_or_else(|| {
            refused(
                "grid",
                format!(
                    "{} x {} is over the limit of {} cells",
                    grid.nx, grid.ny, limits.max_cells
                ),
            )
        })?;
    let local = transformed(zones, &grid.crs, policy)?;
    let inv = rasterize::invert(&grid.transform);
    let mut scratch = try_vec::<u8>(cells, "mask")?;
    scratch.resize(cells, 0);
    let (mut px, mut py, mut sizes, mut ints) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut work = 0u64;
    let mut entries = 0u64;
    let mut out = try_vec(local.len(), "masks")?;
    for zone in &local {
        let mut burn = rasterize::Burn {
            nx: grid.nx as i32,
            ny: grid.ny as i32,
            cells: &mut scratch,
            first_row: i32::MAX,
            last_row: i32::MIN,
            work: &mut work,
        };
        for part in &zone.parts {
            rasterize::burn_part(
                &mut burn, part, &inv, limits, &mut px, &mut py, &mut sizes, &mut ints,
            )?;
        }
        let (first, last) = (burn.first_row, burn.last_row);
        let mut indices: Vec<u32> = Vec::new();
        if first <= last {
            let from = first as usize * grid.nx;
            let to = (last as usize + 1) * grid.nx;
            let count = scratch[from..to].iter().filter(|&&c| c != 0).count();
            entries += count as u64;
            if entries > limits.max_mask_entries {
                return Err(refused(
                    "zones",
                    format!(
                        "their masks hold over {} cells, the limit",
                        limits.max_mask_entries
                    ),
                ));
            }
            indices = try_vec(count, "mask")?;
            for (i, c) in scratch[from..to].iter_mut().enumerate() {
                if *c != 0 {
                    indices.push((from + i) as u32);
                    *c = 0;
                }
            }
        }
        let mut centroid_fallback = false;
        if indices.is_empty() {
            let mut extent = Envelope::empty();
            for part in &zone.parts {
                part.for_each_point(|p| extent.grow(p));
            }
            if extent.min_x <= extent.max_x {
                centroid_fallback = true;
                let x = (extent.min_x + extent.max_x) / 2.0;
                let y = (extent.min_y + extent.max_y) / 2.0;
                if let Some(cell) =
                    rasterize::centroid_cell(&grid.transform, grid.nx, grid.ny, x, y)
                {
                    indices.push(cell as u32);
                }
            }
        }
        out.push(ZoneMask {
            id: zone.id.clone(),
            cells: indices,
            centroid_fallback,
        });
    }
    Ok(out)
}

/// What differs from the default where Vortex and this library part ways. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compat {
    #[default]
    Engine,
    /// Reproduce the behaviours the module docs mark (*).
    Vortex,
}

/// The eight statistics of `ZonalStatistics`, in doubles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZonalStatistics {
    pub average: f64,
    pub min: f64,
    pub max: f64,
    pub median: f64,
    pub first_quartile: f64,
    pub third_quartile: f64,
    pub pct_gt_zero: f64,
    pub pct_gt_first_quartile: f64,
}

// Double.compare: -0 sorts below +0, and every NaN is equal and above infinity.
fn java_compare(a: &f64, b: &f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Equal,
        (true, false) => Greater,
        (false, true) => Less,
        _ => match a.partial_cmp(b) {
            Some(Equal) => (a.to_bits() as i64).cmp(&(b.to_bits() as i64)),
            Some(order) => order,
            None => Equal,
        },
    }
}

/// The sum Java's `DoubleStream.sum` returns (Vortex sums zone values with it on JDK 21, the runtime
/// both Vortex versions ship). Clean-room implementation: Kahan's compensated
/// summation (W. Kahan, "Further remarks on reducing truncation errors", CACM 8(1), 1965) over the
/// values in order, whose result is the running sum minus the final compensation, and, when that is
/// NaN while the plain left-to-right sum is infinite, the plain sum. The `DoubleStream.sum` Javadoc
/// says only that the sum may be compensated; the variant was chosen by black-box tests on JDK
/// 17.0.20.1+1 and 21.0.9+10, where it equals Java bit for bit on 200,000 random finite arrays
/// (plain, Kahan returning the running sum, and Neumaier's 1974 variant do not). With NaN inputs the
/// result is NaN on both sides, but which NaN's sign and payload survives is not matched. No JDK
/// source was consulted.
fn java_sum(values: &[f64]) -> f64 {
    // Kahan (1965): carry the rounding loss into the next addend. Do not reassociate
    // these operations: the rounded intermediates define the compensation.
    let mut accumulated = 0.0;
    let mut rounding_loss = 0.0;
    let mut ordinary = 0.0;
    for &input in values {
        ordinary += input;
        let adjusted = input - rounding_loss;
        let advanced = accumulated + adjusted;
        rounding_loss = (advanced - accumulated) - adjusted;
        accumulated = advanced;
    }
    // Final correction and infinity recovery were selected by public-API JDK
    // experiments, not by implementation inspection.
    let corrected = accumulated - rounding_loss;
    match (corrected.is_nan(), ordinary.is_infinite()) {
        (true, true) => ordinary,
        _ => corrected,
    }
}

/// `computeZonalStatistics` on values already chosen. `missing` fills what cannot be computed.
pub fn statistics_of(values: &mut [f64], missing: f64, compat: Compat) -> ZonalStatistics {
    values.sort_by(java_compare);
    let count = values.len();
    let mut s = ZonalStatistics {
        average: missing,
        min: missing,
        max: missing,
        median: missing,
        first_quartile: missing,
        third_quartile: missing,
        pct_gt_zero: missing,
        pct_gt_first_quartile: missing,
    };
    let percent = |n: usize| -> f64 {
        match compat {
            Compat::Engine => (100 * n as u64) as f64 / count as f64,
            Compat::Vortex => f64::from((n as i32).wrapping_mul(100)) / f64::from(count as i32),
        }
    };
    if count > 0 {
        s.average = java_sum(values) / count as f64;
        s.min = values[0];
        s.max = values[count - 1];
        s.pct_gt_zero = percent(values.iter().filter(|&&v| v > 0.0).count());
    }
    if count >= 2 {
        s.median = if count.is_multiple_of(2) {
            (values[count / 2] + values[count / 2 - 1]) / 2.0
        } else {
            values[count / 2]
        };
    }
    if count >= 4 {
        s.first_quartile = values[(count + 1) / 4];
        s.third_quartile = values[3 * (count + 1) / 4];
        let q1 = s.first_quartile;
        s.pct_gt_first_quartile = percent(values.iter().filter(|&&v| v > q1).count());
    }
    s
}

/// What is asked of each zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Statistic {
    Average,
    Min,
    Max,
    Median,
    FirstQuartile,
    ThirdQuartile,
    PctGtZero,
    PctGtFirstQuartile,
    /// Not Vortex's. Cells weighted by the share of their area inside the zone, in the grid's own
    /// coordinates. C part suffix `-AREAWT`. Polygon zones only.
    AreaWeightedMean,
}

impl Statistic {
    /// `VortexPoints.getTscs`: the C part's suffix.
    pub fn suffix(self) -> &'static str {
        match self {
            Statistic::Average => "",
            Statistic::Min => "-MIN",
            Statistic::Max => "-MAX",
            Statistic::Median => "-MEDIAN",
            Statistic::FirstQuartile => "-Q1",
            Statistic::ThirdQuartile => "-Q3",
            Statistic::PctGtZero => "-PCT>0",
            Statistic::PctGtFirstQuartile => "-PCT>Q1",
            Statistic::AreaWeightedMean => "-AREAWT",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Statistic::Average => "average",
            Statistic::Min => "min",
            Statistic::Max => "max",
            Statistic::Median => "median",
            Statistic::FirstQuartile => "q1",
            Statistic::ThirdQuartile => "q3",
            Statistic::PctGtZero => "pct_gt_0",
            Statistic::PctGtFirstQuartile => "pct_gt_q1",
            Statistic::AreaWeightedMean => "area_weighted_mean",
        }
    }

    fn pick(self, s: &ZonalStatistics) -> f64 {
        match self {
            Statistic::Average | Statistic::AreaWeightedMean => s.average,
            Statistic::Min => s.min,
            Statistic::Max => s.max,
            Statistic::Median => s.median,
            Statistic::FirstQuartile => s.first_quartile,
            Statistic::ThirdQuartile => s.third_quartile,
            Statistic::PctGtZero => s.pct_gt_zero,
            Statistic::PctGtFirstQuartile => s.pct_gt_first_quartile,
        }
    }
}

/// One frame ready for statistics.
struct Frame {
    placement: GridPlacement,
    cells: Vec<f32>,
    /// What makes a cell missing in this library.
    missing: Missing,
    /// `VortexGrid.noDataValue`.
    vortex_marker: f64,
    description: String,
    units: String,
    /// Whole seconds since 1899-12-31 00:00. `None`: the frame has no time.
    start: Option<i64>,
    end: Option<i64>,
}

#[derive(Clone, Copy)]
enum Missing {
    /// `Grid::is_missing`.
    Dss { specified_null: Option<f32> },
    /// A source's marker; NaN makes every NaN missing.
    Marker(Option<f32>),
}

impl Missing {
    fn is_missing(self, v: f32) -> bool {
        match self {
            Missing::Dss { specified_null } => {
                v.is_nan()
                    || v == GRID_UNDEFINED
                    || v == -901.0
                    || v == -902.0
                    || specified_null == Some(v)
            }
            Missing::Marker(Some(m)) => {
                if m.is_nan() {
                    v.is_nan()
                } else {
                    v == m || v.is_nan()
                }
            }
            Missing::Marker(None) => v.is_nan(),
        }
    }
}

fn hec_seconds(t: (i64, i64)) -> i64 {
    t.0 * 86_400 + t.1
}

/// Grids in a DSS file, by pathname: `DssDataReader`.
struct DssGrids {
    file: DssHandle,
    pathnames: Vec<Vec<u8>>,
}

enum DssHandle {
    V7(Box<Dss7File<std::fs::File>>),
    V6(Box<Dss6File<dss_core::dss6::source::FileSource>>),
}

const GRID_TYPES: [i32; 8] = [400, 401, 410, 411, 420, 421, 430, 431];

/// Where frames come from.
enum Source {
    Dss(DssGrids),
    Other {
        source: Box<dyn GridSource>,
        steps: Vec<(String, usize)>,
    },
}

fn is_dss(path: &Path) -> Result<Option<DssVersion>, VortexError> {
    use std::io::Read;
    let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
    let mut f = std::fs::File::open(path)?;
    let mut n = 0;
    while n < head.len() {
        let k = f.read(&mut head[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    head.truncate(n);
    Ok(FileSignature::from_bytes(&head).ok().map(|s| s.version))
}

fn open(
    path: &Path,
    variables: Option<&[String]>,
    limits: &ZoneLimits,
) -> Result<Source, VortexError> {
    let decode = |m: String| VortexError::Decode {
        format: "DSS",
        message: format!("{}: {m}", path.display()),
    };
    match is_dss(path)? {
        Some(version) => {
            let (file, entries): (DssHandle, Vec<(Vec<u8>, i32)>) = match version {
                DssVersion::V7 => {
                    let mut f = Dss7File::open(path).map_err(|e| decode(e.to_string()))?;
                    let c = f.catalog().map_err(|e| decode(e.to_string()))?;
                    let list = c
                        .catalog
                        .entries
                        .iter()
                        .map(|e| (e.pathname.clone(), e.record_type))
                        .collect();
                    (DssHandle::V7(Box::new(f)), list)
                }
                DssVersion::V6 => {
                    let f = Dss6File::open(path).map_err(|e| decode(e.to_string()))?;
                    let c = f.catalog().map_err(|e| decode(e.to_string()))?;
                    let list = c
                        .entries
                        .iter()
                        .map(|e| (e.pathname.clone(), e.record_type))
                        .collect();
                    (DssHandle::V6(Box::new(f)), list)
                }
            };
            let mut pathnames: Vec<Vec<u8>> = Vec::new();
            match variables {
                None => {
                    for (p, t) in entries {
                        if GRID_TYPES.contains(&t) {
                            pathnames.push(p);
                        }
                    }
                    pathnames.sort();
                }
                Some(wanted) => {
                    for w in wanted {
                        let found = entries.iter().find(|(p, t)| {
                            p.eq_ignore_ascii_case(w.as_bytes()) && GRID_TYPES.contains(t)
                        });
                        match found {
                            Some((p, _)) => pathnames.push(p.clone()),
                            None => {
                                return Err(refused(
                                    &format!("variable {w}"),
                                    "no grid record in the file has this pathname",
                                ))
                            }
                        }
                    }
                }
            }
            if pathnames.is_empty() {
                return Err(refused(
                    &format!("{}", path.display()),
                    "it holds no grid records",
                ));
            }
            Ok(Source::Dss(DssGrids { file, pathnames }))
        }
        None => {
            let options = OpenOptions {
                limits: import::SourceLimits {
                    max_cells: limits.max_cells,
                    ..import::SourceLimits::default()
                },
                ..OpenOptions::default()
            };
            let source = import::open_source(path, &options)?;
            let names = match variables {
                Some(v) => v.to_vec(),
                None => source.variables(),
            };
            let mut steps = Vec::new();
            for name in names {
                for step in 0..source.step_count(&name)? {
                    steps.push((name.clone(), step));
                }
            }
            if steps.is_empty() {
                return Err(refused(&format!("{}", path.display()), "it holds no grids"));
            }
            Ok(Source::Other { source, steps })
        }
    }
}

// `DssDataReader.dssToDto`.
fn dss_frame(grid: Grid, pathname: &[u8]) -> Result<Frame, VortexError> {
    let name = String::from_utf8_lossy(pathname).into_owned();
    let what = format!("grid {name}");
    let parts: Vec<&[u8]> = pathname.split(|&b| b == b'/').collect();
    if parts.len() != 8 {
        return Err(refused(&what, "its pathname does not have six parts"));
    }
    let crs = match grid.grid_type {
        GridType::Hrap => CrsDef::hrap(),
        _ if grid.srs_definition.trim().is_empty() => return Err(refused(
            &what,
            "it states no CRS this library reads, and one is not guessed (an undefined grid, or \
                 an Albers grid other than SHG)",
        )),
        _ => CrsDef::parse_wkt(&grid.srs_definition)?,
    };
    if grid.data.len() != grid.nx.saturating_mul(grid.ny) || grid.nx == 0 || grid.ny == 0 {
        return Err(refused(&what, "its cell count does not match its size"));
    }
    // int * float in Java is float arithmetic, and only the result is widened.
    let cell = grid.cell_size;
    let ulx = f64::from(grid.lower_left_cell_x as f32 * cell);
    let lly = f64::from(grid.lower_left_cell_y as f32 * cell);
    let uly = lly + f64::from(grid.ny as i32 as f32 * cell);
    // RasterUtils.flipVertically: north row first.
    let mut cells = try_vec::<f32>(grid.data.len(), &what)?;
    for row in grid.data.chunks_exact(grid.nx).rev() {
        cells.extend_from_slice(row);
    }
    let d = parts[4];
    let e = parts[5];
    let start = parse_grid_time(d).map(hec_seconds);
    if start.is_none() {
        return Err(refused(
            &what,
            "its D part is not a time. Observed behaviour we did not implement: Vortex 1.1.0 fails on \
             a grid with no time when sorting",
        ));
    }
    let end = parse_grid_time(e).map(hec_seconds).or(start);
    let specified_null = (grid.grid_type == GridType::Specified).then_some(grid.null_value);
    Ok(Frame {
        placement: GridPlacement {
            nx: grid.nx,
            ny: grid.ny,
            transform: [ulx, f64::from(cell), 0.0, uly, 0.0, f64::from(-cell)],
            crs,
        },
        cells,
        missing: Missing::Dss { specified_null },
        vortex_marker: MISSING,
        description: String::from_utf8_lossy(parts[3]).into_owned(),
        units: grid.data_units.clone(),
        start,
        end,
    })
}

fn source_frame(
    source: &dyn GridSource,
    variable: &str,
    step: usize,
) -> Result<Frame, VortexError> {
    let meta = source.step_meta(variable, step)?;
    let frame = source.frame(variable, step)?;
    let parts = frame.into_parts();
    let t = parts.transform;
    let crs = CrsDef::from_frame(&parts.crs)?;
    let (start, end) = match parts.time {
        FrameTime::Invariant => (None, None),
        FrameTime::Instant(t) => (Some(hec_seconds(t)), Some(hec_seconds(t))),
        FrameTime::Period { start, end } => (Some(hec_seconds(start)), Some(hec_seconds(end))),
    };
    Ok(Frame {
        placement: GridPlacement {
            nx: parts.nx,
            ny: parts.ny,
            transform: [
                t.origin_x,
                t.dx,
                t.row_rotation,
                t.origin_y,
                t.column_rotation,
                t.dy,
            ],
            crs,
        },
        cells: parts.cells,
        missing: Missing::Marker(parts.nodata),
        vortex_marker: parts.nodata.map_or(f64::NAN, f64::from),
        description: meta.description.unwrap_or_else(|| parts.variable.clone()),
        units: parts.units,
        start,
        end,
    })
}

/// One grid-to-point run, before any output.
#[derive(Debug, Clone)]
pub struct ZonalRequest {
    pub grids: PathBuf,
    /// Exact DSS pathnames or source variables. `None` is every grid in the file.
    pub variables: Option<Vec<String>>,
    pub zones: PathBuf,
    pub field: String,
    pub zones_crs: Option<Crs>,
    pub datum_policy: DatumPolicy,
    pub compat: Compat,
    pub limits: ZoneLimits,
    /// Compute the optional area-weighted mean as well.
    pub area_weighted: bool,
}

/// One frame's identity in a table.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameRow {
    pub description: String,
    pub units: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// Every statistic of every zone and frame.
#[derive(Debug, Clone)]
pub struct ZonalTable {
    pub zones: Vec<ZoneMask>,
    pub frames: Vec<FrameRow>,
    /// `frames.len() * zones.len()`, frame by frame, zones in `zones` order.
    pub stats: Vec<ZonalStatistics>,
    /// The same shape when asked for. A zone with no valid area holds [`MISSING`].
    pub area_weighted: Option<Vec<f64>>,
    /// Each distinct placement in frame order: where masks had to be built.
    pub placements: Vec<GridPlacement>,
}

// Share of each cell inside each zone: sparse, ascending. Refuses overlap, since a share over 1
// would weigh a cell twice.
fn area_weights(
    zones: &ZoneSet,
    g: &GridPlacement,
    policy: DatumPolicy,
    limits: &ZoneLimits,
) -> Result<Vec<Vec<(u32, f64)>>, VortexError> {
    let local = transformed(zones, &g.crs, policy)?;
    let inv = rasterize::invert(&g.transform);
    let cells = g.nx.saturating_mul(g.ny);
    if cells > limits.max_cells {
        return Err(refused("grid", "it is over the cell limit"));
    }
    let mut scratch = try_vec::<f64>(cells, "area weights")?;
    scratch.resize(cells, 0.0);
    let mut out = try_vec(local.len(), "area weights")?;
    for zone in &local {
        let mut touched: Vec<u32> = Vec::new();
        for part in &zone.parts {
            let Part::Polygon(rings) = part else {
                return Err(refused(
                    &format!("zone {}", zone.id),
                    "the area-weighted mean needs polygons, and this zone holds a point or line",
                ));
            };
            rasterize::area_fractions(rings, &inv, g.nx, g.ny, |index, area| {
                if scratch[index] == 0.0 {
                    touched.push(index as u32);
                }
                scratch[index] += area;
            });
        }
        touched.sort_unstable();
        touched.dedup();
        let mut weights = try_vec(touched.len(), "area weights")?;
        for &index in &touched {
            let w = scratch[index as usize];
            scratch[index as usize] = 0.0;
            if w > 1.0 + 1e-9 {
                return Err(refused(
                    &format!("zone {}", zone.id),
                    "polygons in this zone overlap, so a cell's share would count twice",
                ));
            }
            if w > 1e-12 {
                weights.push((index, w.min(1.0)));
            }
        }
        out.push(weights);
    }
    Ok(out)
}

/// Read the grids and compute. Nothing is written.
pub fn compute(request: &ZonalRequest) -> Result<ZonalTable, VortexError> {
    let limits = &request.limits;
    let zones = ZoneSet::read(
        &request.zones,
        &request.field,
        request.zones_crs.as_ref(),
        limits,
    )?;
    let mut source = open(&request.grids, request.variables.as_deref(), limits)?;
    let count = match &source {
        Source::Dss(g) => g.pathnames.len(),
        Source::Other { steps, .. } => steps.len(),
    };
    let rows = (count as u64).saturating_mul(zones.zones.len() as u64);
    if rows > limits.max_rows {
        return Err(refused(
            "run",
            format!(
                "{count} frames and {} zones are over the limit of {} rows",
                zones.zones.len(),
                limits.max_rows
            ),
        ));
    }
    let mut stats = try_vec::<ZonalStatistics>(rows as usize, "statistics")?;
    let mut area = if request.area_weighted {
        Some(try_vec::<f64>(rows as usize, "statistics")?)
    } else {
        None
    };
    let mut frames = try_vec::<FrameRow>(count, "frames")?;
    let mut current: Option<GridPlacement> = None;
    let mut masks: Vec<ZoneMask> = Vec::new();
    let mut weights: Vec<Vec<(u32, f64)>> = Vec::new();
    let mut placements: Vec<GridPlacement> = Vec::new();
    let options = GridReadOptions {
        max_cells: limits.max_cells,
        ..GridReadOptions::default()
    };
    let mut values: Vec<f64> = Vec::new();
    for at in 0..count {
        let frame = match &mut source {
            Source::Dss(g) => {
                let pathname = g.pathnames[at].clone();
                let name = String::from_utf8_lossy(&pathname).into_owned();
                let grid = match &mut g.file {
                    DssHandle::V7(f) => f.read_grid_with(&pathname, &options),
                    DssHandle::V6(f) => f.read_grid_with(&pathname, &options),
                }
                .map_err(|e| VortexError::Decode {
                    format: "DSS",
                    message: format!("{name}: {e}"),
                })?;
                dss_frame(grid, &pathname)?
            }
            Source::Other { source, steps } => {
                let (name, step) = steps[at].clone();
                source_frame(source.as_ref(), &name, step)?
            }
        };
        if frame.placement.nx.saturating_mul(frame.placement.ny) != frame.cells.len() {
            return Err(refused("grid", "its cell count does not match its size"));
        }
        if current.as_ref() != Some(&frame.placement) {
            let built = zone_masks(&zones, &frame.placement, request.datum_policy, limits)?;
            // A later grid's zones must line up with the table's columns; only their cells change.
            masks = built;
            if request.area_weighted {
                weights = area_weights(&zones, &frame.placement, request.datum_policy, limits)?;
            }
            current = Some(frame.placement.clone());
            placements.push(frame.placement.clone());
        }
        let vortex = request.compat == Compat::Vortex;
        let missing_value = if vortex { frame.vortex_marker } else { MISSING };
        for (z, mask) in masks.iter().enumerate() {
            values.clear();
            values
                .try_reserve(mask.cells.len())
                .map_err(|_| refused("statistics", "a zone's values could not be allocated"))?;
            for &i in &mask.cells {
                let v = frame.cells[i as usize];
                let wide = f64::from(v);
                let excluded = if vortex {
                    // `Double.compare(value, noDataValue) == 0`.
                    java_compare(&wide, &frame.vortex_marker).is_eq()
                } else {
                    frame.missing.is_missing(v)
                };
                if !excluded {
                    values.push(wide);
                }
            }
            stats.push(statistics_of(&mut values, missing_value, request.compat));
            if let Some(area) = &mut area {
                let (mut sum, mut weight) = (0.0, 0.0);
                for &(i, w) in &weights[z] {
                    let v = frame.cells[i as usize];
                    if !frame.missing.is_missing(v) {
                        sum += f64::from(v) * w;
                        weight += w;
                    }
                }
                area.push(if weight > 0.0 { sum / weight } else { MISSING });
            }
        }
        frames.push(FrameRow {
            description: frame.description,
            units: frame.units,
            start: frame.start,
            end: frame.end,
        });
    }
    Ok(ZonalTable {
        zones: masks,
        frames,
        stats,
        area_weighted: area,
        placements,
    })
}

/// `DssDataWriter.getDssDataType`. It reads the variable's name, not the grid's own data type.
fn data_type(variable: Option<Variable>, period: i64) -> &'static str {
    use Variable as V;
    match variable {
        Some(V::Precipitation) => "PER-CUM",
        Some(V::PrecipitationFrequency | V::Albedo) => "INST-VAL",
        Some(
            V::Temperature
            | V::Humidity
            | V::ShortwaveRadiation
            | V::LongwaveRadiation
            | V::Pressure
            | V::Windspeed
            | V::SnowWaterEquivalent,
        ) => {
            if period == 0 {
                "INST-VAL"
            } else {
                "PER-AVER"
            }
        }
        // `DssDataType.INVAL`, whose text is what HEC's reader then returns. Matched as observed; whether
        // to keep matching it is an open question.
        _ => "INVALID",
    }
}

/// `getCPartForTimeSeries` with the type above, which is never INST-CUM for precipitation.
fn c_part(description: &str, variable: Option<Variable>) -> String {
    match variable {
        Some(Variable::Precipitation) => "PRECIP-INC".to_string(),
        Some(v) => v.dss_c_part().to_string(),
        None => description.to_uppercase(),
    }
}

/// `DssUtil.getEPart`, with only the labels whose interval is the spacing.
fn e_part(seconds: i64) -> Option<&'static str> {
    Some(match seconds {
        60 => "1Minute",
        120 => "2Minute",
        180 => "3Minute",
        240 => "4Minute",
        300 => "5Minute",
        360 => "6Minute",
        600 => "10Minute",
        720 => "12Minute",
        900 => "15Minute",
        1200 => "20Minute",
        1800 => "30Minute",
        3600 => "1Hour",
        7200 => "2Hour",
        10800 => "3Hour",
        14400 => "4Hour",
        21600 => "6Hour",
        28800 => "8Hour",
        43200 => "12Hour",
        86400 => "1Day",
        604800 => "1Week",
        _ => return None,
    })
}

/// Pathname parts a caller may state. D and E always come from the frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointParts {
    pub a: Option<String>,
    pub f: Option<String>,
}

/// A grid-to-point run.
#[derive(Debug, Clone)]
pub struct GridToPointRequest {
    pub zonal: ZonalRequest,
    pub output: PathBuf,
    pub statistics: Vec<Statistic>,
    /// `isAccumulate`: a `PRECIP-CUM` record beside each `PRECIP-INC` average.
    pub accumulate: bool,
    pub parts: PointParts,
    /// DSS version of a new output. An existing file keeps its own.
    pub version: Option<u8>,
    pub allow_undetectable_readers: bool,
    /// Permit `output` to be the file holding the grids. Off by default, since a distinct name is no
    /// proof of a distinct file. Zone files are never a permitted output.
    pub allow_in_place: bool,
    /// Crash simulation at this record of the run, for tests.
    #[doc(hidden)]
    pub fault: Option<(usize, FaultPoint)>,
}

/// One record written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointRecord {
    pub zone: String,
    pub description: String,
    /// A [`Statistic::name`], or `accumulated`.
    pub statistic: String,
    pub pathname: String,
    pub regular: bool,
    pub values: usize,
    pub units: String,
    pub data_type: String,
    /// What this run did to the blocks it wrote, not what the series now holds. `added`: every block
    /// was new. `unchanged`: every block already held these values. `window_updated`: at least one
    /// block already held something else, and now holds this run's values at this run's stamps. In
    /// each case the pathname keeps any value outside this run's window, so none of them means
    /// the series holds only `values` values.
    pub outcome: String,
}

/// What a run produced.
#[derive(Debug, Clone)]
pub struct GridToPointOutcome {
    pub records: Vec<PointRecord>,
    pub zones: Vec<ZoneMask>,
    pub frames: usize,
    pub readers_undetected: bool,
    /// Values stored although a reader takes them as missing. Always 0 except under [`Compat::Vortex`],
    /// as any other run refuses them.
    pub reserved_values: usize,
}

/// Why a run stopped.
#[derive(Debug)]
pub enum GridToPointFailure {
    Vortex(VortexError),
    Write(WriteError),
    BadRequest(String),
}

/// A run that stopped. With `phase` `planning`, nothing was written. With `writing`, see the fields.
#[derive(Debug)]
pub struct GridToPointError {
    pub phase: &'static str,
    pub failure: GridToPointFailure,
    /// Records the writer acknowledged before the run stopped. This is not every change the file holds:
    /// see `in_doubt`. An empty list does not mean the file is unchanged.
    pub completed: Vec<PointRecord>,
    pub pathname: Option<String>,
    /// The series being written when the writer failed, in the form planned. It may hold none, some or
    /// all of its new blocks, and a reader may see them: the writer leaves no torn block, but
    /// publishes one before the steps that can then fail, and a series spans several blocks. `None`
    /// when no write of a record had begun, or the run removed the new file it had made.
    pub in_doubt: Option<String>,
}

impl fmt::Display for GridToPointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "grid to point stopped while {}", self.phase)?;
        if let Some(p) = &self.pathname {
            write!(f, " at {p}")?;
        }
        write!(f, ", after {} acknowledged records: ", self.completed.len())?;
        match &self.failure {
            GridToPointFailure::Vortex(e) => write!(f, "{e}")?,
            GridToPointFailure::Write(e) => write!(f, "{e}")?,
            GridToPointFailure::BadRequest(m) => write!(f, "{m}")?,
        }
        if let Some(p) = &self.in_doubt {
            write!(
                f,
                "; {p} was being written and may hold none, some or all of its new values, so this \
                 failure does not mean the file is unchanged: read the file, or run again"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for GridToPointError {}

fn planning(failure: GridToPointFailure) -> GridToPointError {
    GridToPointError {
        phase: "planning",
        failure,
        completed: Vec::new(),
        pathname: None,
        in_doubt: None,
    }
}

impl From<VortexError> for GridToPointFailure {
    fn from(e: VortexError) -> Self {
        GridToPointFailure::Vortex(e)
    }
}

/// One record planned.
struct Planned {
    record: PointRecord,
    pathname: Vec<u8>,
    times: Times,
    values: Vec<f32>,
    /// HecMath's accumulation adds a quality word per value, all zero on Vortex 1.1.0.
    quality: Option<Vec<i32>>,
}

enum Times {
    Regular(i64),
    Irregular(Vec<i64>),
}

/// Why a computed value cannot go in a record as a value, by its final float32, as the cast itself
/// can create a marker. A statistic exactly at [`MISSING`] means no valid cell, and is stored as that.
fn stored_value_problem(computed: f64, stored: f32) -> Option<String> {
    if computed == MISSING {
        return None;
    }
    if reader_takes_as_missing(stored) {
        return Some(format!(
            "the valid result {computed:?} is stored as {stored:?} in float32, which readers take as \
             missing, so it would be written and then lost. Nothing was written. Observed behaviour we \
             did not implement: Vortex stores it. compatibility=\"vortex\" gives Vortex's record"
        ));
    }
    if !stored.is_finite() {
        return Some(format!(
            "the result {computed:?} is stored as {stored:?} in float32, which is no value. Nothing \
             was written"
        ));
    }
    None
}

/// HEC seconds (from 1899-12-31) as text, for a message. Hinnant's `civil_from_days`.
fn stamp_text(seconds: i64) -> String {
    let (days, rest) = (seconds.div_euclid(86400), seconds.rem_euclid(86400));
    // 1899-12-31 is 25568 days before 1970-01-01, and 1970-01-01 is 719468 days after 0000-03-01.
    let z = days - 25568 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// A record's outcome from its blocks' (see [`PointRecord::outcome`]).
fn record_outcome<'a>(mut blocks: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = blocks.next() else {
        return "unchanged".to_string();
    };
    if first != "replaced" && blocks.all(|b| b == first) {
        first.to_string()
    } else {
        "window_updated".to_string()
    }
}

fn is_valid_value(v: f64) -> bool {
    !(v == MISSING || v == -901.0 || v == -902.0 || v.is_nan())
}

// HecMath accumulation: running double sum, with an invalid value leaving the running total as it was.
fn running_totals(values: &[f64]) -> Vec<f64> {
    let mut total = 0.0f64;
    values
        .iter()
        .map(|&v| {
            if is_valid_value(v) {
                total += v;
            }
            total
        })
        .collect()
}

/// Refuse a value that cannot be stored as one; under [`Compat::Vortex`], store it as Vortex does.
fn check_stored(
    compat: Compat,
    computed: f64,
    stored: f32,
    zone: &str,
    statistic: &str,
    stamp: i64,
    reserved_values: &mut usize,
) -> Result<(), VortexError> {
    let Some(problem) = stored_value_problem(computed, stored) else {
        return Ok(());
    };
    if compat == Compat::Vortex {
        *reserved_values += usize::from(reader_takes_as_missing(stored));
        return Ok(());
    }
    Err(refused(
        &format!(
            "zone {zone}, statistic {statistic}, time {}",
            stamp_text(stamp)
        ),
        problem,
    ))
}

fn plan_records(
    request: &GridToPointRequest,
    table: &ZonalTable,
    reserved_values: &mut usize,
) -> Result<Vec<Planned>, VortexError> {
    let compat = request.zonal.compat;
    // `ids.forEach(id -> descriptions.forEach(...))`: one series per zone and description.
    let mut descriptions: Vec<&str> = Vec::new();
    for f in &table.frames {
        if !descriptions.contains(&f.description.as_str()) {
            descriptions.push(&f.description);
        }
    }
    let n_zones = table.zones.len();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut out = Vec::new();
    for description in descriptions {
        let what = format!("variable {description}");
        // Sorted by start, stable, as `Comparator.comparing(VortexPoint::startTime)`.
        let mut order: Vec<usize> = (0..table.frames.len())
            .filter(|&i| table.frames[i].description == description)
            .collect();
        let mut rows = try_vec::<(i64, i64)>(order.len(), &what)?;
        for &i in &order {
            let f = &table.frames[i];
            let (Some(start), Some(end)) = (f.start, f.end) else {
                return Err(refused(&what, "a frame has no time, so it has no place in a time series. Observed behaviour we did not implement: Vortex fails here"));
            };
            if start % 60 != 0 || end % 60 != 0 {
                return Err(refused(
                    &what,
                    "a frame's time is not a whole minute, which `HecTime` cannot hold",
                ));
            }
            rows.push((start, end));
        }
        order.sort_by_key(|&i| table.frames[i].start);
        rows.sort_by_key(|r| r.0);
        let first = &table.frames[order[0]];
        if let Some(other) = order
            .iter()
            .map(|&i| &table.frames[i])
            .find(|f| f.units != first.units)
        {
            return Err(refused(
                &what,
                format!(
                    "its frames state units {:?} and {:?}. Observed behaviour we did not implement: \
                     Vortex labels the whole series with the first",
                    first.units, other.units
                ),
            ));
        }
        for w in rows.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(refused(
                    &what,
                    "two frames start at the same time, so one series cannot hold both",
                ));
            }
        }
        let starts: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let mut ends: Vec<i64> = rows.iter().map(|r| r.1).collect();
        // `isRegular`: more than one frame, with one spacing.
        let spacing = if starts.len() > 1 {
            starts[1] - starts[0]
        } else {
            0
        };
        let regular = starts.len() > 1 && starts.windows(2).all(|w| w[1] - w[0] == spacing);
        let period = rows[0].1 - rows[0].0;
        let variable = variable_from_name(description);
        let dtype = data_type(variable, period);
        let is_period = dtype == "PER-CUM" || dtype == "PER-AVER";
        if compat == Compat::Engine {
            if ends.windows(2).any(|w| w[1] < w[0]) {
                return Err(refused(&what, "a later frame ends before an earlier one. Observed behaviour we did not implement: Vortex sorts ends apart from starts, and stamps a value with another frame's end"));
            }
            if is_period {
                if let Some(bad) = rows
                    .iter()
                    .find(|r| r.1 - r.0 != if regular { spacing } else { r.1 - r.0 } || r.1 == r.0)
                {
                    return Err(refused(
                        &what,
                        format!(
                            "a frame lasting {} s in a {dtype} series {}. Observed behaviour we did not implement: Vortex stamps it one spacing after its start, which is not the period's end. compatibility=\"vortex\" gives Vortex's record",
                            bad.1 - bad.0,
                            if regular { format!("spaced {spacing} s") } else { "has no period".to_string() },
                        ),
                    ));
                }
            }
        }
        ends.sort_unstable();
        let (e, start_stamp) = if regular {
            let e = e_part(spacing).ok_or_else(|| {
                refused(
                    &what,
                    format!(
                        "its frames are {spacing} s apart, which is not a DSS interval. Observed behaviour we did not implement: `getEPart` gives such a spacing the E part 0, or another interval's name"
                    ),
                )
            })?;
            // `setRegularTscTimes`.
            (e, starts[0] + if is_period { spacing } else { 0 })
        } else {
            ("IR-Day", 0)
        };
        if request.accumulate && compat == Compat::Vortex && regular && period != spacing {
            return Err(refused(
                &what,
                "accumulation over a period that is not the spacing has no compared result",
            ));
        }
        // The stamp each value is stored at, for a refusal's message.
        let stamp_at = |k: usize| {
            if regular {
                start_stamp + k as i64 * spacing
            } else {
                ends[k]
            }
        };
        let c = c_part(description, variable);
        let a = request.parts.a.as_deref().unwrap_or("");
        let f = request.parts.f.as_deref().unwrap_or("");
        for (z, zone) in table.zones.iter().enumerate() {
            let row = |frame: usize| frame * n_zones + z;
            let mut add = |statistic: String,
                           suffix: &str,
                           units: &str,
                           dtype: &str,
                           values: Vec<f32>|
             -> Result<(), VortexError> {
                let pathname = format!("/{a}/{}/{c}{suffix}//{e}/{f}/", zone.id).into_bytes();
                if !seen.insert(pathname.to_ascii_uppercase()) {
                    return Err(refused(
                        &format!("zone {}", zone.id),
                        format!(
                            "two records would share the pathname {}, as DSS compares it, with no case. Observed behaviour we did not implement: the second overwrites the first",
                            String::from_utf8_lossy(&pathname)
                        ),
                    ));
                }
                out.push(Planned {
                    record: PointRecord {
                        zone: zone.id.clone(),
                        description: description.to_string(),
                        statistic,
                        pathname: String::from_utf8_lossy(&pathname).into_owned(),
                        regular,
                        values: values.len(),
                        units: units.to_string(),
                        data_type: dtype.to_string(),
                        outcome: String::new(),
                    },
                    pathname,
                    times: if regular {
                        Times::Regular(start_stamp)
                    } else {
                        Times::Irregular(ends.clone())
                    },
                    values,
                    quality: None,
                });
                Ok(())
            };
            for &statistic in &request.statistics {
                let mut series = try_vec::<f32>(order.len(), &what)?;
                for (k, &i) in order.iter().enumerate() {
                    let v = if statistic == Statistic::AreaWeightedMean {
                        table.area_weighted.as_ref().map_or(MISSING, |a| a[row(i)])
                    } else {
                        statistic.pick(&table.stats[row(i)])
                    };
                    let stored = v as f32;
                    check_stored(
                        compat,
                        v,
                        stored,
                        &zone.id,
                        statistic.name(),
                        stamp_at(k),
                        reserved_values,
                    )?;
                    series.push(stored);
                }
                let units = if matches!(
                    statistic,
                    Statistic::PctGtZero | Statistic::PctGtFirstQuartile
                ) {
                    "%"
                } else {
                    first.units.as_str()
                };
                add(
                    statistic.name().to_string(),
                    statistic.suffix(),
                    units,
                    dtype,
                    series,
                )?;
            }
            // `tscs.stream().filter(C part is PRECIP-INC)`: the average only, and only precipitation.
            if request.accumulate
                && c == "PRECIP-INC"
                && request.statistics.contains(&Statistic::Average)
            {
                let doubles: Vec<f64> =
                    order.iter().map(|&i| table.stats[row(i)].average).collect();
                let cum = format!("/{a}/{}/PRECIP-CUM//{e}/{f}/", zone.id).into_bytes();
                if !seen.insert(cum.to_ascii_uppercase()) {
                    return Err(refused(
                        &format!("zone {}", zone.id),
                        "the accumulated record's pathname is already in use",
                    ));
                }
                // Observed on Vortex 1.1.0 and 0.13.3: a series all at `-FLT_MAX` gets no accumulated
                // record, and one all NaN (only under `Compat::Vortex`) gets zeros.
                if doubles.iter().all(|&v| v == MISSING) {
                    continue;
                }
                let totals = running_totals(&doubles);
                let values: Vec<f32> = totals.iter().map(|&t| t as f32).collect();
                for (k, (&total, &stored)) in totals.iter().zip(&values).enumerate() {
                    check_stored(
                        compat,
                        total,
                        stored,
                        &zone.id,
                        "accumulated",
                        stamp_at(k),
                        reserved_values,
                    )?;
                }
                out.push(Planned {
                    record: PointRecord {
                        zone: zone.id.clone(),
                        description: description.to_string(),
                        statistic: "accumulated".to_string(),
                        pathname: String::from_utf8_lossy(&cum).into_owned(),
                        regular,
                        values: values.len(),
                        units: first.units.clone(),
                        data_type: "INST-CUM".to_string(),
                        outcome: String::new(),
                    },
                    pathname: cum,
                    times: if regular {
                        Times::Regular(start_stamp)
                    } else {
                        Times::Irregular(ends.clone())
                    },
                    values,
                    quality: Some(vec![0; doubles.len()]),
                });
            }
        }
    }
    Ok(out)
}

fn its6<'a>(p: &'a Planned, times: &'a [i64]) -> dss_core::dss6::write::ItsInput<'a> {
    dss_core::dss6::write::ItsInput {
        pathname: &p.pathname,
        times,
        values: RtsValues::F32(&p.values),
        quality: p.quality.as_deref(),
        units: p.record.units.as_bytes(),
        data_type: p.record.data_type.as_bytes(),
        mode: dss_core::dss6::write::ItsMode::Replace,
    }
}

fn validate(p: &Planned, version: u8) -> Result<(), WriteError> {
    let values = RtsValues::F32(&p.values);
    match &p.times {
        Times::Regular(start) => {
            let mut input = RtsInput::new(
                &p.pathname,
                *start,
                values,
                p.record.units.as_bytes(),
                p.record.data_type.as_bytes(),
            );
            input.quality = p.quality.as_deref();
            if version == 6 {
                dss_core::dss6::write::validate_rts6_input(&input)
            } else {
                write7::validate_rts_input(&input)
            }
        }
        Times::Irregular(times) => {
            if version == 6 {
                dss_core::dss6::write::validate_its6_input(&its6(p, times)).map(|_| ())
            } else {
                let mut input = ItsInput::new(
                    &p.pathname,
                    times,
                    values,
                    p.record.units.as_bytes(),
                    p.record.data_type.as_bytes(),
                );
                input.quality = p.quality.as_deref();
                write7::validate_its_input(&input).map(|_| ())
            }
        }
    }
}

fn store(
    p: &Planned,
    version: u8,
    output: &Path,
    options: &WriteOptions,
) -> Result<write7::WriteReport, WriteError> {
    let values = RtsValues::F32(&p.values);
    match &p.times {
        Times::Regular(start) => {
            let mut input = RtsInput::new(
                &p.pathname,
                *start,
                values,
                p.record.units.as_bytes(),
                p.record.data_type.as_bytes(),
            );
            input.quality = p.quality.as_deref();
            if version == 6 {
                dss_core::dss6::write::write_rts6(output, &input, options)
            } else {
                write7::write_rts(output, &input, options)
            }
        }
        Times::Irregular(times) => {
            if version == 6 {
                dss_core::dss6::write::write_its6(output, &its6(p, times), options)
            } else {
                let mut input = ItsInput::new(
                    &p.pathname,
                    times,
                    values,
                    p.record.units.as_bytes(),
                    p.record.data_type.as_bytes(),
                );
                input.quality = p.quality.as_deref();
                write7::write_its(output, &input, options)
            }
        }
    }
}

/// Compute, plan, then write. Nothing touches `output` until every statistic and record is settled.
pub fn grid_to_point(request: &GridToPointRequest) -> Result<GridToPointOutcome, GridToPointError> {
    let bad = |m: String| planning(GridToPointFailure::BadRequest(m));
    if !matches!(request.version, None | Some(6) | Some(7)) {
        return Err(bad(format!(
            "version must be 6 or 7, not {:?}",
            request.version
        )));
    }
    if request.statistics.is_empty() {
        return Err(bad("no statistic was asked for".to_string()));
    }
    let mut distinct = HashSet::new();
    if !request.statistics.iter().all(|s| distinct.insert(*s)) {
        return Err(bad("a statistic was asked for twice".to_string()));
    }
    let wants_area = request.statistics.contains(&Statistic::AreaWeightedMean);
    if wants_area != request.zonal.area_weighted {
        return Err(bad(
            "area_weighted must be set exactly when the area-weighted mean is asked for"
                .to_string(),
        ));
    }
    for (name, part) in [("A", &request.parts.a), ("F", &request.parts.f)] {
        if part.as_deref().is_some_and(|p| p.contains('/')) {
            return Err(bad(format!("part {name} holds a slash")));
        }
    }
    if let Some(parent) = request
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        if !parent.is_dir() {
            return Err(bad(format!(
                "output folder {} does not exist",
                parent.display()
            )));
        }
    }
    // By the file itself, so another name for an input (a hard link, say) is refused as well. A check
    // that cannot be made refuses too. Only an existing output can be an input.
    for (input, may_share) in [
        (&request.zonal.grids, request.allow_in_place),
        (&request.zonal.zones, false),
    ] {
        let same = file_identity::same_file(input, &request.output).map_err(|e| {
            planning(GridToPointFailure::Vortex(refused(
                &format!("output {}", request.output.display()),
                format!(
                    "cannot tell whether it is the input {}: {e}",
                    input.display()
                ),
            )))
        })?;
        if same && !may_share {
            return Err(planning(GridToPointFailure::Vortex(refused(
                &format!("output {}", request.output.display()),
                format!(
                    "it is also the input {}, which this run would change. Name another output, or \
                     state allow_in_place to add series to the grids' own file",
                    input.display()
                ),
            ))));
        }
    }
    let existing = if request.output.exists() {
        let version = is_dss(&request.output)
            .map_err(|e| planning(e.into()))?
            .ok_or_else(|| {
                planning(GridToPointFailure::Vortex(refused(
                    &format!("output {}", request.output.display()),
                    "it exists and is not a DSS file",
                )))
            })?;
        Some(match version {
            DssVersion::V6 => 6u8,
            DssVersion::V7 => 7u8,
        })
    } else {
        None
    };
    if let (Some(have), Some(want)) = (existing, request.version) {
        if have != want {
            return Err(planning(GridToPointFailure::Vortex(refused(
                &format!("output {}", request.output.display()),
                format!("it is DSS{have}, and version {want} was asked for. Writers keep a file's version"),
            ))));
        }
    }
    let version = existing.or(request.version).unwrap_or(7);
    let policy = LockPolicy {
        allow_undetectable_readers: request.allow_undetectable_readers,
        ..LockPolicy::default()
    };
    // The lock, up front, so a held file fails before any decoding. The writer checks again.
    if existing.is_some() {
        let report = lock::lock_status(&request.output)
            .map_err(|e| planning(GridToPointFailure::Write(WriteError::Lock(e))))?;
        lock::ensure_unlocked_for_write(&report, &policy)
            .map_err(|r| planning(GridToPointFailure::Write(WriteError::Locked(r))))?;
    }
    let table = compute(&request.zonal).map_err(|e| planning(e.into()))?;
    let mut reserved_values = 0usize;
    let plan =
        plan_records(request, &table, &mut reserved_values).map_err(|e| planning(e.into()))?;
    for p in &plan {
        validate(p, version).map_err(|e| GridToPointError {
            phase: "planning",
            failure: GridToPointFailure::Write(e),
            completed: Vec::new(),
            pathname: Some(p.record.pathname.clone()),
            in_doubt: None,
        })?;
    }
    let created = existing.is_none();
    if created {
        let made = if version == 6 {
            dss_core::dss6::write::create_dss6(&request.output)
        } else {
            write7::create_dss7(&request.output)
        };
        made.map_err(|e| planning(GridToPointFailure::Write(e)))?;
    }
    let mut completed: Vec<PointRecord> = Vec::new();
    let mut readers_undetected = false;
    for (i, mut p) in plan.into_iter().enumerate() {
        let options = WriteOptions {
            lock_policy: policy,
            fault: request.fault.filter(|(at, _)| *at == i).map(|(_, f)| f),
            ..WriteOptions::default()
        };
        match store(&p, version, &request.output, &options) {
            Ok(report) => {
                readers_undetected |= report.readers_undetected;
                p.record.outcome = record_outcome(report.blocks.iter().map(|b| b.outcome.as_str()));
                completed.push(p.record);
            }
            Err(e) => {
                let is_fault = matches!(e, WriteError::InjectedFault { .. });
                // A new file with nothing in it says nothing: leave no such thing behind. A simulated
                // crash keeps it, as a real one would.
                let removed = created
                    && completed.is_empty()
                    && !is_fault
                    && std::fs::remove_file(&request.output).is_ok();
                return Err(GridToPointError {
                    phase: "writing",
                    failure: GridToPointFailure::Write(e),
                    completed,
                    in_doubt: (!removed).then(|| p.record.pathname.clone()),
                    pathname: Some(p.record.pathname),
                });
            }
        }
    }
    Ok(GridToPointOutcome {
        records: completed,
        zones: table.zones,
        frames: table.frames.len(),
        readers_undetected,
        reserved_values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_sum_subtracts_the_compensation() {
        // A sum whose plain total loses the small terms: JDK 17+'s `sum - compensation` recovers them.
        let mut values = vec![1e16, 1.0, 1.0, 1.0, 1.0];
        let compensated = java_sum(&values);
        assert_eq!(compensated, 1e16 + 4.0);
        let plain: f64 = values.iter().sum();
        assert_ne!(plain, compensated, "this input must separate the two sums");
        values.reverse();
        assert_eq!(java_sum(&values), 1e16 + 4.0);
    }

    /// Black-box equivalence with the JDK: 200,000 seeded arrays, generated as Java's public API
    /// generates them; Java 17.0.20.1+1 and 21.0.9+10 both give the asserted
    /// `DoubleStream.sum` hash. Every NaN hashes alike.
    #[test]
    fn java_sum_equals_java_black_box() {
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }
            fn below(&mut self, n: u64) -> u64 {
                self.next() % n
            }
            fn unit(&mut self) -> f64 {
                (self.next() >> 11) as f64 * f64::from_bits(0x3CA0_0000_0000_0000)
                // 2^-53
            }
            fn value(&mut self) -> f64 {
                let pick = |r: &mut Rng, a: f64, b: f64| if r.below(2) == 0 { a } else { b };
                match self.below(12) {
                    0 => f64::from_bits(self.next()),
                    1 => {
                        let sign = pick(self, 1.0, -1.0);
                        let m = 1.0 + self.unit();
                        let p = f64::from_bits(((self.below(120) as i64 - 60 + 1023) as u64) << 52);
                        sign * m * p
                    }
                    2 => pick(self, 1e16, -1e16),
                    3 => self.below(5) as f64 - 2.0,
                    4 => pick(self, 0.0, -0.0),
                    5 => f64::from_bits(1) * self.below(1000) as f64,
                    6 => {
                        if self.below(3) == 0 {
                            f64::INFINITY
                        } else {
                            pick(self, f64::NEG_INFINITY, f64::MAX)
                        }
                    }
                    7 => -f64::MAX,
                    8 => f64::NAN,
                    _ => self.unit() * 1000.0 - 500.0,
                }
            }
        }
        fn eat8(h: &mut u64, d: f64) {
            let bits = if d.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                d.to_bits()
            };
            for b in bits.to_le_bytes() {
                *h ^= u64::from(b);
                *h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        let mut r = Rng(0x5059);
        let (mut all, mut finite, mut n_finite) = (0xcbf2_9ce4_8422_2325, 0xcbf2_9ce4_8422_2325, 0);
        for _ in 0..200_000 {
            let len = r.below(40) as usize;
            let mut v: Vec<f64> = (0..len).map(|_| r.value()).collect();
            if r.below(2) == 0 {
                v.sort_by(java_compare);
            }
            let s = java_sum(&v);
            eat8(&mut all, s);
            if v.iter().all(|x| x.is_finite()) {
                eat8(&mut finite, s);
                n_finite += 1;
            }
        }
        assert_eq!(n_finite, 35_919);
        assert_eq!(format!("{finite:016x}"), "e683eedf93ff962f");
        assert_eq!(format!("{all:016x}"), "b51f97308b6ab866");
    }

    #[test]
    fn empty_and_all_missing_zones_are_missing_never_zero() {
        let s = statistics_of(&mut [], MISSING, Compat::Engine);
        for v in [
            s.average,
            s.min,
            s.max,
            s.median,
            s.first_quartile,
            s.third_quartile,
            s.pct_gt_zero,
            s.pct_gt_first_quartile,
        ] {
            assert_eq!(v, MISSING);
        }
    }

    #[test]
    fn thresholds_on_valid_cells_match_vortex() {
        let one = statistics_of(&mut [5.0], MISSING, Compat::Engine);
        assert_eq!(
            (one.average, one.min, one.max, one.pct_gt_zero),
            (5.0, 5.0, 5.0, 100.0)
        );
        assert_eq!(one.median, MISSING, "a median needs 2 cells");
        let two = statistics_of(&mut [1.0, 3.0], MISSING, Compat::Engine);
        assert_eq!(two.median, 2.0);
        assert_eq!(two.first_quartile, MISSING, "a quartile needs 4 cells");
        // ZonalStatisticsCalculatorTest.computeMedianTest, and (n+1)/4 with integer division.
        let four = statistics_of(&mut [7.0, 4.0, 6.0, 5.0], MISSING, Compat::Engine);
        assert_eq!(four.median, 5.5);
        assert_eq!(four.first_quartile, 5.0);
        assert_eq!(four.third_quartile, 7.0);
        assert_eq!(four.pct_gt_first_quartile, 50.0);
    }

    #[test]
    fn negative_zero_sorts_below_zero_and_nan_last() {
        let mut v = [0.0, f64::NAN, -0.0, 1.0];
        v.sort_by(java_compare);
        assert_eq!(v[0].to_bits(), (-0.0f64).to_bits());
        assert_eq!(v[1].to_bits(), 0.0f64.to_bits());
        assert!(v[3].is_nan());
    }

    #[test]
    fn e_parts_name_only_their_own_interval() {
        for s in [60, 3600, 21600, 86400, 604800] {
            let e = e_part(s).unwrap();
            let (seconds, _) = write7::regular_e_part(e.as_bytes()).unwrap();
            assert_eq!(seconds, s, "{e}");
        }
        // getEPart's 3Days, 4Days, 6Days, 1Month and 1Year are for other spacings, or none.
        for s in [420, 172800, 345600, 432000, 518400, 2592000, 31536000] {
            assert_eq!(e_part(s), None, "{s}");
        }
    }

    #[test]
    fn accumulation_keeps_the_total_over_missing() {
        assert_eq!(running_totals(&[1.0, MISSING, 2.0]), vec![1.0, 1.0, 3.0]);
    }

    #[test]
    fn a_stored_value_is_judged_after_the_cast() {
        // A double that is not a marker until float32 rounds it to one.
        let near = -901.0 - 1.5e-5;
        assert_ne!(near, -901.0);
        assert_eq!(near as f32, -901.0);
        assert!(stored_value_problem(near, near as f32).is_some());
        for v in [-901.0, -902.0] {
            assert!(stored_value_problem(v, v as f32).is_some(), "{v}");
        }
        // Rounds to -FLT_MAX, without being the missing statistic itself.
        let edge = MISSING * (1.0 - 1e-12);
        assert_eq!(edge as f32, MISSING as f32);
        assert!(stored_value_problem(edge, edge as f32).is_some());
        assert!(
            stored_value_problem(1e300, 1e300f64 as f32).is_some(),
            "out of range"
        );
        assert!(stored_value_problem(f64::NAN, f32::NAN).is_some());
        // The missing statistic, and ordinary neighbours of the markers.
        assert!(stored_value_problem(MISSING, MISSING as f32).is_none());
        for v in [-900.0, -903.0, -901.5, 0.0, -0.0] {
            assert!(stored_value_problem(v, v as f32).is_none(), "{v}");
        }
    }

    #[test]
    fn stamps_read_as_dates() {
        assert_eq!(stamp_text(43890 * 86400 + 3600), "2020-03-01T01:00");
        assert_eq!(stamp_text(43830 * 86400), "2020-01-01T00:00");
        assert_eq!(stamp_text(0), "1899-12-31T00:00");
        assert_eq!(stamp_text(86400 * 61), "1900-03-02T00:00");
    }

    #[test]
    fn an_outcome_says_what_the_blocks_had() {
        let of = |b: &[&'static str]| record_outcome(b.iter().copied());
        assert_eq!(of(&["added", "added"]), "added");
        assert_eq!(of(&["unchanged"]), "unchanged");
        assert_eq!(of(&["replaced"]), "window_updated");
        assert_eq!(of(&["added", "replaced"]), "window_updated");
        assert_eq!(of(&["unchanged", "added"]), "window_updated");
    }
}

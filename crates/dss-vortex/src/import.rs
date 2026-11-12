//! The gridded importer: sources to warp to units to DSS, one frame at a time.
//!
//! Sources, Vortex 1.1.0: `io/BatchImporter.getDataReaders`, `io/ImportableUnit.process`,
//! `geo/GeographicProcessor`, `geo/VectorUtils.getEnvelope`. No Java runs here; Vortex is used
//! only as a test oracle.
//!
//! A run has two phases. **Planning** reads every input's metadata and decides every record before
//! a cell is decoded: pathnames, windows, units, data types, placement, the warp, and the writer's
//! own checks. Anything refused there leaves the output untouched, and does not create it.
//! **Writing** handles one frame at a time. A failure there keeps the records already written, and
//! the writer guarantees no partial record, so [`ImportError`] says what finished and what stopped.
//!
//! **Memory.** One input is in memory at a time, one frame's cells at a time, and one warp plan.
//! The plan has one row per record, and grows with the run; cells do not. Limits are in
//! [`ImportLimits`].
//!
//! Observed behaviour we did not implement, each on Vortex 1.1.0:
//!
//! - **Two records at one pathname.** In `hrrr.t18z.wrfsfcf*` concatenated (2024-09-26), the file-name
//!   rule gives the 0-1, 0-2, 0-3 and 0-4 h totals the same 18:00 to 19:00 window, and the 0-4 h total
//!   is what stays. This importer refuses a run that would write one pathname twice, unless
//!   [`Duplicates::Last`] is stated.
//! - **An input without the variable.** `getDataReaders` skips it with no message. This importer
//!   refuses the run, naming the input.
//! - **A shapefile that does not exist.** `GeographicProcessor` then uses no envelope, and the whole
//!   grid is written. This importer refuses it.

// `ImportError` carries the writer's whole lock report and every finished record. It is built once, on
// the way out of a run, so its size costs nothing that matters.
#![allow(clippy::result_large_err)]

use crate::crs::{CrsDef, DatumPolicy, Envelope};
use crate::error::VortexError;
use crate::frame::{Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts};
use crate::import_netcdf::{self, NetcdfOptions, NetcdfSource};
use crate::products::window::{self, Accumulation};
use crate::products::{self, WindowRule};
use crate::sink_dss::{self, Parts, PlannedRecord, SinkCrs, SinkOptions, WrittenRecord};
use crate::source::grib::{Grib2File, Limits};
use crate::source::raster::{self, RasterFile};
use crate::units::{self, Overrides};
use crate::warp::{Align, Method, WarpEnvelope, WarpLimits, WarpPlan, WarpSpec};
use dss_core::dss7::write::{FaultPoint, WriteError};
use dss_core::grid_write::MAX_WRITE_CELLS;
use dss_core::lock::{self, LockPolicy};
use dss_core::{DssVersion, FileSignature};
use std::collections::HashSet;
use std::fmt;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

/// A flag a caller sets from another thread to stop a run.
pub type StopFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// What a frame is, before its cells are read.
#[derive(Debug, Clone, PartialEq)]
pub struct StepMeta {
    pub time: FrameTime,
    pub units: String,
    pub description: Option<String>,
    /// Destination of the source's own type, which `DssDataWriter` does not use.
    pub source_type: DataType,
}

/// A source's grid, before its cells are read. Two are equal if they state the same grid: any NaN
/// marker means the same thing, as it does for the cells.
#[derive(Debug, Clone)]
pub struct SourceGridDef {
    pub nx: usize,
    pub ny: usize,
    pub transform: GeoTransform,
    pub crs: Crs,
    pub nodata: Option<f32>,
    /// The CRS text a record stores when the grid is not warped to a stated target, where the source
    /// words it differently from [`Self::crs`] and the record's form depends on that wording.
    pub stored_wkt: Option<String>,
}

impl PartialEq for SourceGridDef {
    fn eq(&self, other: &Self) -> bool {
        (self.nx, self.ny) == (other.nx, other.ny)
            && self.transform == other.transform
            && self.crs == other.crs
            && same_marker(self.nodata, other.nodata)
            && self.stored_wkt == other.stored_wkt
    }
}

fn same_marker(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits(),
        (None, None) => true,
        _ => false,
    }
}

/// One opened input. The importer sees sources only through this, so another format plugs in by
/// implementing it and adding an arm to [`open_source`].
pub trait GridSource {
    /// The file's name, without directories. `getCPart` falls back to it.
    fn file_name(&self) -> &str;
    /// Variables the source can give, in a stable order.
    fn variables(&self) -> Vec<String>;
    /// Why other records were left out; refusals, for error messages.
    fn left_out(&self) -> Vec<String> {
        Vec::new()
    }
    fn step_count(&self, variable: &str) -> Result<usize, VortexError>;
    /// Metadata of one step, without decoding it.
    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError>;
    /// The grid of one step, without decoding it.
    fn grid(&self, variable: &str, step: usize) -> Result<SourceGridDef, VortexError>;
    /// Decode one step.
    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError>;
    /// Take only part of the source: a box, a time range, or the cells a destination envelope
    /// needs. Called once a variable is chosen and before its steps are counted, in planning and
    /// again in writing. A source that cannot select refuses a stated box or time range, and
    /// ignores the envelope, which the warp applies anyway.
    fn select(&self, _variable: &str, selection: &SourceSelection) -> Result<(), VortexError> {
        if selection.bbox.is_some() || selection.times.is_some() {
            return Err(VortexError::Refused {
                what: "source selection".into(),
                reason: format!(
                    "{} is not a source that takes bbox or times; they select from a Zarr store",
                    self.file_name()
                ),
            });
        }
        Ok(())
    }
    /// What states an accumulation's length, besides the caller, or `None` for a source a stated
    /// accumulation cannot apply to. See `products::window`.
    fn accumulation_stated_by(
        &self,
        _variable: &str,
        _wanted: Accumulation,
    ) -> Option<Result<&'static str, VortexError>> {
        None
    }
    /// The name a record is named from, where it is not the name a variable is selected by. Vortex
    /// names a record from `VortexGrid.shortName` (`DssDataWriter.getCPart`), and a NetCDF variable
    /// in a group is selected by its full name, path included.
    fn short_name(&self, variable: &str) -> Result<String, VortexError> {
        Ok(variable.to_string())
    }
    /// Whether a variable this source's name makes precipitation must also state a depth, or a rate
    /// that converts to one, to be stored as precipitation by default. See `import_netcdf`.
    fn precipitation_must_be_a_depth(&self) -> bool {
        false
    }
    /// A data type the source states for a step, used when the caller states none, in place of
    /// the one inferred from the variable and its time. `None` (the default) infers it, as Vortex's
    /// `DssDataWriter` does. SNODAS states its products' types under its default timing.
    fn stated_data_type(
        &self,
        _variable: &str,
        _step: usize,
    ) -> Result<Option<DataType>, VortexError> {
        Ok(None)
    }
}

/// What of a source a run reads. `bbox` and `times` are the caller's; the rest is the
/// request's own destination, from which a source may work out the cells the warp needs.
#[derive(Debug, Clone, Default)]
pub struct SourceSelection {
    /// West, south, east, north, in the source's CRS.
    pub bbox: Option<[f64; 4]>,
    /// First and last time, inclusive, in seconds since 1970 UTC. For a period, its end.
    pub times: Option<(i64, i64)>,
    /// The destination envelope, with its CRS, when the request states one.
    pub envelope: Option<WarpEnvelope>,
    /// The destination cell size, in the destination's units.
    pub cell_size: Option<f64>,
}

/// GRIB2 through [`Grib2File`].
pub struct GribSource {
    file: Grib2File,
    variables: Vec<products::Variable>,
    rule: WindowRule,
}

impl GribSource {
    pub fn new(file: Grib2File, rule: WindowRule) -> Self {
        let variables = file.variables();
        GribSource {
            file,
            variables,
            rule,
        }
    }

    fn variable(&self, name: &str) -> Result<&products::Variable, VortexError> {
        self.variables
            .iter()
            .find(|v| v.name == name)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB2 variable {name}"),
                reason: "not in this file, or no product table entry".into(),
            })
    }

    fn record(&self, name: &str, step: usize) -> Result<usize, VortexError> {
        let variable = self.variable(name)?;
        variable
            .steps
            .get(step)
            .map(|s| s.record)
            .ok_or_else(|| VortexError::Refused {
                what: format!("GRIB2 variable {name}"),
                reason: format!("step {step} of {}", variable.steps.len()),
            })
    }
}

impl GridSource for GribSource {
    fn file_name(&self) -> &str {
        self.file.file_name()
    }

    fn variables(&self) -> Vec<String> {
        self.variables.iter().map(|v| v.name.clone()).collect()
    }

    fn left_out(&self) -> Vec<String> {
        self.file
            .unlisted()
            .into_iter()
            .map(|(i, e)| format!("record {i}: {e}"))
            .collect()
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        Ok(self.variable(variable)?.steps.len())
    }

    fn step_meta(&self, name: &str, step: usize) -> Result<StepMeta, VortexError> {
        let variable = self.variable(name)?;
        let window = products::time_window(self.file.file_name(), name, variable, step, self.rule)?;
        Ok(StepMeta {
            time: crate::source::grib::frame_time(&window),
            units: products::units(variable),
            description: Some(variable.description.clone()),
            source_type: products::data_type(variable, &window),
        })
    }

    fn grid(&self, name: &str, step: usize) -> Result<SourceGridDef, VortexError> {
        let record = self.record(name, step)?;
        let g = self.file.georeference(record)?;
        Ok(SourceGridDef {
            nx: g.nx,
            ny: g.ny,
            transform: GeoTransform::axis_aligned(g.origin_x, g.dx, g.origin_y, g.dy),
            crs: Crs::Wkt(g.wkt),
            nodata: Some(self.variable(name)?.entry.missing),
            stored_wkt: None,
        })
    }

    fn frame(&self, name: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.file.frame_of(self.variable(name)?, step, self.rule)
    }

    fn accumulation_stated_by(
        &self,
        variable: &str,
        wanted: Accumulation,
    ) -> Option<Result<&'static str, VortexError>> {
        Some(window::stated_by(self.file.file_name(), variable, wanted))
    }
}

/// One GeoTIFF or ASCII grid through [`RasterFile`]: one variable and one step, named from the file
/// name as Vortex's `AscDataReader` does. The reader has no metadata-only path, so planning decodes
/// the file too; one input is still in memory at a time.
/// The marker a raster with no stated nodata is imported under, from Vortex 1.1.0 and 0.13.3's
/// `io/AscDataReader.getDto` (`GetNoDataValue`, else `-9999.0`). `geo/RasterUtils.getDatasetFromVortexGrid`
/// sets it on the dataset it warps, `geo/Resampler` passes it as `-srcnodata`, `gdalwarp` copies that to
/// the destination, and `INIT_DEST=NO_DATA` fills what the source does not reach with it.
/// `DssDataWriter.write` then stores those cells as missing, as it does a -9999 cell of an unwarped file.
const RASTER_DEFAULT_MARKER: f32 = -9999.0;

/// [`RasterFile`] keeps what the file states, `None` here, as `rasterio` reads it. Only the importer
/// applies Vortex's default.
fn raster_marker(stated: Option<f32>) -> f32 {
    stated.unwrap_or(RASTER_DEFAULT_MARKER)
}

pub struct RasterSource {
    file_name: String,
    file: RasterFile,
}

impl RasterSource {
    pub fn new(file_name: String, file: RasterFile) -> Self {
        RasterSource { file_name, file }
    }

    fn check(&self, variable: &str, step: usize) -> Result<(), VortexError> {
        let parts = self.file.frame().parts();
        if variable == parts.variable && step == 0 {
            Ok(())
        } else {
            Err(VortexError::Refused {
                what: format!("variable {variable}, step {step}"),
                reason: format!("this raster holds {}, one step", parts.variable),
            })
        }
    }
}

impl GridSource for RasterSource {
    fn file_name(&self) -> &str {
        &self.file_name
    }

    fn variables(&self) -> Vec<String> {
        vec![self.file.frame().parts().variable.clone()]
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        self.check(variable, 0)?;
        Ok(1)
    }

    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError> {
        self.check(variable, step)?;
        let p = self.file.frame().parts();
        Ok(StepMeta {
            time: p.time,
            units: p.units.clone(),
            description: p.description.clone(),
            source_type: p.data_type,
        })
    }

    fn grid(&self, variable: &str, step: usize) -> Result<SourceGridDef, VortexError> {
        self.check(variable, step)?;
        let p = self.file.frame().parts();
        Ok(SourceGridDef {
            nx: p.nx,
            ny: p.ny,
            transform: p.transform,
            crs: p.crs.clone(),
            nodata: Some(raster_marker(p.nodata)),
            stored_wkt: self.file.stored_wkt().map(str::to_string),
        })
    }

    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.check(variable, step)?;
        let p = self.file.frame().parts();
        // A failed allocation must be an error: `clone` would abort the process.
        let mut cells: Vec<f32> = Vec::new();
        cells
            .try_reserve_exact(p.cells.len())
            .map_err(|_| VortexError::Refused {
                what: "raster".into(),
                reason: format!("cannot hold a frame of {} cells", p.cells.len()),
            })?;
        cells.extend_from_slice(&p.cells);
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx: p.nx,
            ny: p.ny,
            row_order: p.row_order,
            transform: p.transform,
            crs: p.crs.clone(),
            nodata: Some(raster_marker(p.nodata)),
            time: p.time,
            data_type: p.data_type,
            units: p.units.clone(),
            variable: p.variable.clone(),
            description: p.description.clone(),
        })?)
    }
}

/// How to read inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenOptions {
    pub window_rule: WindowRule,
    pub limits: SourceLimits,
    /// What a caller set on the NetCDF reader. Other formats ignore it.
    pub netcdf: NetcdfOptions,
    /// P71. Where SNODAS grids take their times and corners from. Other formats ignore it.
    pub snodas: crate::source::snodas::SnodasOptions,
}

/// Limits every source takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLimits {
    pub max_cells: usize,
    pub max_input_bytes: u64,
    pub max_records: usize,
    /// P71: what one ZIP archive's rasters may hold together; see [`ImportLimits::max_archive_decoded_bytes`].
    pub archive: crate::source::zipped::ArchiveBudget,
}

impl Default for SourceLimits {
    fn default() -> Self {
        SourceLimits {
            max_cells: ImportLimits::default().max_source_cells,
            max_input_bytes: ImportLimits::default().max_input_bytes,
            max_records: Limits::default().max_records,
            archive: ImportLimits::default().archive_budget(),
        }
    }
}

/// Open one input by its content. Format detection is by the file's first bytes, not its name, so a
/// renamed file cannot change the reader; Vortex's name-based window rules stay in the product table.
/// An ASCII grid is text with no signature, so it is the one format chosen by name, and only after the
/// other formats' bytes have been ruled out.
pub fn open_source(path: &Path, options: &OpenOptions) -> Result<Box<dyn GridSource>, VortexError> {
    open_source_with_stop(path, options, None)
}

/// [`open_source`], with a stop the caller holds. A NetCDF read in progress ends when it is asked;
/// the GRIB2 and raster readers hold a file whole and do not look at it. A Zarr store opened here
/// takes its default limits and no stop: `import_gridded` opens one with the request's own.
/// Whether a file named as a SNODAS member holds what that name says: gzip for `.dat.gz`, and for
/// `.dat` exactly the masked grid's 2-byte cells.
fn snodas_bytes_agree(path: &Path, name: &str, head: &[u8]) -> Result<bool, VortexError> {
    if name.to_ascii_lowercase().ends_with(".gz") {
        return Ok(crate::source::archive::is_gzip(head));
    }
    let grid = (crate::source::snodas::NCOLS * crate::source::snodas::NROWS * 2) as u64;
    Ok(std::fs::metadata(path)?.len() == grid)
}

pub fn open_source_with_stop(
    path: &Path,
    options: &OpenOptions,
    stop: Option<&StopFlag>,
) -> Result<Box<dyn GridSource>, VortexError> {
    // P49: a Zarr store is a folder, which has no first bytes to tell it by.
    if path.is_dir() {
        return crate::source::zarr::open_source(path, options);
    }
    // WPC's files start with a bulletin header before `GRIB`.
    let mut head = [0u8; 4096];
    let read = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut n = 0;
        while n < head.len() {
            let k = f.read(&mut head[n..])?;
            if k == 0 {
                break;
            }
            n += k;
        }
        n
    };
    let head = &head[..read];
    let raster_options = raster::ReadOptions {
        limits: raster::Limits {
            max_cells: options.limits.max_cells,
            max_input_bytes: options.limits.max_input_bytes,
        },
        ..raster::ReadOptions::default()
    };
    let open_raster = || -> Result<Box<dyn GridSource>, VortexError> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = RasterFile::open_with(path, raster_options)?;
        Ok(Box::new(RasterSource::new(file_name, file)))
    };
    // First: a TIFF's magic is at its first byte, and its tags could hold the text `GRIB`.
    if raster::geotiff::has_magic(head) {
        return open_raster();
    }
    let open_netcdf = || -> Result<Box<dyn GridSource>, VortexError> {
        let limits = options.netcdf.limits(&options.limits);
        let source = NetcdfSource::open(path, limits, options.window_rule, stop)?;
        Ok(Box::new(source))
    };
    // Before GRIB: a NetCDF file made from GRIB can hold the text `GRIB` in its attributes, and its
    // own magic is at its first byte, where no GRIB2 file or bulletin header has it.
    if import_netcdf::has_magic(head) {
        return open_netcdf();
    }
    // P71: archives have their signature at the first byte, and may hold any text after it, so
    // they come before the search for `GRIB`. A tar's signature is at byte 257.
    if crate::source::archive::is_zip(head) {
        return crate::import_archive::open_zip(path, options, stop);
    }
    if crate::source::archive::is_tar(head) {
        return crate::import_archive::open_snodas_tar(path, options);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A SNODAS member is chosen by its name only where its bytes agree, so a renamed file of
    // another format still reaches that format's reader: a `.dat.gz` is gzip, which
    // the search for `GRIB` below cannot read through, and a `.dat` is exactly the masked grid.
    let snodas_name = crate::source::snodas::is_snodas_name(&name);
    if snodas_name && snodas_bytes_agree(path, &name, head)? {
        return crate::import_archive::open_snodas_member(path, options);
    }
    if head.windows(4).any(|w| w == b"GRIB") {
        let limits = Limits {
            max_cells: options.limits.max_cells,
            max_input_bytes: options.limits.max_input_bytes,
            max_records: options.limits.max_records,
            ..Limits::default()
        };
        // P73: the edition octet of the first message chooses the reader. Each refuses a file that
        // also holds the other edition.
        if crate::source::grib1::scan::first_edition(head) == Some(1) {
            let file = crate::source::grib1::Grib1File::open_with_limits(path, limits)?;
            return Ok(Box::new(crate::source::grib1::Grib1Source::new(
                file,
                options.window_rule,
            )));
        }
        let file = Grib2File::open_with_limits(path, limits)?;
        return Ok(Box::new(GribSource::new(file, options.window_rule)));
    }
    let named_asc = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("asc"));
    if named_asc {
        return open_raster();
    }
    // An HDF5 signature after a user block is content, so it comes before the readers chosen by
    // name.
    if import_netcdf::has_offset_magic(head) {
        return open_netcdf();
    }
    // P71: BIL has no signature; its `.hdr` beside it says what it is.
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bil"))
    {
        return crate::import_archive::open_bil(path, options);
    }
    // A SNODAS name whose bytes are neither gzip nor the masked grid: its reader states why.
    if snodas_name {
        return crate::import_archive::open_snodas_member(path, options);
    }
    Err(VortexError::Unsupported {
        what: format!(
            "{}: no reader recognises this file. GRIB2, NetCDF, HDF5, GeoTIFF, ASCII grids, BIL, \
             ZIP archives of those, and SNODAS tars and members are read today; another format \
             plugs in through import::GridSource",
            path.display()
        ),
    })
}

/// What to take from each input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The variable's exact name, as [`GridSource::variables`] lists it.
    Variable(String),
    /// A named precipitation product. It must match exactly one variable in each input.
    Product(Product),
}

/// Explicit product names (`VORTEX_SCOPE.md` section 6: an argument, not file-name guessing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    /// MRMS quantitative precipitation estimates: variables holding `QPE`.
    MrmsQpe,
    /// HRRR, Stage IV and WPC QPF: `Total_precipitation_surface_*`.
    TotalPrecipitation,
}

impl Product {
    fn matches(self, name: &str) -> bool {
        match self {
            Product::MrmsQpe => name.to_lowercase().contains("qpe"),
            Product::TotalPrecipitation => name.starts_with("Total_precipitation_surface_"),
        }
    }
}

fn select(source: &dyn GridSource, selection: &Selection) -> Result<String, VortexError> {
    let names = source.variables();
    let hint = || {
        let left_out = source.left_out();
        if left_out.is_empty() {
            format!("the file lists {names:?}")
        } else {
            format!(
                "the file lists {names:?}, and left out {} record(s), first: {}",
                left_out.len(),
                left_out[0]
            )
        }
    };
    match selection {
        Selection::Variable(name) => {
            if names.iter().any(|n| n == name) {
                Ok(name.clone())
            } else {
                Err(VortexError::Refused {
                    what: format!("variable {name}"),
                    reason: format!(
                        "not in this input; {}. Observed behaviour we did not implement: Vortex \
                         skips such an input with no message",
                        hint()
                    ),
                })
            }
        }
        Selection::Product(product) => {
            let found: Vec<&String> = names.iter().filter(|n| product.matches(n)).collect();
            match found.as_slice() {
                [one] => Ok((*one).clone()),
                [] => Err(VortexError::Refused {
                    what: format!("product {product:?}"),
                    reason: format!("no variable matches; {}", hint()),
                }),
                many => Err(VortexError::Refused {
                    what: format!("product {product:?}"),
                    reason: format!("{} variables match: {many:?}. Name one", many.len()),
                }),
            }
        }
    }
}

// Longest `.prj` read. Far over any WKT1 CRS; `CrsDef::parse_wkt` has its own, shorter limit.
const MAX_PRJ_BYTES: u64 = 1 << 20;

/// `VectorUtils.getEnvelope` and `getWkt`: the union of every feature's points, and the `.prj`
/// beside the file, which must exist. Malformed input is a typed error.
pub fn shapefile_envelope(path: &Path) -> Result<(Envelope, CrsDef), VortexError> {
    let refuse = |reason: String| VortexError::Refused {
        what: format!("shapefile {}", path.display()),
        reason,
    };
    if !path.exists() {
        return Err(refuse(
            "it does not exist. Observed behaviour we did not implement: Vortex then uses no \
             envelope and writes the whole grid"
                .into(),
        ));
    }
    let prj = path.with_extension("prj");
    if !prj.exists() {
        return Err(refuse(
            "it has no .prj, so the CRS of its coordinates is not known. State an envelope and its \
             CRS instead"
                .into(),
        ));
    }
    let owned = path.to_path_buf();
    // `shapefile` is a third-party parser on untrusted bytes. Its shapes are read one at a time, and
    // `ShapeReader::new` leaves the `.shx` unread: only the union of the points is needed.
    let scanned = std::panic::catch_unwind(move || -> Result<Option<Envelope>, VortexError> {
        let decode = |e: shapefile::Error| VortexError::Decode {
            format: "shapefile",
            message: e.to_string(),
        };
        let file = std::io::BufReader::new(std::fs::File::open(&owned)?);
        let mut reader = shapefile::ShapeReader::new(file).map_err(decode)?;
        let mut envelope = None;
        for shape in reader.iter_shapes() {
            add_shape(&mut envelope, &shape.map_err(decode)?).map_err(|t| {
                VortexError::Refused {
                    what: format!("shapefile {}", owned.display()),
                    reason: format!("shape type {t} has no reference result"),
                }
            })?;
        }
        Ok(envelope)
    })
    .map_err(|_| refuse("the parser could not read it".into()))??;
    let envelope = scanned.ok_or_else(|| refuse("it holds no shapes".into()))?;
    if ![
        envelope.min_x,
        envelope.max_x,
        envelope.min_y,
        envelope.max_y,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return Err(refuse("its coordinates are not finite".into()));
    }
    // A CRS's text is short, and reading one must not hold what a hostile file declares.
    if std::fs::metadata(&prj)?.len() > MAX_PRJ_BYTES {
        return Err(refuse(format!("its .prj is over {MAX_PRJ_BYTES} bytes")));
    }
    let text = std::fs::read_to_string(&prj)?;
    let (metric, factor) = in_metres(text.trim());
    let mut crs = CrsDef::parse_wkt(&metric)?;
    if factor == 1.0 {
        return Ok((envelope, crs));
    }
    scale_false_origin(&mut crs, factor);
    let scaled = Envelope {
        min_x: envelope.min_x * factor,
        max_x: envelope.max_x * factor,
        min_y: envelope.min_y * factor,
        max_y: envelope.max_y * factor,
    };
    Ok((scaled, crs))
}

// Grow an envelope by one shape's points. `Err` holds a shape type with no reference result.
fn add_shape(envelope: &mut Option<Envelope>, shape: &shapefile::Shape) -> Result<(), String> {
    let mut add = |x: f64, y: f64| {
        let b = envelope.get_or_insert(Envelope {
            min_x: x,
            max_x: x,
            min_y: y,
            max_y: y,
        });
        b.min_x = b.min_x.min(x);
        b.max_x = b.max_x.max(x);
        b.min_y = b.min_y.min(y);
        b.max_y = b.max_y.max(y);
    };
    use shapefile::Shape as S;
    match shape {
        S::Point(p) => add(p.x, p.y),
        S::PointM(p) => add(p.x, p.y),
        S::PointZ(p) => add(p.x, p.y),
        S::Polyline(s) => s.parts().iter().flatten().for_each(|p| add(p.x, p.y)),
        S::PolylineM(s) => s.parts().iter().flatten().for_each(|p| add(p.x, p.y)),
        S::PolylineZ(s) => s.parts().iter().flatten().for_each(|p| add(p.x, p.y)),
        S::Polygon(s) => s
            .rings()
            .iter()
            .flat_map(|r| r.points())
            .for_each(|p| add(p.x, p.y)),
        S::PolygonM(s) => s
            .rings()
            .iter()
            .flat_map(|r| r.points())
            .for_each(|p| add(p.x, p.y)),
        S::PolygonZ(s) => s
            .rings()
            .iter()
            .flat_map(|r| r.points())
            .for_each(|p| add(p.x, p.y)),
        S::Multipoint(s) => s.points().iter().for_each(|p| add(p.x, p.y)),
        S::MultipointM(s) => s.points().iter().for_each(|p| add(p.x, p.y)),
        S::MultipointZ(s) => s.points().iter().for_each(|p| add(p.x, p.y)),
        other => return Err(other.shapetype().to_string()),
    }
    Ok(())
}

/// A projected CRS's outermost linear unit, as the metres per unit. The CRS module takes metres
/// only, so a shapefile in feet is converted here: `pj_inv` starts with `x * to_meter - x0`.
pub(crate) fn in_metres(text: &str) -> (String, f64) {
    let is_projected = text
        .trim_start()
        .get(..6)
        .is_some_and(|h| h.eq_ignore_ascii_case("PROJCS"));
    let clause = text.rfind("UNIT[").and_then(|at| {
        let end = at + text[at..].find(']')?;
        Some((at, end))
    });
    let (Some((at, end)), true) = (clause, is_projected) else {
        return (text.to_string(), 1.0);
    };
    // Transverse Mercator carries its unit in the definition, where the transformation applies it
    // as PROJ does: `unitconvert`'s own factor, and the false origin in metres at 15 digits.
    if text
        .to_ascii_lowercase()
        .contains("projection[\"transverse_mercator\"")
    {
        return (text.to_string(), 1.0);
    }
    let factor = text[at..end]
        .rsplit(',')
        .next()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0);
    match factor {
        Some(f) if f != 1.0 => (
            format!("{}UNIT[\"Meter\",1{}", &text[..at], &text[end..]),
            f,
        ),
        _ => (text.to_string(), 1.0),
    }
}

// WKT1 states false easting and northing in the CRS's linear unit.
pub(crate) fn scale_false_origin(crs: &mut CrsDef, factor: f64) {
    use crate::crs::Projection as P;
    match &mut crs.projection {
        P::LongLat | P::WebMercator => {}
        // `in_metres` leaves Transverse Mercator's text as it is. Text it did not recognise as
        // one arrives here stated in metres with its false origin still in the file's unit.
        P::TransverseMercator { x_0, y_0, .. }
        | P::Albers { x_0, y_0, .. }
        | P::Lambert { x_0, y_0, .. }
        | P::Stereographic { x_0, y_0, .. } => {
            *x_0 *= factor;
            *y_0 *= factor;
        }
    }
}

/// Where the grid is clipped.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Bounds {
    #[default]
    None,
    /// An envelope, in `crs`. `None` means the target's CRS, or with no target, each source's own.
    Envelope {
        envelope: Envelope,
        crs: Option<Crs>,
    },
    /// A shapefile's bounding box only. Vortex has no polygon mask.
    Shapefile(PathBuf),
}

/// What to do when a run would write one pathname twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Duplicates {
    /// Refuse, before anything is written.
    #[default]
    Refuse,
    /// Keep the last, as Vortex's writes do within one input.
    Last,
}

/// Bounds on one run. Each is checked while planning, before any cell is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportLimits {
    /// Most cells in a source grid. 2^26: about 2.7 times MRMS (24.5 million), the largest sampled
    /// product, and a quarter of the GRIB2 reader's own default, because a small file can declare a
    /// constant field of any size and each decode holds about 13 bytes per cell.
    pub max_source_cells: usize,
    /// Most cells in a destination grid: the P24 writer's limit, 2^24. The warp's own default of
    /// 2^28 would decode and warp up to 1 GiB per frame and then be refused by the writer, so this
    /// is the warp's bound during an import, and it is refused before any cell is decoded.
    pub max_destination_cells: u64,
    /// What one warp may hold, bytes. 2^30: the destination at most 64 MiB, and GDAL's chunking
    /// keeps a chunk's source window small; the sampled products need well under it.
    pub max_warp_bytes: u64,
    /// Largest input file, which is held in memory while it is read, one input at a time. 2^32,
    /// and not the GRIB2 reader's 2^34: the largest sampled file is well under it. A NetCDF file is
    /// read by range and not held, so this does not apply to it: its reader's own limit does
    /// (`source::netcdf::Limits::max_input_bytes`), and what one read may cost is in
    /// [`ImportRequest::netcdf`].
    pub max_input_bytes: u64,
    /// Most records in one run. The plan holds one row per record, whose pathname and names are a few
    /// hundred bytes, so this is what bounds its growth: 2^20 is 119 years of hourly grids, and under
    /// 1 GiB of plan.
    pub max_records: usize,
    /// P71: most bytes of decoded cells (4 per cell) kept from one ZIP archive, whose rasters are
    /// all decoded when it is opened. 2^30. Checked before each raster is decoded.
    pub max_archive_decoded_bytes: u64,
    /// P71: most rasters read from one ZIP archive, checked before any is read. 256; a PRISM
    /// package holds one.
    pub max_archive_rasters: usize,
}

impl ImportLimits {
    /// The aggregate bounds on one ZIP archive.
    pub fn archive_budget(&self) -> crate::source::zipped::ArchiveBudget {
        crate::source::zipped::ArchiveBudget {
            max_decoded_bytes: self.max_archive_decoded_bytes,
            max_rasters: self.max_archive_rasters,
        }
    }
}

impl Default for ImportLimits {
    fn default() -> Self {
        ImportLimits {
            max_source_cells: 1 << 26,
            max_destination_cells: MAX_WRITE_CELLS as u64,
            max_warp_bytes: 1 << 30,
            max_input_bytes: 1 << 32,
            max_records: 1 << 20,
            max_archive_decoded_bytes: crate::source::zipped::ArchiveBudget::default()
                .max_decoded_bytes,
            max_archive_rasters: crate::source::zipped::ArchiveBudget::default().max_rasters,
        }
    }
}

/// One import.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub selection: Selection,
    pub bounds: Bounds,
    /// `None` keeps each source's CRS. With no target, cell size or bounds, frames are not warped
    /// (`GeographicProcessor`, `isEmpty`).
    pub target: Option<Crs>,
    pub cell_size: Option<f64>,
    pub method: Method,
    pub parts: ImportParts,
    /// Relabels the units in the record; does not convert (`DssDataWriter.write`).
    pub units: Option<String>,
    pub data_type: Option<DataType>,
    /// `None` keeps an existing file's version, and creates DSS7.
    pub version: Option<u8>,
    pub align: Align,
    pub window_rule: WindowRule,
    pub duplicates: Duplicates,
    /// Warp workers. Default [`crate::warp::host_threads`], all cores; the
    /// written records do not depend on it.
    pub threads: usize,
    pub limits: ImportLimits,
    /// Flush policy of the run: `End`, the default, flushes once after the last
    /// record; `Step` flushes at every flush point of every record; `None` never.
    pub sync: dss_core::dss7::write::SyncMode,
    pub allow_undetectable_readers: bool,
    /// Limits on the NetCDF reader: a read's time, the bytes it may decode, and the chunk cache.
    pub netcdf: NetcdfOptions,
    /// Where SNODAS grids take their times, data types and corners from; the default
    /// is the checked header's times with the product's data type, and Vortex's corners.
    pub snodas: crate::source::snodas::SnodasOptions,
    /// Crash simulation at this record of the run, for tests.
    #[doc(hidden)]
    pub fault: Option<(usize, FaultPoint)>,
    /// P49. Read only this box of each source: west, south, east, north in the source's CRS. For
    /// a Zarr store; other sources refuse it. With none, a Zarr store reads the cells the
    /// request's envelope needs, padded for the warp, and its whole grid when there is no envelope.
    pub source_bbox: Option<[f64; 4]>,
    /// P49. Read only steps whose time is in this range, inclusive, seconds since 1970 UTC.
    pub source_times: Option<(i64, i64)>,
    /// P49. Limits for Zarr inputs, in place of the defaults.
    pub zarr_limits: Option<crate::source::zarr::Limits>,
    /// P49 and P46b. A stop the caller keeps a clone of ([`crate::import_netcdf::StopHandle::flag`]
    /// gives one). Set from another thread, it ends the run before its next frame, and a Zarr or
    /// NetCDF read in progress before its next chunk; for NetCDF, also before each filter stage.
    /// On a file input the run then fails as [`Failure::Stopped`]; on a Zarr store the read in
    /// progress is refused by the store itself. Either way `completed` holds the records
    /// already written.
    pub stop: Option<StopFlag>,
}

impl ImportRequest {
    pub fn new(
        inputs: Vec<PathBuf>,
        output: PathBuf,
        selection: Selection,
        method: Method,
    ) -> Self {
        ImportRequest {
            inputs,
            output,
            selection,
            bounds: Bounds::None,
            target: None,
            cell_size: None,
            method,
            parts: ImportParts::default(),
            units: None,
            data_type: None,
            version: None,
            align: Align::default(),
            window_rule: WindowRule::default(),
            duplicates: Duplicates::default(),
            // All cores by default; an explicit `threads` (1 included) wins.
            threads: crate::warp::host_threads(),
            limits: ImportLimits::default(),
            sync: dss_core::dss7::write::SyncMode::End,
            allow_undetectable_readers: true,
            netcdf: NetcdfOptions::default(),
            snodas: crate::source::snodas::SnodasOptions::default(),
            fault: None,
            source_bbox: None,
            source_times: None,
            zarr_limits: None,
            stop: None,
        }
    }
}

/// Stated pathname parts. D and E always come from each frame's window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportParts {
    pub a: String,
    pub b: String,
    pub c: Option<String>,
    pub f: String,
}

/// Progress after one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub input: usize,
    pub variable: String,
    pub step: usize,
    pub pathname: String,
}

/// One record written, and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedRecord {
    pub input: PathBuf,
    pub variable: String,
    pub step: usize,
    pub record: WrittenRecord,
    /// Set under [`WindowRule::Accumulation`] only.
    pub window: Option<WindowNote>,
}

/// What a stated accumulation means for one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowNote {
    pub seconds: i64,
    /// `caller` when nothing in the file states a duration; otherwise `variable`, `file_name`, or
    /// `variable+file_name`, each of which agrees with the caller.
    pub stated_by: &'static str,
    /// This record's period overlaps one that ends earlier in this run, in the same A, B, C and F
    /// parts. Each keeps its own D and E parts, so all are stored, and a consumer summing them counts
    /// the shared rain more than once. Records already in the file are not looked at.
    pub overlaps_earlier: bool,
    /// Time passes between the previous period in this run and this one, and a sum would miss it.
    pub gap_before: bool,
}

/// Why a run stopped.
#[derive(Debug)]
pub enum Failure {
    /// A source, warp or rule refused or failed.
    Vortex(VortexError),
    /// The DSS writer refused or failed, including a locked file.
    Write(WriteError),
    /// The progress callback asked to stop.
    Cancelled,
    /// The caller's stop flag ([`ImportRequest::stop`]) asked to stop.
    Stopped,
    /// A request's own arguments are wrong.
    BadRequest(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Vortex(e) => write!(f, "{e}"),
            Failure::Write(e) => write!(f, "{e}"),
            Failure::Cancelled => write!(f, "cancelled by the progress callback"),
            Failure::Stopped => write!(f, "stopped by the caller's stop handle"),
            Failure::BadRequest(m) => write!(f, "{m}"),
        }
    }
}

/// Which phase, input and frame stopped a run, and what had finished.
#[derive(Debug)]
pub struct ImportError {
    pub failure: Failure,
    /// `planning` or `writing`.
    pub phase: &'static str,
    pub input: Option<PathBuf>,
    pub variable: Option<String>,
    pub step: Option<usize>,
    /// Records the writer acknowledged before the run stopped. They remain in the file. This is not
    /// every record the file may now hold: see `in_doubt`.
    pub completed: Vec<ImportedRecord>,
    /// The pathname being written when the writer failed, in the form planned. Its state is unknown:
    /// the writer leaves no torn record, but it may have published this one whole before it failed
    /// (seen on a fault during the last steps of a write). An error is no proof it was not written.
    /// Read the file, or run again, which replaces it. `None` when no write of a record had begun.
    pub in_doubt: Option<String>,
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "import stopped while {}", self.phase)?;
        if let Some(input) = &self.input {
            write!(f, " at input {}", input.display())?;
        }
        if let Some(variable) = &self.variable {
            write!(f, ", variable {variable}")?;
        }
        if let Some(step) = self.step {
            write!(f, ", frame {step}")?;
        }
        write!(f, ": {}", self.failure)?;
        write!(
            f,
            " ({} record(s) already written remain)",
            self.completed.len()
        )?;
        if let Some(pathname) = &self.in_doubt {
            write!(
                f,
                "; {pathname} was being written, and may or may not be in the file: read the file, \
                 or run again, which replaces it"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ImportError {}

impl ImportError {
    fn planning(failure: Failure) -> Self {
        ImportError {
            failure,
            phase: "planning",
            input: None,
            variable: None,
            step: None,
            completed: Vec::new(),
            in_doubt: None,
        }
    }

    fn at(mut self, input: &Path, variable: Option<&str>, step: Option<usize>) -> Self {
        self.input = Some(input.to_path_buf());
        self.variable = variable.map(str::to_string);
        self.step = step;
        self
    }
}

impl From<VortexError> for ImportError {
    fn from(e: VortexError) -> Self {
        ImportError::planning(Failure::Vortex(e))
    }
}

// One planned frame.
struct Item {
    input: usize,
    variable: String,
    step: usize,
    planned: PlannedRecord,
    time: FrameTime,
    source_units: String,
    description: Option<String>,
    // The grid planned for it, as an index into the plan's grids.
    grid: usize,
    // Whether a later record in the run replaces this one.
    superseded: bool,
    window: Option<WindowNote>,
}

// The target as `WarpSpec` wants it, and the text the sink stores.
struct Geo {
    target: Option<CrsDef>,
    sink_crs: Option<SinkCrs>,
    envelope: Option<WarpEnvelope>,
}

fn prepare_geo(request: &ImportRequest) -> Result<Geo, VortexError> {
    let (target, sink_crs) = match &request.target {
        None => (None, None),
        Some(crs) => (
            Some(CrsDef::from_frame(crs)?),
            Some(SinkCrs::of_frame(crs)?),
        ),
    };
    let envelope = match &request.bounds {
        Bounds::None => None,
        Bounds::Envelope { envelope, crs } => Some(WarpEnvelope {
            envelope: *envelope,
            crs: match (crs, &target) {
                (Some(c), _) => Some(CrsDef::from_frame(c)?),
                (None, Some(t)) => Some(t.clone()),
                (None, None) => None,
            },
        }),
        Bounds::Shapefile(path) => {
            let (envelope, crs) = shapefile_envelope(path)?;
            Some(WarpEnvelope {
                envelope,
                crs: Some(crs),
            })
        }
    };
    if let Some(e) = &envelope {
        let b = e.envelope;
        if !(b.min_x < b.max_x && b.min_y < b.max_y) {
            return Err(VortexError::Refused {
                what: "envelope".into(),
                reason: format!("{b:?} is empty"),
            });
        }
    }
    Ok(Geo {
        target,
        sink_crs,
        envelope,
    })
}

// `GeographicProcessor.process`, `isEmpty`.
fn wants_warp(request: &ImportRequest, geo: &Geo) -> bool {
    geo.target.is_some() || request.cell_size.is_some() || geo.envelope.is_some()
}

fn warp_spec(request: &ImportRequest, geo: &Geo) -> WarpSpec {
    WarpSpec {
        target: geo.target.clone(),
        cell_size: request.cell_size,
        envelope: geo.envelope.clone(),
        method: request.method,
        align: request.align,
        datum_policy: DatumPolicy::default(),
        threads: request.threads,
        limits: WarpLimits {
            max_cells: request.limits.max_destination_cells,
            max_memory_bytes: request.limits.max_warp_bytes,
            ..WarpLimits::default()
        },
    }
}

fn plan_for(def: &SourceGridDef, spec: &WarpSpec) -> Result<WarpPlan, VortexError> {
    WarpPlan::new(
        &crate::warp::SourceGrid {
            nx: def.nx,
            ny: def.ny,
            transform: def.transform,
            crs: CrsDef::from_frame(&def.crs)?,
        },
        spec,
    )
}

// What a destination looks like, for placement and the sink's checks.
struct Destination {
    plan: Option<WarpPlan>,
    source: SourceGridDef,
    transform: GeoTransform,
    ny: usize,
    sink_crs: SinkCrs,
}

fn destination(
    request: &ImportRequest,
    geo: &Geo,
    def: &SourceGridDef,
    keep_plan: bool,
) -> Result<Destination, VortexError> {
    if wants_warp(request, geo) {
        // `WarpPlan::warp_frame` needs a marker, and would otherwise find this only while writing. A
        // raster always has one (`raster_marker`); this is for a source with no rule of its own.
        if def.nodata.is_none() {
            return Err(VortexError::Refused {
                what: "warp".into(),
                reason: "the source states no nodata marker, and no warp is qualified without one"
                    .into(),
            });
        }
        let plan = plan_for(def, &warp_spec(request, geo))?;
        let g = plan.geotransform();
        let transform = GeoTransform {
            origin_x: g[0],
            dx: g[1],
            row_rotation: g[2],
            origin_y: g[3],
            column_rotation: g[4],
            dy: g[5],
        };
        let ny = plan.ny();
        let sink_crs = sink_crs_for(geo, def)?;
        Ok(Destination {
            plan: keep_plan.then_some(plan),
            source: def.clone(),
            transform,
            ny,
            sink_crs,
        })
    } else {
        let cells = def.nx.saturating_mul(def.ny) as u64;
        if cells > request.limits.max_destination_cells {
            return Err(VortexError::Refused {
                what: "DSS grid".into(),
                reason: format!(
                    "{} x {} is over the writer's limit of {} cells; state a target, cell size \
                     and envelope",
                    def.nx, def.ny, request.limits.max_destination_cells
                ),
            });
        }
        Ok(Destination {
            plan: None,
            source: def.clone(),
            transform: def.transform,
            ny: def.ny,
            sink_crs: sink_crs_for(geo, def)?,
        })
    }
}

pub(crate) fn existing_version(path: &Path) -> Result<Option<u8>, Failure> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Failure::Vortex(e.into())),
    };
    let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
    let mut n = 0;
    loop {
        let k = file
            .read(&mut head[n..])
            .map_err(|e| Failure::Vortex(e.into()))?;
        if k == 0 {
            break;
        }
        n += k;
        if n == head.len() {
            break;
        }
    }
    head.truncate(n);
    let sig = FileSignature::from_bytes(&head).map_err(|e| {
        Failure::Vortex(VortexError::Refused {
            what: format!("output {}", path.display()),
            reason: format!("it exists and is not a DSS file: {e}"),
        })
    })?;
    Ok(Some(match sig.version {
        DssVersion::V6 => 6,
        DssVersion::V7 => 7,
    }))
}

// The lock guard, up front, so a held file fails before any decoding. The writer checks again.
fn check_lock(path: &Path, request: &ImportRequest) -> Result<(), Failure> {
    let report = lock::lock_status(path).map_err(|e| Failure::Write(WriteError::Lock(e)))?;
    let policy = LockPolicy {
        allow_undetectable_readers: request.allow_undetectable_readers,
        ..LockPolicy::default()
    };
    lock::ensure_unlocked_for_write(&report, &policy)
        .map(|_| ())
        .map_err(|r| Failure::Write(WriteError::Locked(r)))
}

fn plan_output(request: &ImportRequest) -> Result<(u8, bool), ImportError> {
    let bad = |m: String| ImportError::planning(Failure::BadRequest(m));
    if !matches!(request.version, None | Some(6) | Some(7)) {
        return Err(bad(format!(
            "version must be 6 or 7, not {:?}",
            request.version
        )));
    }
    let existing = existing_version(&request.output).map_err(ImportError::planning)?;
    let version = match (existing, request.version) {
        (Some(have), Some(want)) if have != want => {
            return Err(ImportError::planning(Failure::Vortex(
                VortexError::Refused {
                    what: format!("output {}", request.output.display()),
                    reason: format!(
                        "it is DSS{have}, and version {want} was asked for. Writers keep a file's \
                     version"
                    ),
                },
            )))
        }
        (Some(have), _) => have,
        (None, Some(want)) => want,
        (None, None) => 7,
    };
    if existing.is_some() {
        check_lock(&request.output, request).map_err(ImportError::planning)?;
    } else if let Some(parent) = request
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
    Ok((version, existing.is_some()))
}

fn name_step(
    request: &ImportRequest,
    source_file: &str,
    variable: &str,
    meta: &StepMeta,
    version: u8,
    must_be_a_depth: bool,
    stated_type: Option<DataType>,
) -> Result<PlannedRecord, VortexError> {
    let accumulation = match request.window_rule {
        WindowRule::Accumulation(wanted) => Some(wanted),
        _ => None,
    };
    if let (Some(wanted), Some(stated)) = (accumulation, request.data_type) {
        if stated != DataType::PerCum {
            return Err(VortexError::Refused {
                what: format!("data type of {variable}"),
                reason: format!(
                    "a stated accumulation of {} is PER-CUM, and {stated:?} was stated",
                    wanted.name()
                ),
            });
        }
    }
    if version == 6
        && request.data_type == Some(DataType::InstVal)
        && matches!(meta.time, FrameTime::Period { .. })
    {
        return Err(VortexError::Refused {
            what: format!("data type of {variable}"),
            reason: "INST-VAL stated over a period, in DSS6. Observed behaviour we did not \
                     implement: Monolith 3.3.28, from Vortex 0.13.3, stores the period's start in \
                     the DSS6 header while the pathname holds only its end; the writer here takes \
                     the header's times from the pathname. State the instant, or use DSS7"
                .into(),
        });
    }
    let overrides = Overrides {
        c_part: request.parts.c.as_deref(),
        units: request.units.as_deref(),
        data_type: request.data_type.or(stated_type),
    };
    let description = meta.description.as_deref();
    let meaning = units::record_meaning(
        variable,
        description,
        source_file,
        &meta.units,
        &meta.time,
        &overrides,
    )?;
    let (derived, _) = units::c_part(variable, description, source_file);
    if let Some(wanted) = accumulation {
        check_accumulation(
            variable,
            wanted,
            &derived,
            &meaning,
            &meta.units,
            request.units.as_deref(),
        )?;
    } else if request.window_rule == WindowRule::Refuse
        && variable.to_ascii_lowercase().contains("qpe")
        && derived == units::Variable::Precipitation.dss_c_part()
    {
        // Observed behaviour we did not implement by default: `VortexVariable.fromName` calls any name
        // holding `qpe` precipitation, so Vortex 1.1.0 and 0.13.3 store the real `FLASH_QPE_ARI01H` as
        // PRECIPITATION in years, as INST-VAL, and as PER-CUM under a one-hour file name.
        if let Some(why) = window::not_a_rain_depth(variable, &meta.units) {
            return Err(VortexError::Refused {
                what: format!("precipitation from {variable}"),
                reason: format!(
                    "{why}. Observed behaviour we did not implement: Vortex stores it as \
                     PRECIPITATION. Pass window_rule vortex for Vortex's record"
                ),
            });
        }
    } else if must_be_a_depth
        && request.window_rule == WindowRule::Refuse
        && derived == units::Variable::Precipitation.dss_c_part()
    {
        // Observed behaviour we did not implement by default: `VortexVariable.fromName` goes by the
        // name alone, so Vortex 1.1.0 stores IMERG's `precipitationCal_cnt`, a count, as PRECIPITATION.
        if let Some(why) =
            import_netcdf::not_a_depth(variable, &meta.units, request.units.as_deref())
        {
            return Err(VortexError::Refused {
                what: format!("precipitation from {variable}"),
                reason: format!(
                    "{why}. Observed behaviour we did not implement: Vortex stores it as \
                     PRECIPITATION. Pass window_rule vortex for Vortex's record"
                ),
            });
        }
    }
    let parts = Parts {
        a: request.parts.a.clone(),
        b: request.parts.b.clone(),
        f: request.parts.f.clone(),
    };
    sink_dss::plan_record(&parts, meaning, &derived, description, &meta.time)
}

// A stated accumulation stamps a depth, and changes no value. A rate would be multiplied by the window
// (`DataConverter`), which is a change of value resting on the caller's word alone, and a variable that
// is not precipitation has no accumulation at all, whatever C part the caller gives it.
fn check_accumulation(
    variable: &str,
    wanted: Accumulation,
    derived_c_part: &str,
    meaning: &units::RecordMeaning,
    source_units: &str,
    label: Option<&str>,
) -> Result<(), VortexError> {
    let refuse = |reason: String| VortexError::Refused {
        what: format!("accumulation {} of {variable}", wanted.name()),
        reason,
    };
    if derived_c_part != units::Variable::Precipitation.dss_c_part() {
        return Err(refuse(format!(
            "the source is {derived_c_part}, not precipitation"
        )));
    }
    // Before the conversion check: years are refused as what they are, whatever a label says.
    if !source_units_convert(source_units) {
        if let Some(why) = window::not_a_rain_depth(variable, source_units) {
            return Err(refuse(why));
        }
    }
    if let Some(label) = label {
        if !units::parse_unit(Some(label)).is_some_and(window::is_depth_unit) {
            return Err(refuse(format!(
                "units {label:?} are not a depth, and a stated accumulation stores a depth. A label \
                 relabels and does not convert"
            )));
        }
    }
    if meaning.conversion != units::Conversion::None {
        return Err(refuse(format!(
            "its values would be converted ({:?}), and a stated accumulation changes no value",
            meaning.conversion
        )));
    }
    if meaning.data_type != DataType::PerCum {
        return Err(refuse(format!(
            "it would be stored as {:?}, not PER-CUM",
            meaning.data_type
        )));
    }
    Ok(())
}

// A rate says what it is in the conversion refusal below, which names the factor.
fn source_units_convert(source_units: &str) -> bool {
    matches!(
        units::parse_unit(Some(source_units)),
        Some(units::Unit::MmPerSecond | units::Unit::MmPerHour | units::Unit::MmPerDay)
    )
}

// Whole seconds of a frame's period, in the frame's own day count.
fn period_seconds(time: &FrameTime) -> Option<(i64, i64)> {
    match *time {
        FrameTime::Period { start, end } => {
            Some((start.0 * 86_400 + start.1, end.0 * 86_400 + end.1))
        }
        _ => None,
    }
}

// Mark which periods overlap or leave a gap, within each set of A, B, C and F parts. Records replaced
// by a later one in this run are not stored, so they do not count.
fn note_overlaps(items: &mut [Item]) {
    let series = |item: &Item| {
        let parts: Vec<&str> = item.planned.pathname.split('/').collect();
        [1, 2, 3, 6].map(|i| parts.get(i).copied().unwrap_or("").to_string())
    };
    let mut order: Vec<([String; 4], i64, i64, usize)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.superseded && item.window.is_some())
        .filter_map(|(at, item)| {
            let (start, end) = period_seconds(&item.time)?;
            Some((series(item), start, end, at))
        })
        .collect();
    order.sort();
    let mut reached: Option<(&[String; 4], i64)> = None;
    let mut marks = Vec::with_capacity(order.len());
    for (key, start, end, at) in &order {
        let (overlaps, gap, latest) = match reached {
            Some((known, latest)) if known == key => {
                (*start < latest, *start > latest, latest.max(*end))
            }
            _ => (false, false, *end),
        };
        reached = Some((key, latest));
        marks.push((*at, overlaps, gap));
    }
    for (at, overlaps, gap) in marks {
        if let Some(note) = &mut items[at].window {
            note.overlaps_earlier = overlaps;
            note.gap_before = gap;
        }
    }
}

/// A source the importer opens itself or is given.
pub trait SourceProvider {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// A label for errors and reports.
    fn path(&self, index: usize) -> PathBuf;
    fn open(&self, index: usize) -> Result<Box<dyn GridSource + '_>, VortexError>;
}

struct Files<'a> {
    paths: &'a [PathBuf],
    options: OpenOptions,
    request: &'a ImportRequest,
}

impl SourceProvider for Files<'_> {
    fn len(&self) -> usize {
        self.paths.len()
    }

    fn path(&self, index: usize) -> PathBuf {
        self.paths[index].clone()
    }

    fn open(&self, index: usize) -> Result<Box<dyn GridSource + '_>, VortexError> {
        let path = &self.paths[index];
        // A store takes the request's own limits and stop flag, which `OpenOptions` cannot hold.
        if path.is_dir() {
            return crate::source::zarr::open_source_with(
                path,
                &self.options,
                self.request.zarr_limits,
                self.request.stop.clone(),
            );
        }
        open_source_with_stop(path, &self.options, self.request.stop.as_ref())
    }
}

// An output that is one of the inputs, by the file itself and not its name, so a hard link is seen
// (`file_identity`). The first record's write would replace what the run is reading.
fn refuse_output_that_is_an_input(request: &ImportRequest) -> Result<(), ImportError> {
    for input in &request.inputs {
        let same = crate::file_identity::same_file(input, &request.output)
            .map_err(|e| ImportError::from(VortexError::Io(e)).at(input, None, None))?;
        if same {
            return Err(ImportError::from(VortexError::Refused {
                what: format!("output {}", request.output.display()),
                reason: format!(
                    "it is the input {}, by another name or the same one: a write would replace \
                     what the run reads",
                    input.display()
                ),
            })
            .at(input, None, None));
        }
    }
    Ok(())
}

/// Import files. See the module's documentation.
pub fn import_gridded(
    request: &ImportRequest,
    progress: &mut dyn FnMut(&Progress) -> ControlFlow<()>,
) -> Result<Vec<ImportedRecord>, ImportError> {
    refuse_output_that_is_an_input(request)?;
    let files = Files {
        paths: &request.inputs,
        options: OpenOptions {
            window_rule: request.window_rule,
            limits: SourceLimits {
                max_cells: request.limits.max_source_cells,
                max_input_bytes: request.limits.max_input_bytes,
                archive: request.limits.archive_budget(),
                ..SourceLimits::default()
            },
            netcdf: request.netcdf,
            snodas: request.snodas,
        },
        request,
    };
    run(&files, request, progress)
}

/// Import from any [`SourceProvider`], for arrays and other formats.
pub fn run(
    provider: &dyn SourceProvider,
    request: &ImportRequest,
    progress: &mut dyn FnMut(&Progress) -> ControlFlow<()>,
) -> Result<Vec<ImportedRecord>, ImportError> {
    let result = run_records(provider, request, progress);
    sink_dss::end_sync(request.sync, &request.output, result, |e, completed| {
        ImportError {
            completed,
            ..ImportError::planning(Failure::Write(e))
        }
        .phase_writing()
    })
}

fn run_records(
    provider: &dyn SourceProvider,
    request: &ImportRequest,
    progress: &mut dyn FnMut(&Progress) -> ControlFlow<()>,
) -> Result<Vec<ImportedRecord>, ImportError> {
    if provider.is_empty() {
        return Err(ImportError::planning(Failure::BadRequest(
            "no inputs".into(),
        )));
    }
    let (version, existed) = plan_output(request)?;
    let geo = prepare_geo(request)?;
    let selection = SourceSelection {
        bbox: request.source_bbox,
        times: request.source_times,
        envelope: geo.envelope.clone(),
        cell_size: request.cell_size,
    };
    let (items, grids, mut superseded_count) =
        plan_run(provider, request, &geo, &selection, version)?;
    if items.is_empty() {
        return Err(ImportError::planning(Failure::Vortex(
            VortexError::Refused {
                what: "import".into(),
                reason: "the inputs hold no frames".into(),
            },
        )));
    }
    superseded_count = superseded_count.min(items.len());
    let total = items.len() - superseded_count;
    let mut created = existed;
    let mut completed: Vec<ImportedRecord> = Vec::new();
    let mut done = 0;
    let mut index = 0;
    let options = |record: usize| SinkOptions {
        version,
        sync: sink_dss::record_sync(request.sync),
        allow_undetectable_readers: request.allow_undetectable_readers,
        fault: request
            .fault
            .filter(|(at, _)| *at == record)
            .map(|(_, point)| point),
    };
    while index < items.len() {
        let input = items[index].input;
        let path = provider.path(input);
        let end = items[index..]
            .iter()
            .position(|i| i.input != input)
            .map_or(items.len(), |k| index + k);
        // A source stays open only for its own frames.
        let source = provider.open(input).map_err(|e| {
            ImportError {
                completed: std::mem::take(&mut completed),
                ..ImportError::planning(Failure::Vortex(e))
            }
            .phase_writing()
            .at(&path, None, None)
        })?;
        // The same part of the source the plan was made on.
        source
            .select(&items[index].variable, &selection)
            .map_err(|e| {
                ImportError {
                    completed: std::mem::take(&mut completed),
                    ..ImportError::planning(Failure::Vortex(e))
                }
                .phase_writing()
                .at(&path, Some(&items[index].variable), None)
            })?;
        let mut current: Option<Destination> = None;
        for item in &items[index..end] {
            let stop = |failure: Failure, completed: &mut Vec<ImportedRecord>| ImportError {
                phase: "writing",
                input: Some(path.clone()),
                variable: Some(item.variable.clone()),
                step: Some(item.step),
                in_doubt: matches!(failure, Failure::Write(_))
                    .then(|| item.planned.pathname.clone()),
                failure,
                completed: std::mem::take(completed),
            };
            if item.superseded {
                continue;
            }
            if stop_asked(request, &path) {
                return Err(stop(Failure::Stopped, &mut completed));
            }
            let result = write_item(
                source.as_ref(),
                item,
                &grids[item.grid],
                request,
                &geo,
                &mut current,
                &mut created,
                version,
                &options(done),
            );
            match result {
                Ok(record) => {
                    done += 1;
                    let reported = Progress {
                        done,
                        total,
                        input,
                        variable: item.variable.clone(),
                        step: item.step,
                        pathname: record.pathname.clone(),
                    };
                    completed.push(ImportedRecord {
                        input: path.clone(),
                        variable: item.variable.clone(),
                        step: item.step,
                        record,
                        window: item.window.clone(),
                    });
                    if progress(&reported).is_break() && done < total {
                        return Err(stop(Failure::Cancelled, &mut completed));
                    }
                }
                // A read the caller stopped fails as a refusal of that read. No write of this record
                // had begun, so it is the run that was stopped, and nothing is in doubt.
                Err(Failure::Vortex(_)) if stop_asked(request, &path) => {
                    return Err(stop(Failure::Stopped, &mut completed))
                }
                Err(failure) => return Err(stop(failure, &mut completed)),
            }
        }
        index = end;
    }
    Ok(completed)
}

// Whether the caller's stop ends the run here, as [`Failure::Stopped`]. Not for a Zarr store: P49's
// store looks at the same flag in each read and refuses that read itself ("the read was stopped"),
// which is what its callers and tests expect, so the importer leaves a store's stop to the store.
fn stop_asked(request: &ImportRequest, input: &Path) -> bool {
    !input.is_dir()
        && request
            .stop
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
}

impl ImportError {
    fn phase_writing(mut self) -> Self {
        self.phase = "writing";
        self
    }
}

fn plan_run(
    provider: &dyn SourceProvider,
    request: &ImportRequest,
    geo: &Geo,
    selection: &SourceSelection,
    version: u8,
) -> Result<(Vec<Item>, Vec<SourceGridDef>, usize), ImportError> {
    let mut items: Vec<Item> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut checked: Vec<SourceGridDef> = Vec::new();
    let mut superseded = 0;
    let mut stored_at: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for input in 0..provider.len() {
        let path = provider.path(input);
        let at = |e: VortexError, variable: Option<&str>, step: Option<usize>| {
            // Whatever a stopped read reports, the run was stopped.
            if stop_asked(request, &path) {
                return ImportError::planning(Failure::Stopped).at(&path, variable, step);
            }
            ImportError::from(e).at(&path, variable, step)
        };
        if stop_asked(request, &path) {
            return Err(ImportError::planning(Failure::Stopped).at(&path, None, None));
        }
        let source = provider.open(input).map_err(|e| at(e, None, None))?;
        let variable =
            select(source.as_ref(), &request.selection).map_err(|e| at(e, None, None))?;
        let v = Some(variable.as_str());
        source
            .select(&variable, selection)
            .map_err(|e| at(e, v, None))?;
        let note = match request.window_rule {
            WindowRule::Accumulation(wanted) => {
                let stated_by = source
                    .accumulation_stated_by(&variable, wanted)
                    .ok_or_else(|| VortexError::Refused {
                        what: format!("accumulation {} of {variable}", wanted.name()),
                        reason: "a stated accumulation is for GRIB2 input; this source's frames \
                                 state their own times"
                            .into(),
                    })
                    .and_then(|stated| stated)
                    .map_err(|e| at(e, v, None))?;
                Some(WindowNote {
                    seconds: wanted.seconds(),
                    stated_by,
                    overlaps_earlier: false,
                    gap_before: false,
                })
            }
            _ => None,
        };
        let steps = source.step_count(&variable).map_err(|e| at(e, v, None))?;
        if steps == 0 {
            return Err(at(
                VortexError::Refused {
                    what: format!("variable {variable}"),
                    reason: "it holds no frames".into(),
                },
                v,
                None,
            ));
        }
        for step in 0..steps {
            let s = Some(step);
            let def = source.grid(&variable, step).map_err(|e| at(e, v, s))?;
            if def.nx.saturating_mul(def.ny) > request.limits.max_source_cells {
                return Err(at(
                    VortexError::Refused {
                        what: "source grid".into(),
                        reason: format!(
                            "{} x {} is over the limit of {} cells",
                            def.nx, def.ny, request.limits.max_source_cells
                        ),
                    },
                    v,
                    s,
                ));
            }
            let meta = source.step_meta(&variable, step).map_err(|e| at(e, v, s))?;
            let grid = match checked.iter().position(|known| *known == def) {
                Some(known) => known,
                None => {
                    let d = destination(request, geo, &def, false).map_err(|e| at(e, v, s))?;
                    sink_dss::placement(&d.transform, d.ny, request.align)
                        .map_err(|e| at(e, v, s))?;
                    checked.push(def.clone());
                    checked.len() - 1
                }
            };
            let short = source.short_name(&variable).map_err(|e| at(e, v, s))?;
            let stated_type = source
                .stated_data_type(&variable, step)
                .map_err(|e| at(e, v, s))?;
            let planned = name_step(
                request,
                source.file_name(),
                &short,
                &meta,
                version,
                source.precipitation_must_be_a_depth(),
                stated_type,
            )
            .map_err(|e| at(e, v, s))?;
            let crs = sink_crs_for(geo, &def).map_err(|e| at(e, v, s))?;
            let stored = sink_dss::check_record(&planned, &crs, version)
                .map_err(|e| ImportError::planning(Failure::Write(e)).at(&path, v, s))?;
            if !seen.insert(stored.clone()) {
                match request.duplicates {
                    Duplicates::Refuse => {
                        return Err(at(
                            VortexError::Refused {
                                what: format!("pathname {}", String::from_utf8_lossy(&stored)),
                                reason: "the run would write it twice, and the second would replace \
                                         the first. Observed behaviour we did not implement: Vortex \
                                         keeps whichever came last. Pass duplicates=\"last\" for that"
                                    .into(),
                            },
                            v,
                            s,
                        ))
                    }
                    Duplicates::Last => {
                        if let Some(&earlier) = stored_at.get(&stored) {
                            let earlier: &mut Item = &mut items[earlier];
                            earlier.superseded = true;
                            superseded += 1;
                        }
                    }
                }
            }
            stored_at.insert(stored, items.len());
            if items.len() >= request.limits.max_records {
                return Err(at(
                    VortexError::Refused {
                        what: "import".into(),
                        reason: format!("over the limit of {} records", request.limits.max_records),
                    },
                    v,
                    s,
                ));
            }
            items.push(Item {
                input,
                variable: variable.clone(),
                step,
                planned,
                time: meta.time,
                source_units: meta.units,
                description: meta.description,
                grid,
                superseded: false,
                window: note.clone(),
            });
        }
    }
    note_overlaps(&mut items);
    Ok((items, checked, superseded))
}

fn sink_crs_for(geo: &Geo, def: &SourceGridDef) -> Result<SinkCrs, VortexError> {
    match &geo.sink_crs {
        Some(c) => Ok(c.clone()),
        None => match &def.stored_wkt {
            Some(text) => SinkCrs::of_frame(&Crs::Wkt(text.clone())),
            None => SinkCrs::of_frame(&def.crs),
        },
    }
}

// What the decoded frame states against what the plan named it, so a record cannot be written under
// another's window, variable or units if a source's answers differ, or an input changes mid-run.
fn identity_mismatch(item: &Item, def: &SourceGridDef, parts: &GridFrameParts) -> Option<String> {
    if parts.time != item.time {
        return Some(format!("time {:?}, planned {:?}", parts.time, item.time));
    }
    if parts.variable != item.variable {
        return Some(format!(
            "variable {}, planned {}",
            parts.variable, item.variable
        ));
    }
    if parts.units != item.source_units {
        return Some(format!(
            "units {}, planned {}",
            parts.units, item.source_units
        ));
    }
    if parts.description != item.description {
        return Some(format!(
            "description {:?}, planned {:?}",
            parts.description, item.description
        ));
    }
    if (parts.nx, parts.ny) != (def.nx, def.ny) || parts.transform != def.transform {
        return Some(format!(
            "grid {} x {} at {:?}, planned {} x {} at {:?}",
            parts.nx, parts.ny, parts.transform, def.nx, def.ny, def.transform
        ));
    }
    if parts.crs != def.crs {
        return Some(format!("CRS {:?}, planned {:?}", parts.crs, def.crs));
    }
    if !same_marker(parts.nodata, def.nodata) {
        return Some(format!(
            "nodata {:?}, planned {:?}",
            parts.nodata, def.nodata
        ));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn write_item(
    source: &dyn GridSource,
    item: &Item,
    planned: &SourceGridDef,
    request: &ImportRequest,
    geo: &Geo,
    current: &mut Option<Destination>,
    created: &mut bool,
    version: u8,
    options: &SinkOptions,
) -> Result<WrittenRecord, Failure> {
    let vortex = Failure::Vortex;
    let def = source.grid(&item.variable, item.step).map_err(vortex)?;
    // The plan checked this grid: its placement, CRS and limits. An input that now states another
    // must not reach a record under the plan's name.
    if def != *planned {
        return Err(vortex(VortexError::Refused {
            what: format!("frame {} of {}", item.step, item.variable),
            reason: format!(
                "the input's grid has changed since planning: {} x {} at {:?}, CRS {:?}, nodata \
                 {:?}; planned {} x {} at {:?}, CRS {:?}, nodata {:?}",
                def.nx,
                def.ny,
                def.transform,
                def.crs,
                def.nodata,
                planned.nx,
                planned.ny,
                planned.transform,
                planned.crs,
                planned.nodata
            ),
        }));
    }
    if current.as_ref().is_none_or(|d| d.source != def) {
        *current = Some(destination(request, geo, &def, true).map_err(vortex)?);
    }
    let Some(dest) = current.as_ref() else {
        return Err(vortex(VortexError::Refused {
            what: "import".into(),
            reason: "no destination".into(),
        }));
    };
    let frame = source.frame(&item.variable, item.step).map_err(vortex)?;
    if let Some(reason) = identity_mismatch(item, planned, frame.parts()) {
        return Err(vortex(VortexError::Refused {
            what: format!("frame {} of {}", item.step, item.variable),
            reason: format!("the decoded frame states {reason}"),
        }));
    }
    let frame = match &dest.plan {
        Some(plan) => {
            let (warped, collisions) = plan.warp_frame_counted(&frame).map_err(vortex)?;
            if collisions > 0 {
                return Err(vortex(VortexError::Refused {
                    what: format!("frame {} of {}", item.step, item.variable),
                    reason: format!(
                        "{collisions} cell(s) computed from valid data came out equal to the \
                         nodata marker {:?}, and would read as missing. State a marker the data \
                         cannot reach. Observed behaviour we did not implement: Vortex 1.1.0 stores \
                         them as missing",
                        planned.nodata
                    ),
                }));
            }
            warped
        }
        None => frame,
    };
    let mut parts = frame.into_parts();
    let place = sink_dss::placement(&parts.transform, parts.ny, request.align).map_err(vortex)?;
    sink_dss::to_stored_cells(&mut parts, item.planned.meaning.conversion).map_err(vortex)?;
    if !*created {
        create_output(&request.output, version, request.sync).map_err(Failure::Write)?;
        *created = true;
    }
    sink_dss::write_record(
        &request.output,
        &item.planned,
        &dest.sink_crs,
        place,
        &parts.cells,
        parts.nx,
        parts.ny,
        options,
    )
    .map_err(Failure::Write)
}

pub(crate) fn create_output(
    path: &Path,
    version: u8,
    sync: dss_core::dss7::write::SyncMode,
) -> Result<(), WriteError> {
    // The new file's own flush follows the run's policy record by record: with `End` the
    // run's single flush covers it.
    let sync = sink_dss::record_sync(sync);
    if version == 6 {
        dss_core::dss6::write::create_dss6_with(path, sync)
    } else {
        dss_core::dss7::write::create_dss7_with(path, sync)
    }
}

/// Frames a caller already holds in memory (`import_arrays`). Times are one per frame.
#[derive(Debug, Clone)]
pub struct ArrayInput {
    /// `nt` frames of `nx * ny` cells, frame after frame, in the transform's row order.
    pub cells: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
    pub transform: GeoTransform,
    pub crs: Crs,
    pub nodata: f32,
    pub times: Vec<FrameTime>,
    pub variable: String,
    pub description: Option<String>,
    pub units: String,
    /// A name for reports and for `getCPart`'s file-name fallback. May be empty.
    pub file_name: String,
}

impl ArrayInput {
    fn validate(&self) -> Result<(), VortexError> {
        let refuse = |reason: String| VortexError::Refused {
            what: "arrays".into(),
            reason,
        };
        if self.times.is_empty() {
            return Err(refuse("no frames: give at least one time".into()));
        }
        let per_frame = self
            .nx
            .checked_mul(self.ny)
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                refuse(format!(
                    "grid {} x {} is empty or too large",
                    self.nx, self.ny
                ))
            })?;
        let expected = per_frame
            .checked_mul(self.times.len())
            .ok_or_else(|| refuse("too many cells".into()))?;
        if self.cells.len() != expected {
            return Err(refuse(format!(
                "{} cells for {} frame(s) of {} x {}",
                self.cells.len(),
                self.times.len(),
                self.nx,
                self.ny
            )));
        }
        if self.variable.is_empty() {
            return Err(refuse("a variable name is needed".into()));
        }
        Ok(())
    }
}

struct ArraySource<'a> {
    input: &'a ArrayInput,
}

impl ArraySource<'_> {
    fn time(&self, step: usize) -> Result<FrameTime, VortexError> {
        self.input
            .times
            .get(step)
            .copied()
            .ok_or_else(|| VortexError::Refused {
                what: "arrays".into(),
                reason: format!("step {step} of {}", self.input.times.len()),
            })
    }

    fn check(&self, variable: &str) -> Result<(), VortexError> {
        if variable == self.input.variable {
            Ok(())
        } else {
            Err(VortexError::Refused {
                what: format!("variable {variable}"),
                reason: "not in these arrays".into(),
            })
        }
    }
}

// The frame model wants a data type that fits the time; the writer's rule decides the record's.
fn type_for(time: &FrameTime) -> DataType {
    match time {
        FrameTime::Period { .. } => DataType::PerAver,
        _ => DataType::InstVal,
    }
}

impl GridSource for ArraySource<'_> {
    fn file_name(&self) -> &str {
        &self.input.file_name
    }

    fn variables(&self) -> Vec<String> {
        vec![self.input.variable.clone()]
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        self.check(variable)?;
        Ok(self.input.times.len())
    }

    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError> {
        self.check(variable)?;
        let time = self.time(step)?;
        Ok(StepMeta {
            time,
            units: self.input.units.clone(),
            description: self.input.description.clone(),
            source_type: type_for(&time),
        })
    }

    fn grid(&self, variable: &str, _step: usize) -> Result<SourceGridDef, VortexError> {
        self.check(variable)?;
        Ok(SourceGridDef {
            nx: self.input.nx,
            ny: self.input.ny,
            transform: self.input.transform,
            crs: self.input.crs.clone(),
            nodata: Some(self.input.nodata),
            stored_wkt: None,
        })
    }

    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.check(variable)?;
        let time = self.time(step)?;
        let per_frame = self.input.nx * self.input.ny;
        let start = step * per_frame;
        // A failed allocation must be an error: `to_vec` would abort the process.
        let mut cells: Vec<f32> = Vec::new();
        cells
            .try_reserve_exact(per_frame)
            .map_err(|_| VortexError::Refused {
                what: "arrays".into(),
                reason: format!("cannot hold a frame of {per_frame} cells"),
            })?;
        cells.extend_from_slice(&self.input.cells[start..start + per_frame]);
        let row_order = if self.input.transform.dy < 0.0 {
            crate::RowOrder::NorthToSouth
        } else {
            crate::RowOrder::SouthToNorth
        };
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx: self.input.nx,
            ny: self.input.ny,
            row_order,
            transform: self.input.transform,
            crs: self.input.crs.clone(),
            nodata: Some(self.input.nodata),
            time,
            data_type: type_for(&time),
            units: self.input.units.clone(),
            variable: self.input.variable.clone(),
            description: self.input.description.clone(),
        })?)
    }
}

struct Arrays<'a> {
    input: &'a ArrayInput,
}

impl SourceProvider for Arrays<'_> {
    fn len(&self) -> usize {
        1
    }

    fn path(&self, _index: usize) -> PathBuf {
        PathBuf::from(if self.input.file_name.is_empty() {
            "<arrays>"
        } else {
            &self.input.file_name
        })
    }

    fn open(&self, _index: usize) -> Result<Box<dyn GridSource + '_>, VortexError> {
        Ok(Box::new(ArraySource { input: self.input }))
    }
}

/// Import arrays through the same warp, unit, naming and writer steps as a file (`VORTEX_SCOPE.md`
/// section 4.7: any format a caller can decode still gets the native warp and writer).
/// `request.inputs` and `request.selection` are ignored.
pub fn import_arrays(
    input: &ArrayInput,
    request: &ImportRequest,
    progress: &mut dyn FnMut(&Progress) -> ControlFlow<()>,
) -> Result<Vec<ImportedRecord>, ImportError> {
    input.validate()?;
    let mut request = request.clone();
    request.selection = Selection::Variable(input.variable.clone());
    run(&Arrays { input }, &request, progress)
}

/// Variables each input offers.
pub fn list_variables(path: &Path) -> Result<Vec<String>, VortexError> {
    Ok(open_source(path, &OpenOptions::default())?.variables())
}

//! Running a tool: plan every record, compute every frame once, then write.
//!
//! Sources, Vortex 1.1.0: `math/BatchCalculator`, `math/BatchGridCalculator`, `math/BatchSanitizer`,
//! `math/Normalizer.normalize`, `geo/BatchSubsetter`, `geo/SubsettableUnit.process`, and
//! `io/DssDataWriter.write` for what each record becomes. See the module note in [`super`] for what
//! is deliberately different.

// `ImportError` is the importer's own, so the bindings map a tool's failures the same way.
#![allow(clippy::result_large_err)]

use super::cells::{self, Operation, Thresholds};
use super::dss_source::{self, DssSource};
use super::java_order;
use crate::crs::{CrsDef, DatumPolicy, Envelope};
use crate::error::VortexError;
use crate::file_identity;
use crate::frame::{Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts};
use crate::import::{
    self, Failure, GridSource, ImportError, ImportLimits, ImportedRecord, OpenOptions,
    SourceGridDef,
};
use crate::sink_dss::{self, Parts, PlannedRecord, SinkCrs, SinkOptions};
use crate::units::{self, Conversion, Overrides};
use crate::warp::{Align, Method, WarpEnvelope, WarpLimits, WarpPlan, WarpSpec};
use dss_core::dss7::write::{FaultPoint, WriteError};
use dss_core::grid_write::GRID_NODATA;
use dss_core::lock::{self, LockPolicy};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where a tool writes, and what the caller states over each record (`writeOptions`).
#[derive(Debug, Clone)]
pub struct OutputOptions {
    pub output: PathBuf,
    pub part_a: String,
    pub part_b: String,
    pub part_c: Option<String>,
    /// `*` keeps a DSS input's F part (`DssDataWriter.write`). Vortex keeps no other part this way.
    pub part_f: String,
    /// Relabels the units, and does not convert them.
    pub units: Option<String>,
    pub data_type: Option<DataType>,
    pub version: Option<u8>,
    pub align: Align,
    pub sync: bool,
    pub allow_undetectable_readers: bool,
    /// Crash simulation, at the nth record written, for tests.
    pub fault: Option<(usize, FaultPoint)>,
}

impl OutputOptions {
    pub fn new(output: impl Into<PathBuf>) -> Self {
        OutputOptions {
            output: output.into(),
            part_a: String::new(),
            part_b: String::new(),
            part_c: None,
            part_f: String::new(),
            units: None,
            data_type: None,
            version: None,
            align: Align::Refuse,
            sync: true,
            allow_undetectable_readers: true,
            fault: None,
        }
    }
}

/// One record written, and what it was before.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRecord {
    pub imported: ImportedRecord,
    pub source_units: String,
    /// `None` when the source is not a DSS record and so states no type of HEC's.
    pub source_data_type: Option<DataType>,
    /// Whether `DataConverter.convert` changed the cells' units, as well as how they are spelled.
    pub converted: bool,
}

/// A raster calculation (`BatchGridCalculator`).
#[derive(Debug, Clone)]
pub struct RasterRequest {
    pub raster: PathBuf,
    /// Which of the raster file's grids. Required when it holds more than one; Vortex takes the first.
    pub raster_variable: Option<String>,
    pub operation: Operation,
    pub method: Method,
    /// Reuse one timed DSS raster with every input grid, whatever times they state: a factor grid.
    /// Off, a DSS raster and an input grid stating different times are refused. A raster with no
    /// time, such as a GeoTIFF, is static by nature and needs no option.
    pub static_raster: bool,
}

/// The envelope to clip to.
#[derive(Debug, Clone)]
pub enum ClipBounds {
    /// `VectorUtils.getEnvelope`, in the `.prj`'s CRS.
    Shapefile(PathBuf),
    /// In a stated CRS. There is no default: a CRS is never assumed.
    Envelope { envelope: Envelope, crs: Crs },
}

/// A clip (`BatchSubsetter`): the envelope, by nearest neighbour, in the grid's CRS.
#[derive(Debug, Clone)]
pub struct ClipRequest {
    pub bounds: ClipBounds,
}

/// A normalization (`Normalizer`). Times are (julian day, seconds past midnight).
#[derive(Debug, Clone)]
pub struct NormalizeRequest {
    pub normals: PathBuf,
    pub source_variables: Option<Vec<String>>,
    pub normals_variables: Option<Vec<String>>,
    pub start: (i64, i64),
    pub end: (i64, i64),
    pub interval_seconds: i64,
}

/// One interval a normalization walked, in seconds from julian day 0.
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalNote {
    pub start: i128,
    pub end: i128,
    pub sources: usize,
    pub normals: usize,
}

/// What a normalization did, including what its windows left out.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizeOutcome {
    pub records: Vec<ToolRecord>,
    pub intervals: Vec<IntervalNote>,
    /// Source grids not wholly inside any interval, which Vortex also leaves out with no message.
    pub sources_left_out: Vec<String>,
    /// Normal grids not wholly inside any interval.
    pub normals_left_out: Vec<String>,
    /// A hash bin long enough that the JDK may iterate it otherwise: see [`java_order`].
    pub long_hash_bin: bool,
    /// Valid source cells written as missing, over all records, because their normal was missing.
    /// A cell the source already held as missing is not counted.
    pub cells_made_missing_by_normal: u64,
}

// One planned frame.
struct Item {
    variable: String,
    step: usize,
    planned: PlannedRecord,
    time: FrameTime,
    source_units: String,
    source_type: Option<DataType>,
    def: SourceGridDef,
    sink_crs: SinkCrs,
}

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

fn planning(e: VortexError) -> ImportError {
    ImportError::from(e)
}

fn bad_request(message: String) -> ImportError {
    ImportError {
        failure: Failure::BadRequest(message),
        phase: "planning",
        input: None,
        variable: None,
        step: None,
        completed: Vec::new(),
        in_doubt: None,
    }
}

fn at(
    mut e: ImportError,
    input: &Path,
    variable: Option<&str>,
    step: Option<usize>,
) -> ImportError {
    e.input = Some(input.to_path_buf());
    e.variable = variable.map(str::to_string);
    e.step = step;
    e
}

/// Open a DSS file, a GeoTIFF, an ASCII grid or a GRIB2 file. `DataReader.builder` also reads NetCDF,
/// which arrives with P48.
fn open_any(path: &Path) -> Result<(Box<dyn GridSource>, bool), VortexError> {
    if !path.exists() {
        return Err(refused(
            format!("input {}", path.display()),
            "it does not exist",
        ));
    }
    if dss_source::is_dss(path)? {
        return Ok((Box::new(DssSource::open(path)?), true));
    }
    Ok((import::open_source(path, &OpenOptions::default())?, false))
}

fn select_variables(
    source: &dyn GridSource,
    stated: Option<&[String]>,
) -> Result<Vec<String>, VortexError> {
    let available = source.variables();
    let chosen = match stated {
        None => available.clone(),
        Some(names) => {
            if let Some(name) = names.iter().find(|n| !available.contains(n)) {
                return Err(refused(
                    format!("variable {name}"),
                    format!(
                        "{} does not hold it. Observed behaviour we did not implement: Vortex skips \
                         a name the file does not hold, with no message",
                        source.file_name()
                    ),
                ));
            }
            names.to_vec()
        }
    };
    if chosen.is_empty() {
        return Err(refused(
            format!("input {}", source.file_name()),
            "the selection holds no grids",
        ));
    }
    Ok(chosen)
}

// Planning's output checks. A tool reads its input while writing, so the two must differ.
fn plan_output(options: &OutputOptions, inputs: &[&Path]) -> Result<(u8, bool), ImportError> {
    if !matches!(options.version, None | Some(6) | Some(7)) {
        return Err(bad_request(format!(
            "version must be 6 or 7, not {:?}",
            options.version
        )));
    }
    // By the file itself, so another name for an input (a hard link, say) is refused as well. Only
    // an existing output can be one; a check that cannot be made refuses too.
    for input in inputs {
        let same = file_identity::same_file(input, &options.output).map_err(|e| {
            planning(refused(
                format!("output {}", options.output.display()),
                format!(
                    "cannot tell whether it is the input {}: {e}",
                    input.display()
                ),
            ))
        })?;
        if same {
            return Err(planning(refused(
                format!("output {}", options.output.display()),
                format!(
                    "it is also the input {}, whose records this run would replace as it reads them. \
                     Name another output",
                    input.display()
                ),
            )));
        }
    }
    let existing = import::existing_version(&options.output).map_err(|failure| ImportError {
        failure,
        ..bad_request(String::new())
    })?;
    let version = match (existing, options.version) {
        (Some(have), Some(want)) if have != want => {
            return Err(planning(refused(
                format!("output {}", options.output.display()),
                format!(
                "it is DSS{have}, and version {want} was asked for. Writers keep a file's version"
            ),
            )))
        }
        (Some(have), _) => have,
        (None, Some(want)) => want,
        (None, None) => 7,
    };
    if existing.is_some() {
        let report = lock::lock_status(&options.output).map_err(|e| ImportError {
            failure: Failure::Write(WriteError::Lock(e)),
            ..bad_request(String::new())
        })?;
        let policy = LockPolicy {
            allow_undetectable_readers: options.allow_undetectable_readers,
            ..LockPolicy::default()
        };
        lock::ensure_unlocked_for_write(&report, &policy).map_err(|r| ImportError {
            failure: Failure::Write(WriteError::Locked(r)),
            ..bad_request(String::new())
        })?;
    } else if let Some(parent) = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        if !parent.is_dir() {
            return Err(bad_request(format!(
                "output folder {} does not exist",
                parent.display()
            )));
        }
    }
    Ok((version, existing.is_some()))
}

// What a record becomes: `DssUtil.getGridInfo` and `DssDataWriter.write`, through the importer's rules.
fn plan_item(
    source: &dyn GridSource,
    is_dss: bool,
    variable: &str,
    step: usize,
    options: &OutputOptions,
    version: u8,
) -> Result<Item, VortexError> {
    let meta = source.step_meta(variable, step)?;
    let def = source.grid(variable, step)?;
    if def.nx.saturating_mul(def.ny) as u64 > ImportLimits::default().max_destination_cells {
        return Err(refused(
            "source grid",
            format!(
                "{} x {} is over the writer's limit of {} cells",
                def.nx,
                def.ny,
                ImportLimits::default().max_destination_cells
            ),
        ));
    }
    if version == 6
        && options.data_type == Some(DataType::InstVal)
        && matches!(meta.time, FrameTime::Period { .. })
    {
        return Err(refused(
            format!("data type of {variable}"),
            "INST-VAL stated over a period, in DSS6. Observed behaviour we did not implement: \
             Monolith 3.3.28, from Vortex 0.13.3, stores the period's start in the DSS6 header while \
             the pathname holds only its end. State the instant, or use DSS7",
        ));
    }
    let overrides = Overrides {
        c_part: options.part_c.as_deref(),
        units: options.units.as_deref(),
        data_type: options.data_type,
    };
    // `dssToDto` names a grid by its C part, and `getCPart` falls back to the file's whole path. A
    // record's C part is its own here, so a DSS input offers no file name.
    let (short, file_name) = if is_dss {
        (dss_source::c_part(variable), "")
    } else {
        (variable.to_string(), source.file_name())
    };
    let description = meta.description.as_deref();
    let meaning = units::record_meaning(
        &short,
        description,
        file_name,
        &meta.units,
        &meta.time,
        &overrides,
    )?;
    let source_type = is_dss.then_some(meta.source_type);
    if let Some(stored) = source_type {
        if options.data_type.is_none() && meaning.data_type != stored {
            return Err(refused(
                format!("data type of {variable}"),
                format!(
                    "the record is {stored:?}, and its C part and window would make it {:?}. State \
                     data_type to choose. Observed behaviour we did not implement: Vortex 1.1.0 and \
                     0.13.3 change it with no message",
                    meaning.data_type
                ),
            ));
        }
    }
    let (derived, _) = units::c_part(&short, description, file_name);
    let f = if options.part_f == "*" {
        if is_dss {
            dss_source::f_part(variable)
        } else {
            String::new()
        }
    } else {
        options.part_f.clone()
    };
    // `updatePathname`: `*` leaves a part as it is, and A and B are still empty at that point.
    let keep = |part: &str| {
        if part == "*" {
            String::new()
        } else {
            part.to_string()
        }
    };
    let parts = Parts {
        a: keep(&options.part_a),
        b: keep(&options.part_b),
        f,
    };
    let planned = sink_dss::plan_record(&parts, meaning, &derived, description, &meta.time)?;
    let sink_crs = match &def.stored_wkt {
        Some(text) => SinkCrs::of_frame(&Crs::Wkt(text.clone()))?,
        None => SinkCrs::of_frame(&def.crs)?,
    };
    Ok(Item {
        variable: variable.to_string(),
        step,
        planned,
        time: meta.time,
        source_units: meta.units,
        source_type,
        def,
        sink_crs,
    })
}

fn plan_items(
    input: &Path,
    source: &dyn GridSource,
    is_dss: bool,
    variables: &[String],
    options: &OutputOptions,
    version: u8,
) -> Result<Vec<Item>, ImportError> {
    let mut items = Vec::new();
    for variable in variables {
        let v = Some(variable.as_str());
        let steps = source
            .step_count(variable)
            .map_err(|e| at(planning(e), input, v, None))?;
        if steps == 0 {
            let e = refused(format!("variable {variable}"), "it holds no frames");
            return Err(at(planning(e), input, v, None));
        }
        for step in 0..steps {
            let item = plan_item(source, is_dss, variable, step, options, version)
                .map_err(|e| at(planning(e), input, v, Some(step)))?;
            items.push(item);
        }
    }
    Ok(items)
}

// The writer's own checks, and one record per pathname. Vortex keeps whichever came last.
fn check_records(input: &Path, items: &[Item], version: u8) -> Result<(), ImportError> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for item in items {
        let v = Some(item.variable.as_str());
        let stored =
            sink_dss::check_record(&item.planned, &item.sink_crs, version).map_err(|e| {
                at(
                    ImportError {
                        failure: Failure::Write(e),
                        ..bad_request(String::new())
                    },
                    input,
                    v,
                    Some(item.step),
                )
            })?;
        if !seen.insert(stored.clone()) {
            let e = refused(
                format!("pathname {}", String::from_utf8_lossy(&stored)),
                "the run would write it twice, and the second would replace the first. Observed \
                 behaviour we did not implement: Vortex keeps whichever came last. Select fewer \
                 grids, or state other parts",
            );
            return Err(at(planning(e), input, v, Some(item.step)));
        }
    }
    Ok(())
}

// Where a frame's grid goes, and its cells. Each tool is one of these.
trait Tool {
    /// The destination's transform and size. Georeference only, so a run can check it before any cell.
    fn destination(&mut self, def: &SourceGridDef) -> Result<GeoTransform, VortexError>;
    /// Cells for one frame, marked with [`GRID_NODATA`] or the frame's own marker.
    fn apply(&mut self, index: usize, frame: GridFrame) -> Result<GridFrame, VortexError>;
}

fn with_cells(frame: GridFrame, cells: Vec<f32>) -> Result<GridFrame, VortexError> {
    Ok(GridFrame::new(GridFrameParts {
        cells,
        nodata: Some(GRID_NODATA),
        ..frame.into_parts()
    })?)
}

fn read_frame(source: &dyn GridSource, item: &Item) -> Result<GridFrame, VortexError> {
    let def = source.grid(&item.variable, item.step)?;
    if def != item.def {
        return Err(refused(
            format!("frame {} of {}", item.step, item.variable),
            "the input's grid has changed since planning",
        ));
    }
    let frame = source.frame(&item.variable, item.step)?;
    let p = frame.parts();
    if p.time != item.time || p.units != item.source_units || p.nx != def.nx || p.ny != def.ny {
        return Err(refused(
            format!("frame {} of {}", item.step, item.variable),
            "the decoded frame does not state what was planned",
        ));
    }
    Ok(frame)
}

// One frame, as the writer will take it.
fn stored_frame(
    tool: &mut dyn Tool,
    source: &dyn GridSource,
    index: usize,
    item: &Item,
) -> Result<GridFrameParts, VortexError> {
    let frame = tool.apply(index, read_frame(source, item)?)?;
    let mut parts = frame.into_parts();
    sink_dss::to_stored_cells(&mut parts, item.planned.meaning.conversion)?;
    Ok(parts)
}

// Check every record, compute every frame once, then write.
fn execute(
    input: &Path,
    source: &dyn GridSource,
    items: &[Item],
    options: &OutputOptions,
    version: u8,
    existed: bool,
    tool: &mut dyn Tool,
) -> Result<Vec<ToolRecord>, ImportError> {
    check_records(input, items, version)?;
    let mut places = Vec::with_capacity(items.len());
    for item in items {
        let v = Some(item.variable.as_str());
        let s = Some(item.step);
        let transform = tool
            .destination(&item.def)
            .map_err(|e| at(planning(e), input, v, s))?;
        places.push(transform);
    }
    for (index, item) in items.iter().enumerate() {
        let v = Some(item.variable.as_str());
        let s = Some(item.step);
        let parts =
            stored_frame(tool, source, index, item).map_err(|e| at(planning(e), input, v, s))?;
        sink_dss::placement(&places[index], parts.ny, options.align)
            .map_err(|e| at(planning(e), input, v, s))?;
    }
    let mut created = existed;
    let mut done: Vec<ToolRecord> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let stop = |failure: Failure, done: &mut Vec<ToolRecord>, in_doubt: bool| ImportError {
            failure,
            in_doubt: in_doubt.then(|| item.planned.pathname.clone()),
            phase: "writing",
            input: Some(input.to_path_buf()),
            variable: Some(item.variable.clone()),
            step: Some(item.step),
            completed: std::mem::take(done)
                .into_iter()
                .map(|r| r.imported)
                .collect(),
        };
        let parts = match stored_frame(tool, source, index, item) {
            Ok(p) => p,
            Err(e) => return Err(stop(Failure::Vortex(e), &mut done, false)),
        };
        let place = match sink_dss::placement(&places[index], parts.ny, options.align) {
            Ok(p) => p,
            Err(e) => return Err(stop(Failure::Vortex(e), &mut done, false)),
        };
        if !created {
            if let Err(e) = import::create_output(&options.output, version) {
                return Err(stop(Failure::Write(e), &mut done, false));
            }
            created = true;
        }
        let sink = SinkOptions {
            version,
            sync: options.sync,
            allow_undetectable_readers: options.allow_undetectable_readers,
            fault: options
                .fault
                .filter(|(at, _)| *at == done.len())
                .map(|(_, point)| point),
        };
        let written = sink_dss::write_record(
            &options.output,
            &item.planned,
            &item.sink_crs,
            place,
            &parts.cells,
            parts.nx,
            parts.ny,
            &sink,
        );
        match written {
            Ok(record) => done.push(ToolRecord {
                imported: ImportedRecord {
                    input: input.to_path_buf(),
                    variable: item.variable.clone(),
                    step: item.step,
                    record,
                    window: None,
                },
                source_units: item.source_units.clone(),
                source_data_type: item.source_type,
                converted: item.planned.meaning.conversion != Conversion::None,
            }),
            Err(e) => return Err(stop(Failure::Write(e), &mut done, true)),
        }
    }
    Ok(done)
}

// A tool that keeps the input's grid and only changes its cells.
struct CellTool<F: FnMut(&[f32], Option<f32>) -> Result<Vec<f32>, VortexError>> {
    compute: F,
}

impl<F: FnMut(&[f32], Option<f32>) -> Result<Vec<f32>, VortexError>> Tool for CellTool<F> {
    fn destination(&mut self, def: &SourceGridDef) -> Result<GeoTransform, VortexError> {
        Ok(def.transform)
    }

    fn apply(&mut self, _index: usize, frame: GridFrame) -> Result<GridFrame, VortexError> {
        let cells = (self.compute)(frame.cells(), frame.parts().nodata)?;
        with_cells(frame, cells)
    }
}

fn run_simple(
    input: &Path,
    variables: Option<&[String]>,
    options: &OutputOptions,
    tool: &mut dyn Tool,
) -> Result<Vec<ToolRecord>, ImportError> {
    let (version, existed) =
        plan_output(options, &[input]).map_err(|e| at(e, input, None, None))?;
    let (source, is_dss) = open_any(input).map_err(|e| at(planning(e), input, None, None))?;
    let chosen = select_variables(source.as_ref(), variables)
        .map_err(|e| at(planning(e), input, None, None))?;
    let items = plan_items(input, source.as_ref(), is_dss, &chosen, options, version)?;
    execute(
        input,
        source.as_ref(),
        &items,
        options,
        version,
        existed,
        tool,
    )
}

/// `BatchCalculator`: one operation by one constant. The value is a float, as there, and must be
/// finite once it is one; Vortex takes NaN to mean no operation, and its builder then refuses.
pub fn calculate(
    input: &Path,
    variables: Option<&[String]>,
    operation: Operation,
    value: f64,
    options: &OutputOptions,
) -> Result<Vec<ToolRecord>, ImportError> {
    let value = value as f32;
    if !value.is_finite() {
        return Err(bad_request(format!(
            "the constant must be finite in single precision, and is {value}"
        )));
    }
    let mut tool = CellTool {
        compute: |cells: &[f32], marker| cells::with_constant(cells, marker, operation, value),
    };
    run_simple(input, variables, options, &mut tool)
}

/// `BatchSanitizer`.
pub fn sanitize(
    input: &Path,
    variables: Option<&[String]>,
    thresholds: Thresholds,
    options: &OutputOptions,
) -> Result<Vec<ToolRecord>, ImportError> {
    if thresholds.minimum.is_nan() || thresholds.maximum.is_nan() {
        return Err(bad_request(
            "a threshold is NaN, and so would replace nothing. Observed behaviour we did not \
             implement: Vortex then rewrites the grid unchanged"
                .into(),
        ));
    }
    if thresholds.minimum_replacement.is_infinite() || thresholds.maximum_replacement.is_infinite()
    {
        return Err(bad_request(
            "a replacement is infinite, which a DSS grid cannot hold. NaN means missing".into(),
        ));
    }
    // HEC's marker means missing, as NaN does. These two would be written as values and read as missing.
    for value in [
        thresholds.minimum_replacement,
        thresholds.maximum_replacement,
    ] {
        if value == -901.0 || value == -902.0 {
            return Err(bad_request(format!(
                "a replacement of {value} cannot be held as a value: readers take as missing what \
                 would be written as one. Use NaN for missing. Observed behaviour we did not \
                 implement: Vortex 1.1.0 and 0.13.3 store the number"
            )));
        }
    }
    let mut tool = CellTool {
        compute: |cells: &[f32], marker| cells::sanitize(cells, marker, &thresholds),
    };
    run_simple(input, variables, options, &mut tool)
}

fn warp_limits() -> WarpLimits {
    let limits = ImportLimits::default();
    WarpLimits {
        max_cells: limits.max_destination_cells,
        max_memory_bytes: limits.max_warp_bytes,
        ..WarpLimits::default()
    }
}

fn plan_of(def: &SourceGridDef, spec: &WarpSpec) -> Result<WarpPlan, VortexError> {
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

fn transform_of_plan(plan: &WarpPlan) -> GeoTransform {
    let g = plan.geotransform();
    GeoTransform {
        origin_x: g[0],
        dx: g[1],
        row_rotation: g[2],
        origin_y: g[3],
        column_rotation: g[4],
        dy: g[5],
    }
}

// The raster, resampled once onto the input's grid (`BatchGridCalculator.getRaster`).
struct RasterTool {
    raster_cells: Vec<f32>,
    raster_marker: Option<f32>,
    def: SourceGridDef,
    operation: Operation,
}

impl Tool for RasterTool {
    fn destination(&mut self, def: &SourceGridDef) -> Result<GeoTransform, VortexError> {
        if *def != self.def {
            return Err(refused(
                "raster calculator",
                "the selected grids are not all on one grid, and the raster was resampled for the \
                 first. Observed behaviour we did not implement: Vortex resamples for the first \
                 grid only, and applies it to any other of the same size",
            ));
        }
        Ok(def.transform)
    }

    fn apply(&mut self, _index: usize, frame: GridFrame) -> Result<GridFrame, VortexError> {
        let cells = cells::with_raster(
            frame.cells(),
            frame.parts().nodata,
            &self.raster_cells,
            self.raster_marker,
            self.operation,
        )?;
        with_cells(frame, cells)
    }
}

fn one_raster_step(
    raster: &dyn GridSource,
    stated: Option<&str>,
) -> Result<(String, usize), VortexError> {
    let variables = raster.variables();
    let variable = match stated {
        Some(name) if variables.iter().any(|v| v == name) => name.to_string(),
        Some(name) => {
            return Err(refused(
                format!("raster variable {name}"),
                format!("{} does not hold it", raster.file_name()),
            ))
        }
        None if variables.len() == 1 => variables[0].clone(),
        None => {
            return Err(refused(
                format!("raster {}", raster.file_name()),
                format!(
                    "it holds {} variables. State which. Observed behaviour we did not implement: \
                     Vortex takes the first",
                    variables.len()
                ),
            ))
        }
    };
    let steps = raster.step_count(&variable)?;
    if steps != 1 {
        return Err(refused(
            format!("raster variable {variable}"),
            format!(
                "it holds {steps} frames. Observed behaviour we did not implement: Vortex takes \
                 the first"
            ),
        ));
    }
    Ok((variable, 0))
}

// A timed DSS raster belongs to one time. A DSS raster is this library's own extension: Vortex 1.1.0 and
// 0.13.3 fail on one before computing (`getRaster` builds a `DataReader` with no variable name), and
// their rasters have no time to compare.
fn check_raster_times(
    raster_is_dss: bool,
    raster_variable: &str,
    raster_time: &FrameTime,
    items: &[Item],
    request: &RasterRequest,
) -> Result<(), VortexError> {
    if !raster_is_dss || request.static_raster || *raster_time == FrameTime::Invariant {
        return Ok(());
    }
    let other = items
        .iter()
        .find(|i| i.time != FrameTime::Invariant && i.time != *raster_time);
    if let Some(item) = other {
        return Err(refused(
            format!("raster {}", request.raster.display()),
            format!(
                "its record {raster_variable} and the input grid {} state different times. To reuse \
                 one grid as a factor, pass static_raster=True. Vortex 1.1.0 and 0.13.3 read no DSS \
                 raster at all, so they offer no behaviour to follow here",
                item.variable
            ),
        ));
    }
    Ok(())
}

fn resample_raster(
    request: &RasterRequest,
    items: &[Item],
    input_def: &SourceGridDef,
    align: Align,
) -> Result<(Vec<f32>, Option<f32>), VortexError> {
    let (raster, raster_is_dss) = open_any(&request.raster)?;
    let (variable, step) = one_raster_step(raster.as_ref(), request.raster_variable.as_deref())?;
    let frame = raster.frame(&variable, step)?;
    check_raster_times(
        raster_is_dss,
        &variable,
        &frame.parts().time,
        items,
        request,
    )?;
    let target = CrsDef::from_frame(&input_def.crs)?;
    let t = input_def.transform;
    // `getRaster`: `originX`, `getTerminusX`, and the two Y ends, in the input's CRS.
    let (end_x, end_y) = (
        t.origin_x + t.dx * input_def.nx as f64,
        t.origin_y + t.dy * input_def.ny as f64,
    );
    let spec = WarpSpec {
        target: Some(target.clone()),
        cell_size: None,
        envelope: Some(WarpEnvelope {
            envelope: Envelope {
                min_x: t.origin_x,
                max_x: end_x,
                min_y: t.origin_y.min(end_y),
                max_y: t.origin_y.max(end_y),
            },
            crs: Some(target),
        }),
        method: request.method,
        align,
        datum_policy: DatumPolicy::default(),
        threads: 1,
        limits: warp_limits(),
    };
    let plan = WarpPlan::for_frame(&frame, &spec)?;
    // `GridCalculator.Builder.build` throws on a length mismatch. The same length on another grid
    // would combine cells that are not in the same place.
    if plan.nx() != input_def.nx || plan.ny() != input_def.ny || transform_of_plan(&plan) != t {
        return Err(refused(
            format!("raster {}", request.raster.display()),
            format!(
                "it resamples to {} x {} at {:?}, and the input is {} x {} at {:?}. Vortex 1.1.0 and \
                 0.13.3 refuse a different size too",
                plan.nx(),
                plan.ny(),
                transform_of_plan(&plan),
                input_def.nx,
                input_def.ny,
                t
            ),
        ));
    }
    let (warped, collisions) = plan.warp_frame_counted(&frame)?;
    if collisions > 0 {
        return Err(refused(
            format!("raster {}", request.raster.display()),
            format!(
                "{collisions} cell(s) computed from valid data came out equal to its nodata marker, \
                 and would read as missing"
            ),
        ));
    }
    let marker = warped.parts().nodata;
    Ok((warped.into_parts().cells, marker))
}

/// `BatchGridCalculator`: the input, on the left, with a raster resampled to its grid.
pub fn calculate_raster(
    input: &Path,
    variables: Option<&[String]>,
    request: &RasterRequest,
    options: &OutputOptions,
) -> Result<Vec<ToolRecord>, ImportError> {
    let (version, existed) =
        plan_output(options, &[input, &request.raster]).map_err(|e| at(e, input, None, None))?;
    let (source, is_dss) = open_any(input).map_err(|e| at(planning(e), input, None, None))?;
    let chosen = select_variables(source.as_ref(), variables)
        .map_err(|e| at(planning(e), input, None, None))?;
    let items = plan_items(input, source.as_ref(), is_dss, &chosen, options, version)?;
    let first = items
        .first()
        .ok_or_else(|| planning(refused("input", "the selection holds no frames")))?;
    let (raster_cells, raster_marker) = resample_raster(request, &items, &first.def, options.align)
        .map_err(|e| at(planning(e), &request.raster, None, None))?;
    let mut tool = RasterTool {
        raster_cells,
        raster_marker,
        def: first.def.clone(),
        operation: request.operation,
    };
    execute(
        input,
        source.as_ref(),
        &items,
        options,
        version,
        existed,
        &mut tool,
    )
}

// `SubsettableUnit.process`.
struct ClipTool {
    envelope: WarpEnvelope,
    align: Align,
    plan: Option<(SourceGridDef, WarpPlan)>,
}

impl ClipTool {
    fn plan_for(&mut self, def: &SourceGridDef) -> Result<&WarpPlan, VortexError> {
        if self.plan.as_ref().is_none_or(|(known, _)| known != def) {
            let crs = CrsDef::from_frame(&def.crs)?;
            if crs.is_geographic() {
                return Err(refused(
                    "clip",
                    "the grid is in a geographic CRS. Vortex 1.1.0 and 0.13.3 do not clip these: \
                     they log it and write nothing",
                ));
            }
            let spec = WarpSpec {
                target: None,
                cell_size: None,
                envelope: Some(self.envelope.clone()),
                method: Method::Near,
                align: self.align,
                datum_policy: DatumPolicy::default(),
                threads: 1,
                limits: warp_limits(),
            };
            self.plan = Some((def.clone(), plan_of(def, &spec)?));
        }
        match &self.plan {
            Some((_, plan)) => Ok(plan),
            None => Err(refused("clip", "no plan")),
        }
    }
}

impl Tool for ClipTool {
    fn destination(&mut self, def: &SourceGridDef) -> Result<GeoTransform, VortexError> {
        let plan = self.plan_for(def)?;
        let g = transform_of_plan(plan);
        let (nx, ny) = (plan.nx(), plan.ny());
        if (nx * ny) as u64 > ImportLimits::default().max_destination_cells {
            return Err(refused(
                "clip",
                format!("{nx} x {ny} is over the writer's limit"),
            ));
        }
        Ok(g)
    }

    fn apply(&mut self, _index: usize, frame: GridFrame) -> Result<GridFrame, VortexError> {
        let p = frame.parts();
        let def = SourceGridDef {
            nx: p.nx,
            ny: p.ny,
            transform: p.transform,
            crs: p.crs.clone(),
            nodata: p.nodata,
            stored_wkt: None,
        };
        let plan = self.plan_for(&def)?;
        let (warped, collisions) = plan.warp_frame_counted(&frame)?;
        if collisions > 0 {
            return Err(refused(
                "clip",
                format!("{collisions} cell(s) came out equal to the nodata marker"),
            ));
        }
        Ok(warped)
    }
}

/// `BatchSubsetter`: the grids clipped to an envelope in their own CRS, by nearest neighbour. The
/// extent is used as given, with no cell size to snap it, so one not on whole cells is refused unless
/// `align` is [`Align::Vortex`] (decision V6).
pub fn clip(
    input: &Path,
    variables: Option<&[String]>,
    request: &ClipRequest,
    options: &OutputOptions,
) -> Result<Vec<ToolRecord>, ImportError> {
    let (envelope, crs) = match &request.bounds {
        ClipBounds::Shapefile(path) => {
            import::shapefile_envelope(path).map_err(|e| at(planning(e), path, None, None))?
        }
        ClipBounds::Envelope { envelope, crs } => (
            *envelope,
            CrsDef::from_frame(crs).map_err(|e| at(planning(e), input, None, None))?,
        ),
    };
    if !(envelope.min_x < envelope.max_x && envelope.min_y < envelope.max_y) {
        return Err(at(
            planning(refused("envelope", format!("{envelope:?} is empty"))),
            input,
            None,
            None,
        ));
    }
    let mut tool = ClipTool {
        envelope: WarpEnvelope {
            envelope,
            crs: Some(crs),
        },
        align: options.align,
        plan: None,
    };
    run_simple(input, variables, options, &mut tool)
}

fn seconds((julian, seconds): (i64, i64)) -> i128 {
    i128::from(julian) * 86_400 + i128::from(seconds)
}

// `(start, end)` of a frame, or `None` for one with no time, on which `Normalizer` fails.
fn window(time: &FrameTime) -> Option<(i128, i128)> {
    match *time {
        FrameTime::Invariant => None,
        FrameTime::Instant(t) => Some((seconds(t), seconds(t))),
        FrameTime::Period { start, end } => Some((seconds(start), seconds(end))),
    }
}

struct Frames {
    variable: String,
    step: usize,
}

struct Group {
    sources: Vec<Frames>,
    normals: Vec<Frames>,
}

struct NormalizeTool<'a> {
    source: &'a dyn GridSource,
    normals: &'a dyn GridSource,
    groups: Vec<Group>,
    /// Each item's group.
    group_of: Vec<usize>,
    current: Option<(usize, Vec<f32>, Vec<f32>)>,
    /// Each item's cells made missing for want of a normal. By item, as planning computes each frame
    /// once and writing again.
    made_missing: Vec<u64>,
}

impl NormalizeTool<'_> {
    fn totals(&mut self, group: usize, len: usize) -> Result<(), VortexError> {
        if self.current.as_ref().is_some_and(|(g, _, _)| *g == group) {
            return Ok(());
        }
        let g = &self.groups[group];
        let mut source_totals = cells::zero_totals(len)?;
        for f in &g.sources {
            let frame = self.source.frame(&f.variable, f.step)?;
            cells::add_source(&mut source_totals, frame.cells(), frame.parts().nodata)?;
        }
        let mut normal_totals = cells::zero_totals(len)?;
        for f in &g.normals {
            let frame = self.normals.frame(&f.variable, f.step)?;
            cells::add_normal(&mut normal_totals, frame.cells(), frame.parts().nodata)?;
        }
        self.current = Some((group, source_totals, normal_totals));
        Ok(())
    }
}

impl Tool for NormalizeTool<'_> {
    fn destination(&mut self, def: &SourceGridDef) -> Result<GeoTransform, VortexError> {
        Ok(def.transform)
    }

    fn apply(&mut self, index: usize, frame: GridFrame) -> Result<GridFrame, VortexError> {
        self.totals(self.group_of[index], frame.cells().len())?;
        let Some((_, source_totals, normal_totals)) = &self.current else {
            return Err(refused("normalizer", "no totals"));
        };
        let (cells, made_missing) = cells::normalize_counted(
            frame.cells(),
            frame.parts().nodata,
            source_totals,
            normal_totals,
        )?;
        self.made_missing[index] = made_missing;
        with_cells(frame, cells)
    }
}

// `Normalizer.validate`, and origin, CRS and units as well, which it does not compare.
fn check_agree(
    reference: &SourceGridDef,
    reference_units: &str,
    other: &SourceGridDef,
    units: &str,
    name: &str,
) -> Result<(), VortexError> {
    let same_grid = (reference.nx, reference.ny) == (other.nx, other.ny)
        && reference.transform == other.transform
        && reference.crs == other.crs;
    if !same_grid {
        return Err(refused(
            format!("grid {name}"),
            format!(
                "it is {} x {} at {:?}, CRS {:?}; the interval's first source is {} x {} at {:?}, \
                 CRS {:?}. Observed behaviour we did not implement: Vortex compares only the size \
                 and spacing, and leaves the whole interval out if those differ",
                other.nx,
                other.ny,
                other.transform,
                other.crs,
                reference.nx,
                reference.ny,
                reference.transform,
                reference.crs
            ),
        ));
    }
    let same_units = match (
        units::parse_unit(Some(reference_units)),
        units::parse_unit(Some(units)),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => reference_units.eq_ignore_ascii_case(units),
    };
    if !same_units {
        return Err(refused(
            format!("units of {name}"),
            format!(
                "{units:?} against {reference_units:?}, and a scale needs one unit. Observed \
                 behaviour we did not implement: Vortex divides the numbers as they are"
            ),
        ));
    }
    Ok(())
}

/// `Normalizer.normalize`: each interval's source grids, scaled so their total is its normal grids'.
///
/// Totals sum in the order `DssDataReader.getVariables` gives ([`java_order`]), as Vortex does, since
/// that order changes the last bits of a float sum.
pub fn normalize(
    input: &Path,
    request: &NormalizeRequest,
    options: &OutputOptions,
) -> Result<NormalizeOutcome, ImportError> {
    let (start, end) = (seconds(request.start), seconds(request.end));
    if request.interval_seconds <= 0 || start >= end {
        return Err(bad_request(format!(
            "start must precede end and the interval must be positive; interval is {} s",
            request.interval_seconds
        )));
    }
    let interval = i128::from(request.interval_seconds);
    let (version, existed) =
        plan_output(options, &[input, &request.normals]).map_err(|e| at(e, input, None, None))?;
    let (source, is_dss) = open_any(input).map_err(|e| at(planning(e), input, None, None))?;
    let (normals, _) =
        open_any(&request.normals).map_err(|e| at(planning(e), &request.normals, None, None))?;
    let chosen = select_variables(source.as_ref(), request.source_variables.as_deref())
        .map_err(|e| at(planning(e), input, None, None))?;
    let chosen_normals = select_variables(normals.as_ref(), request.normals_variables.as_deref())
        .map_err(|e| at(planning(e), &request.normals, None, None))?;
    let (chosen, long_source) = java_order::hash_set_order(&chosen);
    let (chosen_normals, long_normals) = java_order::hash_set_order(&chosen_normals);

    let all_sources = plan_items(input, source.as_ref(), is_dss, &chosen, options, version)?;
    let mut all_normals = Vec::new();
    for variable in &chosen_normals {
        let path = &request.normals;
        let steps = normals
            .step_count(variable)
            .map_err(|e| at(planning(e), path, Some(variable), None))?;
        for step in 0..steps {
            let a = |e| at(planning(e), path, Some(variable), Some(step));
            let meta = normals.step_meta(variable, step).map_err(a)?;
            let def = normals.grid(variable, step).map_err(a)?;
            all_normals.push((variable.clone(), step, meta, def));
        }
    }

    let time_refusal = |name: &str| {
        refused(
            format!("grid {name}"),
            "it has no time, so no interval can hold it. Vortex 1.1.0 and 0.13.3 fail on it too",
        )
    };
    let mut source_windows = Vec::with_capacity(all_sources.len());
    for item in &all_sources {
        let w = window(&item.time).ok_or_else(|| {
            at(
                planning(time_refusal(&item.variable)),
                input,
                Some(&item.variable),
                Some(item.step),
            )
        })?;
        source_windows.push(w);
    }
    let mut normal_windows = Vec::with_capacity(all_normals.len());
    for (variable, step, meta, _) in &all_normals {
        let w = window(&meta.time).ok_or_else(|| {
            at(
                planning(time_refusal(variable)),
                &request.normals,
                Some(variable),
                Some(*step),
            )
        })?;
        normal_windows.push(w);
    }

    // `normalize()`'s loop: while the interval's start is before the end, whole grids only.
    let mut groups: Vec<Group> = Vec::new();
    let mut notes = Vec::new();
    let mut order: Vec<usize> = Vec::new();
    let mut group_of: Vec<usize> = Vec::new();
    let mut source_used = vec![false; all_sources.len()];
    let mut normal_used = vec![false; all_normals.len()];
    let mut from = start;
    while from < end {
        let to = from + interval;
        let inside = |w: &(i128, i128)| w.0 >= from && w.1 <= to;
        let in_sources: Vec<usize> = (0..all_sources.len())
            .filter(|i| inside(&source_windows[*i]))
            .collect();
        let in_normals: Vec<usize> = (0..all_normals.len())
            .filter(|i| inside(&normal_windows[*i]))
            .collect();
        notes.push(IntervalNote {
            start: from,
            end: to,
            sources: in_sources.len(),
            normals: in_normals.len(),
        });
        if !in_sources.is_empty() && in_normals.is_empty() {
            let e = refused(
                "normalizer",
                format!(
                    "an interval holds {} source grid(s) and no normals, starting {} s after the \
                     run's start. Observed behaviour we did not implement: Vortex logs a warning and \
                     writes nothing for it",
                    in_sources.len(),
                    from - start
                ),
            );
            return Err(at(planning(e), &request.normals, None, None));
        }
        if !in_sources.is_empty() {
            let first = &all_sources[in_sources[0]];
            for &i in &in_sources {
                let item = &all_sources[i];
                check_agree(
                    &first.def,
                    &first.source_units,
                    &item.def,
                    &item.source_units,
                    &item.variable,
                )
                .map_err(|e| at(planning(e), input, Some(&item.variable), Some(item.step)))?;
            }
            for &i in &in_normals {
                let (variable, step, meta, def) = &all_normals[i];
                check_agree(&first.def, &first.source_units, def, &meta.units, variable)
                    .map_err(|e| at(planning(e), &request.normals, Some(variable), Some(*step)))?;
            }
            let group = groups.len();
            for &i in &in_sources {
                source_used[i] = true;
                order.push(i);
                group_of.push(group);
            }
            for &i in &in_normals {
                normal_used[i] = true;
            }
            groups.push(Group {
                sources: in_sources
                    .iter()
                    .map(|&i| Frames {
                        variable: all_sources[i].variable.clone(),
                        step: all_sources[i].step,
                    })
                    .collect(),
                normals: in_normals
                    .iter()
                    .map(|&i| Frames {
                        variable: all_normals[i].0.clone(),
                        step: all_normals[i].1,
                    })
                    .collect(),
            });
        }
        from = to;
    }
    if groups.is_empty() {
        return Err(at(
            planning(refused(
                "normalizer",
                "no source grid lies wholly inside any interval, so nothing would be written",
            )),
            input,
            None,
            None,
        ));
    }
    let left_out = |names: Vec<&str>| names.into_iter().map(str::to_string).collect::<Vec<_>>();
    let sources_left_out = left_out(
        (0..all_sources.len())
            .filter(|i| !source_used[*i])
            .map(|i| all_sources[i].variable.as_str())
            .collect(),
    );
    let normals_left_out = left_out(
        (0..all_normals.len())
            .filter(|i| !normal_used[*i])
            .map(|i| all_normals[i].0.as_str())
            .collect(),
    );

    // Items in the order Vortex writes them. Consumed once each, unless an instant on an interval's
    // edge lands in two, which `check_records` then refuses.
    let mut slots: Vec<Option<Item>> = all_sources.into_iter().map(Some).collect();
    let mut items = Vec::with_capacity(order.len());
    let mut groups_of_items = Vec::with_capacity(order.len());
    for (&i, &g) in order.iter().zip(&group_of) {
        match slots[i].take() {
            Some(item) => {
                items.push(item);
                groups_of_items.push(g);
            }
            None => {
                return Err(at(
                    planning(refused(
                        "normalizer",
                        "a source grid lies in two intervals, at an instant on their shared edge, \
                         and would be written twice. Observed behaviour we did not implement: Vortex \
                         keeps the later one",
                    )),
                    input,
                    None,
                    None,
                ))
            }
        }
    }
    let mut tool = NormalizeTool {
        source: source.as_ref(),
        normals: normals.as_ref(),
        groups,
        made_missing: vec![0; groups_of_items.len()],
        group_of: groups_of_items,
        current: None,
    };
    let records = execute(
        input,
        source.as_ref(),
        &items,
        options,
        version,
        existed,
        &mut tool,
    )?;
    Ok(NormalizeOutcome {
        records,
        intervals: notes,
        sources_left_out,
        normals_left_out,
        long_hash_bin: long_source || long_normals,
        cells_made_missing_by_normal: tool.made_missing.iter().sum(),
    })
}

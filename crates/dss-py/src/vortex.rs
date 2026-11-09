//! Bindings for the gridded importer (`dss_vortex::import`). The public API is
//! `python/ras_commander_fast_dss/gridded/`; this module moves data and maps errors.
//!
//! Imports run with the GIL released, as other writes do here. A progress callback takes the GIL
//! for its call only. Errors use the classes in `ras_commander_fast_dss.exceptions`, with `phase`,
//! `input`, `variable`, `frame` and `completed` set on the exception.

use super::{ctx, write_err, BindErr, DssError, DssFormatError, DssUnsupportedRecordError};
use dss_vortex::import::{
    self, ArrayInput, Bounds, Duplicates, Failure, ImportError, ImportParts, ImportRequest,
    ImportedRecord, Product, Progress, Selection,
};
use dss_vortex::import_netcdf::{NetcdfOptions, StopHandle};
use dss_vortex::products::window::Accumulation;
use dss_vortex::products::WindowRule;
use dss_vortex::warp::{Align, Method};
use dss_vortex::{Crs, DataType, FrameTime, GeoTransform, NamedCrs, VortexError};

use pyo3::exceptions::{PyMemoryError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Mutex;

pub(super) fn crs_of(text: &str) -> Crs {
    match text.to_ascii_uppercase().as_str() {
        "SHG" => Crs::Named(NamedCrs::Shg),
        "HRAP" => Crs::Named(NamedCrs::Hrap),
        _ => Crs::Wkt(text.to_string()),
    }
}

pub(super) fn data_type_of(text: Option<&str>) -> PyResult<Option<DataType>> {
    Ok(match text {
        None => None,
        Some("PER-AVER") => Some(DataType::PerAver),
        Some("PER-CUM") => Some(DataType::PerCum),
        Some("INST-VAL") => Some(DataType::InstVal),
        Some("INST-CUM") => Some(DataType::InstCum),
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "data_type must be PER-AVER, PER-CUM, INST-VAL or INST-CUM, not {other:?}"
            )))
        }
    })
}

pub(super) fn data_type_name(t: DataType) -> &'static str {
    match t {
        DataType::PerAver => "PER-AVER",
        DataType::PerCum => "PER-CUM",
        DataType::InstVal => "INST-VAL",
        DataType::InstCum => "INST-CUM",
    }
}

pub(super) fn record_dict<'py>(
    py: Python<'py>,
    r: &ImportedRecord,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("input", r.input.to_string_lossy().as_ref())?;
    d.set_item("variable", &r.variable)?;
    d.set_item("frame", r.step)?;
    let w = &r.record;
    d.set_item("pathname", &w.pathname)?;
    d.set_item("record_type", w.record_type)?;
    d.set_item("outcome", &w.outcome)?;
    d.set_item("data_type", data_type_name(w.data_type))?;
    d.set_item("units", &w.units)?;
    d.set_item("c_part_source", w.c_part_source.as_str())?;
    d.set_item("nx", w.nx)?;
    d.set_item("ny", w.ny)?;
    d.set_item("lower_left_cell_x", w.lower_left_x)?;
    d.set_item("lower_left_cell_y", w.lower_left_y)?;
    d.set_item("cell_size", w.cell_size)?;
    d.set_item("readers_undetected", w.readers_undetected)?;
    let note = r.window.as_ref();
    d.set_item("accumulation_seconds", note.map(|n| n.seconds))?;
    d.set_item("accumulation_stated_by", note.map(|n| n.stated_by))?;
    d.set_item("overlaps_earlier", note.map(|n| n.overlaps_earlier))?;
    d.set_item("gap_before", note.map(|n| n.gap_before))?;
    Ok(d)
}

fn records_list<'py>(py: Python<'py>, records: &[ImportedRecord]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for r in records {
        list.append(record_dict(py, r)?)?;
    }
    Ok(list)
}

pub(super) fn import_err(py: Python<'_>, output: &str, e: ImportError) -> PyErr {
    let message = e.to_string();
    let completed = records_list(py, &e.completed).ok().map(|l| l.unbind());
    let (phase, input, variable, frame) = (e.phase, e.input.clone(), e.variable.clone(), e.step);
    let in_doubt = e.in_doubt.clone();
    let raised = match e.failure {
        Failure::Vortex(VortexError::Io(io)) => {
            let path = input
                .as_ref()
                .map_or(output.to_string(), |p| p.to_string_lossy().into_owned());
            BindErr::Os(io, path).into_py()
        }
        Failure::Vortex(VortexError::Decode { .. }) => DssFormatError::new_err(message),
        Failure::Vortex(_) => DssUnsupportedRecordError::new_err(message),
        Failure::Write(w) => {
            let original = write_err(py, output, w);
            // Keep the class and its attributes; say where the run stopped.
            let v = original.value(py);
            let _ = v.setattr("args", (message,));
            original
        }
        Failure::Cancelled | Failure::Stopped => DssError::new_err(message),
        Failure::BadRequest(m) => PyValueError::new_err(ctx(output, m)),
    };
    let v = raised.value(py);
    let _ = v.setattr("phase", phase);
    let _ = v.setattr("input", input.map(|p| p.to_string_lossy().into_owned()));
    let _ = v.setattr("variable", variable);
    let _ = v.setattr("frame", frame);
    let _ = v.setattr("completed", completed);
    let _ = v.setattr("in_doubt", in_doubt);
    raised
}

pub(super) struct Options {
    bounds: Bounds,
    target: Option<Crs>,
    cell_size: Option<f64>,
    method: Method,
    parts: ImportParts,
    units: Option<String>,
    data_type: Option<DataType>,
    version: Option<u8>,
    align: Align,
    window_rule: WindowRule,
    duplicates: Duplicates,
    threads: usize,
    sync: bool,
    allow_undetectable_readers: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn options(
    envelope: Option<(f64, f64, f64, f64)>,
    envelope_crs: Option<&str>,
    shapefile: Option<&str>,
    target_crs: Option<&str>,
    cell_size: Option<f64>,
    method: &str,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    window_rule: &str,
    duplicates: &str,
    threads: usize,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<Options> {
    let bounds = match (envelope, shapefile) {
        (Some(_), Some(_)) => {
            return Err(PyValueError::new_err(
                "give an envelope or a shapefile, not both",
            ))
        }
        (Some((min_x, min_y, max_x, max_y)), None) => Bounds::Envelope {
            envelope: dss_vortex::crs::Envelope {
                min_x,
                max_x,
                min_y,
                max_y,
            },
            crs: envelope_crs.map(crs_of),
        },
        (None, Some(path)) => Bounds::Shapefile(PathBuf::from(path)),
        (None, None) => Bounds::None,
    };
    let method = match method {
        "near" => Method::Near,
        "bilinear" => Method::Bilinear,
        "average" => Method::Average,
        other => {
            return Err(PyValueError::new_err(format!(
                "method must be near, bilinear or average, not {other:?}"
            )))
        }
    };
    let align = match align {
        "refuse" => Align::Refuse,
        "vortex" => Align::Vortex,
        other => {
            return Err(PyValueError::new_err(format!(
                "align must be \"refuse\" or \"vortex\", not {other:?}"
            )))
        }
    };
    let window_rule = match window_rule {
        "refuse" => WindowRule::Refuse,
        "vortex" => WindowRule::Vortex,
        other => match other
            .strip_prefix("accumulation:")
            .and_then(Accumulation::parse)
        {
            Some(wanted) => WindowRule::Accumulation(wanted),
            None => {
                return Err(PyValueError::new_err(format!(
                    "window_rule must be \"refuse\", \"vortex\" or \"accumulation:<duration>\", \
                     with a duration of 15M, 01H, 03H, 06H, 12H, 24H, 48H or 72H, not {other:?}"
                )))
            }
        },
    };
    let duplicates = match duplicates {
        "refuse" => Duplicates::Refuse,
        "last" => Duplicates::Last,
        other => {
            return Err(PyValueError::new_err(format!(
                "duplicates must be \"refuse\" or \"last\", not {other:?}"
            )))
        }
    };
    Ok(Options {
        bounds,
        target: target_crs.map(crs_of),
        cell_size,
        method,
        parts: ImportParts {
            a: part_a.to_string(),
            b: part_b.to_string(),
            c: part_c.map(str::to_string),
            f: part_f.to_string(),
        },
        units: units.map(str::to_string),
        data_type: data_type_of(data_type)?,
        version,
        align,
        window_rule,
        duplicates,
        threads,
        sync,
        allow_undetectable_readers,
    })
}

pub(super) fn request_of(
    inputs: Vec<PathBuf>,
    output: &str,
    selection: Selection,
    o: Options,
) -> ImportRequest {
    let mut r = ImportRequest::new(inputs, PathBuf::from(output), selection, o.method);
    r.bounds = o.bounds;
    r.target = o.target;
    r.cell_size = o.cell_size;
    r.parts = o.parts;
    r.units = o.units;
    r.data_type = o.data_type;
    r.version = o.version;
    r.align = o.align;
    r.window_rule = o.window_rule;
    r.duplicates = o.duplicates;
    r.threads = o.threads;
    r.sync = o.sync;
    r.allow_undetectable_readers = o.allow_undetectable_readers;
    r
}

/// A stop for a run in progress. Pass it as `stop=` to `import_gridded`, keep it, and
/// call `request()` from another thread: the run ends before its next frame, and a NetCDF read in
/// progress before its next chunk.
#[pyclass(
    frozen,
    module = "ras_commander_fast_dss._native",
    name = "GriddedStop"
)]
pub(super) struct GriddedStop {
    handle: StopHandle,
}

#[pymethods]
impl GriddedStop {
    #[new]
    fn new() -> Self {
        GriddedStop {
            handle: StopHandle::new(),
        }
    }

    /// Ask the run to stop. Safe from any thread, and before the run starts.
    fn request(&self) {
        self.handle.request();
    }

    /// Withdraw the request, so the handle can serve another run.
    fn clear(&self) {
        self.handle.clear();
    }

    #[getter]
    fn requested(&self) -> bool {
        self.handle.is_requested()
    }

    fn __repr__(&self) -> String {
        let requested = if self.handle.is_requested() {
            "True"
        } else {
            "False"
        };
        format!("GriddedStop(requested={requested})")
    }
}

// A callback error stays in Python's hands: it is raised as itself, with the run's records on it.
pub(super) struct Callback {
    pub(super) function: Option<PyObject>,
    pub(super) error: Mutex<Option<PyErr>>,
}

impl Callback {
    pub(super) fn call(&self, p: &Progress) -> ControlFlow<()> {
        let Some(function) = &self.function else {
            return ControlFlow::Continue(());
        };
        Python::with_gil(|py| {
            let d = PyDict::new(py);
            let filled = (|| -> PyResult<()> {
                d.set_item("done", p.done)?;
                d.set_item("total", p.total)?;
                d.set_item("input", p.input)?;
                d.set_item("variable", &p.variable)?;
                d.set_item("frame", p.step)?;
                d.set_item("pathname", &p.pathname)?;
                Ok(())
            })();
            match filled.and_then(|_| function.call1(py, (d,))) {
                Ok(_) => ControlFlow::Continue(()),
                Err(e) => {
                    if let Ok(mut slot) = self.error.lock() {
                        *slot = Some(e);
                    }
                    ControlFlow::Break(())
                }
            }
        })
    }

    fn take_error(&self) -> Option<PyErr> {
        self.error.lock().ok().and_then(|mut slot| slot.take())
    }
}

pub(super) fn finish<'py>(
    py: Python<'py>,
    output: &str,
    result: Result<Vec<ImportedRecord>, ImportError>,
    callback: &Callback,
) -> PyResult<Bound<'py, PyList>> {
    // A callback's error wins whichever way the run ended: after the last record there is nothing
    // left to stop, so the run returns its records, and the exception must still reach the caller.
    let raised = callback.take_error();
    match (result, raised) {
        (Ok(records), None) => records_list(py, &records),
        (Ok(records), Some(original)) => {
            let v = original.value(py);
            let _ = v.setattr("completed", records_list(py, &records)?);
            Err(original)
        }
        (Err(e), Some(original)) => {
            let v = original.value(py);
            let _ = v.setattr("completed", records_list(py, &e.completed)?);
            Err(original)
        }
        (Err(e), None) => Err(import_err(py, output, e)),
    }
}

/// Import files; returns one dict per record written.
#[pyfunction]
#[pyo3(signature = (inputs, output, variable=None, product=None, envelope=None, envelope_crs=None,
                    shapefile=None, target_crs=None, cell_size=None, method="bilinear", part_a="",
                    part_b="", part_c=None, part_f="", units=None, data_type=None, version=None,
                    align="refuse", window_rule="refuse", duplicates="refuse", threads=1,
                    sync=true, allow_undetectable_readers=true, progress=None, bbox=None,
                    times=None, max_seconds_per_read=None, max_decoded_bytes_per_read=None,
                    chunk_cache_bytes=None, stop=None))]
#[allow(clippy::too_many_arguments)]
fn import_gridded<'py>(
    py: Python<'py>,
    inputs: Vec<String>,
    output: &str,
    variable: Option<String>,
    product: Option<&str>,
    envelope: Option<(f64, f64, f64, f64)>,
    envelope_crs: Option<&str>,
    shapefile: Option<&str>,
    target_crs: Option<&str>,
    cell_size: Option<f64>,
    method: &str,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    window_rule: &str,
    duplicates: &str,
    threads: usize,
    sync: bool,
    allow_undetectable_readers: bool,
    progress: Option<PyObject>,
    bbox: Option<(f64, f64, f64, f64)>,
    times: Option<(i64, i64)>,
    max_seconds_per_read: Option<u64>,
    max_decoded_bytes_per_read: Option<u64>,
    chunk_cache_bytes: Option<usize>,
    stop: Option<PyRef<'py, GriddedStop>>,
) -> PyResult<Bound<'py, PyList>> {
    let selection = match (variable, product) {
        (Some(v), None) => Selection::Variable(v),
        (None, Some("mrms_qpe")) => Selection::Product(Product::MrmsQpe),
        (None, Some("total_precipitation")) => Selection::Product(Product::TotalPrecipitation),
        (None, Some(other)) => {
            return Err(PyValueError::new_err(format!(
                "product must be mrms_qpe or total_precipitation, not {other:?}"
            )))
        }
        _ => {
            return Err(PyValueError::new_err(
                "give exactly one of variable, product",
            ))
        }
    };
    let o = options(
        envelope,
        envelope_crs,
        shapefile,
        target_crs,
        cell_size,
        method,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        window_rule,
        duplicates,
        threads,
        sync,
        allow_undetectable_readers,
    )?;
    let inputs: Vec<PathBuf> = inputs.into_iter().map(PathBuf::from).collect();
    let mut request = request_of(inputs, output, selection, o);
    request.netcdf = NetcdfOptions {
        max_seconds_per_read,
        max_decoded_bytes_per_read,
        chunk_cache_bytes,
        ..NetcdfOptions::default()
    };
    request.stop = stop.map(|s| s.handle.flag());
    // P49: what of a Zarr store to read. Other sources refuse them.
    request.source_bbox = bbox.map(|(west, south, east, north)| [west, south, east, north]);
    request.source_times = times;
    let callback = Callback {
        function: progress,
        error: Mutex::new(None),
    };
    let result = py.allow_threads(|| import::import_gridded(&request, &mut |p| callback.call(p)));
    finish(py, output, result, &callback)
}

/// What a cell size in metres becomes on a target CRS: the record's cell size in the CRS's unit,
/// and that size in metres (`dss_vortex::warp::CellSize`).
#[pyfunction]
#[pyo3(signature = (target_crs, metres, align="refuse"))]
fn gridded_cell_size(target_crs: &str, metres: f64, align: &str) -> PyResult<(f32, f64)> {
    let target = dss_vortex::crs::CrsDef::from_frame(&crs_of(target_crs))
        .map_err(|e| DssUnsupportedRecordError::new_err(e.to_string()))?;
    let align = match align {
        "vortex" => dss_vortex::warp::Align::Vortex,
        _ => dss_vortex::warp::Align::Refuse,
    };
    let size = dss_vortex::warp::CellSize::of(metres, &target, align);
    Ok((size.stored, size.stored_metres))
}

/// Import arrays. `times` holds one (start, end) in HEC seconds per frame, or `None` for a
/// time-invariant grid; start equal to end is an instant.
#[pyfunction]
#[pyo3(signature = (cells, nx, ny, transform, crs, nodata, times, variable, description, source_units,
                    output, file_name="", envelope=None, envelope_crs=None, shapefile=None,
                    target_crs=None, cell_size=None, method="bilinear", part_a="", part_b="",
                    part_c=None, part_f="", units=None, data_type=None, version=None,
                    align="refuse", duplicates="refuse", threads=1, sync=true,
                    allow_undetectable_readers=true, progress=None))]
#[allow(clippy::too_many_arguments)]
fn import_arrays<'py>(
    py: Python<'py>,
    cells: numpy::PyReadonlyArray1<'py, f32>,
    nx: usize,
    ny: usize,
    transform: (f64, f64, f64, f64),
    crs: &str,
    nodata: f32,
    times: Vec<Option<(i64, i64)>>,
    variable: String,
    description: Option<String>,
    source_units: String,
    output: &str,
    file_name: &str,
    envelope: Option<(f64, f64, f64, f64)>,
    envelope_crs: Option<&str>,
    shapefile: Option<&str>,
    target_crs: Option<&str>,
    cell_size: Option<f64>,
    method: &str,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    duplicates: &str,
    threads: usize,
    sync: bool,
    allow_undetectable_readers: bool,
    progress: Option<PyObject>,
) -> PyResult<Bound<'py, PyList>> {
    // Bounds first: past them no copy is attempted, and a system that grants any allocation (macOS does) would
    // kill the process partway through filling it.
    let limits = import::ImportLimits::default();
    if nx.saturating_mul(ny) > limits.max_source_cells {
        return Err(DssUnsupportedRecordError::new_err(format!(
            "{ny} x {nx} is over the limit of {} cells in a source grid",
            limits.max_source_cells
        )));
    }
    if times.len() > limits.max_records {
        return Err(DssUnsupportedRecordError::new_err(format!(
            "{} frames is over the limit of {} records",
            times.len(),
            limits.max_records
        )));
    }
    let split = |t: i64| (t.div_euclid(86_400), t.rem_euclid(86_400));
    let frame_times = times
        .into_iter()
        .map(|t| match t {
            None => FrameTime::Invariant,
            Some((s, e)) if s == e => FrameTime::Instant(split(e)),
            Some((s, e)) => FrameTime::Period {
                start: split(s),
                end: split(e),
            },
        })
        .collect();
    // `to_vec` aborts the process if the allocation fails, and `catch_unwind` cannot catch an abort.
    let view = cells.as_array();
    let mut copied: Vec<f32> = Vec::new();
    copied.try_reserve_exact(view.len()).map_err(|_| {
        PyMemoryError::new_err(format!(
            "cannot hold {} cells of import_arrays input",
            view.len()
        ))
    })?;
    copied.extend(view.iter().copied());
    let input = ArrayInput {
        cells: copied,
        nx,
        ny,
        transform: GeoTransform::axis_aligned(transform.0, transform.1, transform.2, transform.3),
        crs: crs_of(crs),
        nodata,
        times: frame_times,
        variable: variable.clone(),
        description,
        units: source_units,
        file_name: file_name.to_string(),
    };
    let o = options(
        envelope,
        envelope_crs,
        shapefile,
        target_crs,
        cell_size,
        method,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        "refuse",
        duplicates,
        threads,
        sync,
        allow_undetectable_readers,
    )?;
    let request = request_of(Vec::new(), output, Selection::Variable(variable), o);
    let callback = Callback {
        function: progress,
        error: Mutex::new(None),
    };
    let result =
        py.allow_threads(|| import::import_arrays(&input, &request, &mut |p| callback.call(p)));
    finish(py, output, result, &callback)
}

/// Variables a file offers. Reads only metadata.
#[pyfunction]
fn gridded_variables(py: Python<'_>, path: &str) -> PyResult<Vec<String>> {
    let p = PathBuf::from(path);
    let result = py.allow_threads(move || import::list_variables(&p));
    result.map_err(|e| match e {
        VortexError::Io(io) => BindErr::Os(io, path.to_string()).into_py(),
        VortexError::Decode { .. } => DssFormatError::new_err(ctx(path, e)),
        other => DssUnsupportedRecordError::new_err(ctx(path, other)),
    })
}

/// The importer's default bounds, so the wrapper can refuse before it copies an array.
#[pyfunction]
fn gridded_limits(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let limits = import::ImportLimits::default();
    let d = PyDict::new(py);
    d.set_item("max_source_cells", limits.max_source_cells)?;
    d.set_item("max_records", limits.max_records)?;
    // The NetCDF reader's defaults, which `import_gridded` can change.
    let netcdf = dss_vortex::source::netcdf::Limits::default();
    d.set_item(
        "netcdf_max_decoded_bytes_per_read",
        netcdf.max_decoded_bytes_per_read,
    )?;
    d.set_item("netcdf_chunk_cache_bytes", netcdf.chunk_cache_bytes)?;
    d.set_item("netcdf_max_seconds_per_read", netcdf.max_seconds_per_read)?;
    Ok(d)
}

/// Add this module's functions to `_native`.
pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<GriddedStop>()?;
    m.add_function(wrap_pyfunction!(gridded_limits, m)?)?;
    m.add_function(wrap_pyfunction!(import_gridded, m)?)?;
    m.add_function(wrap_pyfunction!(gridded_cell_size, m)?)?;
    m.add_function(wrap_pyfunction!(import_arrays, m)?)?;
    m.add_function(wrap_pyfunction!(gridded_variables, m)?)?;
    Ok(())
}

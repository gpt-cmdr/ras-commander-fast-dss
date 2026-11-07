//! Bindings for a local Zarr v2 store (`dss_vortex::source::zarr`, packet P49). The public API is
//! `python/ras_commander_fast_dss/gridded/_zarr.py`; this module moves data and maps errors.

use super::vortex::{finish, options, request_of, Callback};
use super::{ctx, BindErr, DssFormatError, DssUnsupportedRecordError};
use dss_vortex::import::Selection;
use dss_vortex::source::zarr::{self, Bbox, Limits, StoreSelection, Window, ZarrStore};
use dss_vortex::{Crs, VortexError};
use numpy::IntoPyArray;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A flag another thread sets to stop a store's read before its next chunk.
#[pyclass(frozen, module = "ras_commander_fast_dss._native", name = "ZarrStop")]
pub(super) struct ZarrStop {
    flag: Arc<AtomicBool>,
}

#[pymethods]
impl ZarrStop {
    #[new]
    fn new() -> Self {
        ZarrStop {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

fn stop_of(stop: Option<&Bound<'_, ZarrStop>>) -> Option<Arc<AtomicBool>> {
    stop.map(|s| s.get().flag.clone())
}

/// The reader's limits by name, with their defaults. `zarr_limits` returns these, and `limits=`
/// takes any of them.
fn limits_dict<'py>(py: Python<'py>, l: &Limits) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("max_cells", l.max_cells)?;
    d.set_item("max_read_bytes", l.max_read_bytes)?;
    d.set_item("max_metadata_bytes", l.max_metadata_bytes)?;
    d.set_item("max_variables", l.max_variables)?;
    d.set_item("max_axis_points", l.max_axis_points)?;
    d.set_item("max_chunk_bytes", l.max_chunk_bytes)?;
    d.set_item("max_chunks_per_read", l.max_chunks_per_read)?;
    d.set_item("max_decoded_bytes_per_read", l.max_decoded_bytes_per_read)?;
    d.set_item("chunk_cache_bytes", l.chunk_cache_bytes)?;
    d.set_item("max_seconds_per_read", l.max_seconds_per_read)?;
    d.set_item("max_cells_per_import", l.max_cells_per_import)?;
    d.set_item("refuse_absent_chunks", l.refuse_absent_chunks)?;
    Ok(d)
}

fn limits_of(given: Option<&Bound<'_, PyDict>>) -> PyResult<Option<Limits>> {
    let Some(given) = given else {
        return Ok(None);
    };
    let mut l = Limits::default();
    for (key, value) in given.iter() {
        let key: String = key.extract()?;
        let bad = |e: PyErr| PyValueError::new_err(format!("limit {key}: {e}"));
        match key.as_str() {
            "max_cells" => l.max_cells = value.extract().map_err(bad)?,
            "max_read_bytes" => l.max_read_bytes = value.extract().map_err(bad)?,
            "max_metadata_bytes" => l.max_metadata_bytes = value.extract().map_err(bad)?,
            "max_variables" => l.max_variables = value.extract().map_err(bad)?,
            "max_axis_points" => l.max_axis_points = value.extract().map_err(bad)?,
            "max_chunk_bytes" => l.max_chunk_bytes = value.extract().map_err(bad)?,
            "max_chunks_per_read" => l.max_chunks_per_read = value.extract().map_err(bad)?,
            "max_decoded_bytes_per_read" => {
                l.max_decoded_bytes_per_read = value.extract().map_err(bad)?
            }
            "chunk_cache_bytes" => l.chunk_cache_bytes = value.extract().map_err(bad)?,
            "max_seconds_per_read" => l.max_seconds_per_read = value.extract().map_err(bad)?,
            "max_cells_per_import" => l.max_cells_per_import = value.extract().map_err(bad)?,
            "refuse_absent_chunks" => l.refuse_absent_chunks = value.extract().map_err(bad)?,
            _ => {
                return Err(PyValueError::new_err(format!(
                    "limits has no {key:?}; zarr_limits() names them"
                )))
            }
        }
    }
    Ok(Some(l))
}

/// The store reader's default limits.
#[pyfunction]
fn zarr_limits(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    limits_dict(py, &Limits::default())
}

fn zarr_err(path: &str, e: VortexError) -> PyErr {
    match e {
        VortexError::Io(io) => BindErr::Os(io, path.to_string()).into_py(),
        VortexError::Decode { .. } => DssFormatError::new_err(ctx(path, e)),
        other => DssUnsupportedRecordError::new_err(ctx(path, other)),
    }
}

fn bbox_of(bbox: Option<(f64, f64, f64, f64)>) -> Option<Bbox> {
    bbox.map(|(west, south, east, north)| Bbox {
        west,
        south,
        east,
        north,
    })
}

/// A store's variables: one dict each, with why for those that are not grids. Metadata and axes
/// only; no cell is read.
#[pyfunction]
fn zarr_variables<'py>(py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyList>> {
    struct Row {
        name: String,
        error: Option<String>,
        steps: usize,
        ny: usize,
        nx: usize,
        units: String,
        description: Option<String>,
        crs: Option<String>,
        accumulation: Option<i64>,
        note: Option<String>,
        first: Option<i64>,
        last: Option<i64>,
    }
    let p = PathBuf::from(path);
    let rows = py
        .allow_threads(move || -> Result<Vec<Row>, VortexError> {
            let store = ZarrStore::open(&p)?;
            let mut rows = Vec::new();
            for name in store.variable_names() {
                rows.push(match store.variable(&name) {
                    Err(error) => Row {
                        name,
                        error: Some(error.to_string()),
                        steps: 0,
                        ny: 0,
                        nx: 0,
                        units: String::new(),
                        description: None,
                        crs: None,
                        accumulation: None,
                        note: None,
                        first: None,
                        last: None,
                    },
                    Ok(v) => {
                        let window = v.full_window();
                        let steps = v.step_count();
                        Row {
                            error: None,
                            steps,
                            ny: window.rows.end,
                            nx: window.cols.end,
                            units: v.units().to_string(),
                            description: v.description().map(str::to_string),
                            crs: match v.crs() {
                                Crs::Wkt(text) => Some(text.clone()),
                                _ => None,
                            },
                            accumulation: v.stated_accumulation(),
                            note: v.period_note().map(str::to_string),
                            first: v.instant(0)?,
                            last: v.instant(steps.saturating_sub(1))?,
                            name,
                        }
                    }
                });
            }
            Ok(rows)
        })
        .map_err(|e| zarr_err(path, e))?;
    let list = PyList::empty(py);
    for r in rows {
        let d = PyDict::new(py);
        d.set_item("name", r.name)?;
        d.set_item("error", r.error)?;
        d.set_item("steps", r.steps)?;
        d.set_item("ny", r.ny)?;
        d.set_item("nx", r.nx)?;
        d.set_item("units", r.units)?;
        d.set_item("description", r.description)?;
        d.set_item("crs", r.crs)?;
        d.set_item("accumulation_seconds", r.accumulation)?;
        d.set_item("period_note", r.note)?;
        d.set_item("first_time", r.first)?;
        d.set_item("last_time", r.last)?;
        list.append(d)?;
    }
    Ok(list)
}

/// Read a window of a variable over a run of steps. Times are seconds since 1970 UTC.
#[pyfunction]
#[pyo3(signature = (path, variable, first_step, last_step, bbox=None, rows=None, cols=None,
                    limits=None, stop=None))]
#[allow(clippy::too_many_arguments)]
fn read_zarr<'py>(
    py: Python<'py>,
    path: &str,
    variable: &str,
    first_step: usize,
    last_step: usize,
    bbox: Option<(f64, f64, f64, f64)>,
    rows: Option<(usize, usize)>,
    cols: Option<(usize, usize)>,
    limits: Option<&Bound<'py, PyDict>>,
    stop: Option<&Bound<'py, ZarrStop>>,
) -> PyResult<Bound<'py, PyDict>> {
    let limits = limits_of(limits)?.unwrap_or_default();
    let stop = stop_of(stop);
    if bbox.is_some() && (rows.is_some() || cols.is_some()) {
        return Err(PyValueError::new_err(
            "give bbox, or rows and cols, not both",
        ));
    }
    struct Out {
        cells: Vec<f32>,
        window: Window,
        nx: usize,
        ny: usize,
        transform: [f64; 6],
        crs: Option<String>,
        nodata: Option<f32>,
        instants: Vec<Option<i64>>,
        accumulation: Option<i64>,
        units: String,
        description: Option<String>,
    }
    let p = PathBuf::from(path);
    let name = variable.to_string();
    let out = py
        .allow_threads(move || -> Result<Out, VortexError> {
            let store = ZarrStore::open_with(&p, limits, stop)?;
            let v = store.variable(&name)?;
            let window = match (bbox_of(bbox), rows, cols) {
                (Some(b), _, _) => v.window_for_bbox(&b)?,
                (None, rows, cols) => {
                    let full = v.full_window();
                    Window {
                        rows: rows.map_or(full.rows, |(a, b)| a..b),
                        cols: cols.map_or(full.cols, |(a, b)| a..b),
                    }
                }
            };
            let (nx, ny, t) = v.grid(&window)?;
            let steps = first_step..last_step;
            let mut instants = Vec::new();
            for step in steps.clone() {
                instants.push(v.instant(step)?);
            }
            Ok(Out {
                cells: v.read(steps, &window)?,
                window,
                nx,
                ny,
                transform: [
                    t.origin_x,
                    t.dx,
                    t.row_rotation,
                    t.origin_y,
                    t.column_rotation,
                    t.dy,
                ],
                crs: match v.crs() {
                    Crs::Wkt(text) => Some(text.clone()),
                    _ => None,
                },
                nodata: v.nodata(),
                instants,
                accumulation: v.stated_accumulation(),
                units: v.units().to_string(),
                description: v.description().map(str::to_string),
            })
        })
        .map_err(|e| zarr_err(path, e))?;
    let d = PyDict::new(py);
    d.set_item("cells", out.cells.into_pyarray(py))?;
    d.set_item("nx", out.nx)?;
    d.set_item("ny", out.ny)?;
    d.set_item("rows", (out.window.rows.start, out.window.rows.end))?;
    d.set_item("cols", (out.window.cols.start, out.window.cols.end))?;
    d.set_item("transform", out.transform.to_vec())?;
    d.set_item("crs", out.crs)?;
    d.set_item("nodata", out.nodata)?;
    d.set_item("times", out.instants)?;
    d.set_item("accumulation_seconds", out.accumulation)?;
    d.set_item("units", out.units)?;
    d.set_item("description", out.description)?;
    Ok(d)
}

/// Import stores on a box and a time range; returns one dict per record written.
#[pyfunction]
#[pyo3(signature = (inputs, output, variable, bbox=None, times=None, envelope=None,
                    envelope_crs=None, shapefile=None, target_crs=None, cell_size=None,
                    method="bilinear", part_a="", part_b="", part_c=None, part_f="", units=None,
                    data_type=None, version=None, align="refuse", window_rule="refuse",
                    duplicates="refuse", threads=1, sync=true, allow_undetectable_readers=true,
                    progress=None, limits=None, stop=None))]
#[allow(clippy::too_many_arguments)]
fn import_zarr<'py>(
    py: Python<'py>,
    inputs: Vec<String>,
    output: &str,
    variable: String,
    bbox: Option<(f64, f64, f64, f64)>,
    times: Option<(i64, i64)>,
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
    limits: Option<&Bound<'py, PyDict>>,
    stop: Option<&Bound<'py, ZarrStop>>,
) -> PyResult<Bound<'py, PyList>> {
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
    let mut request = request_of(inputs, output, Selection::Variable(variable), o);
    request.zarr_limits = limits_of(limits)?;
    request.stop = stop_of(stop);
    let selection = StoreSelection {
        bbox: bbox_of(bbox),
        times,
    };
    let callback = Callback {
        function: progress,
        error: Mutex::new(None),
    };
    let result =
        py.allow_threads(|| zarr::import_stores(&request, &selection, &mut |p| callback.call(p)));
    finish(py, output, result, &callback)
}

pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ZarrStop>()?;
    m.add_function(wrap_pyfunction!(zarr_limits, m)?)?;
    m.add_function(wrap_pyfunction!(zarr_variables, m)?)?;
    m.add_function(wrap_pyfunction!(read_zarr, m)?)?;
    m.add_function(wrap_pyfunction!(import_zarr, m)?)?;
    Ok(())
}

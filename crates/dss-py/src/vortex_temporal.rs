//! Bindings for the gridded time tools (`dss_vortex::temporal`, packet P52). The public API is
//! `python/ras_commander_fast_dss/gridded/_temporal.py`; this module moves data and maps errors.
//!
//! Runs release the GIL, as other writes do here. A progress callback takes it for its call only.

use super::{ctx, write_err, BindErr, DssError, DssFormatError, DssUnsupportedRecordError};
use dss_vortex::import::Failure;
use dss_vortex::sink_dss::Parts;
use dss_vortex::temporal::{
    self, Rule, SeriesInfo, TemporalError, TemporalLimits, TemporalRecord, TemporalReport,
    TemporalRequest, Window, WindowRead,
};
use dss_vortex::{DataType, VortexError};

use pyo3::exceptions::{PyMemoryError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn data_type_of(text: Option<&str>) -> PyResult<Option<DataType>> {
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

fn type_name(t: DataType) -> &'static str {
    match t {
        DataType::PerAver => "PER-AVER",
        DataType::PerCum => "PER-CUM",
        DataType::InstVal => "INST-VAL",
        DataType::InstCum => "INST-CUM",
    }
}

fn rule_of(text: &str) -> PyResult<Rule> {
    match text {
        "strict" => Ok(Rule::Strict),
        "vortex" => Ok(Rule::Vortex),
        other => Err(PyValueError::new_err(format!(
            "rule must be strict or vortex, not {other:?}"
        ))),
    }
}

fn record_dict<'py>(py: Python<'py>, r: &TemporalRecord) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("source", &r.source)?;
    d.set_item("source_type", type_name(r.source_type))?;
    d.set_item("every_cell_missing", r.every_cell_missing)?;
    let w = &r.record;
    d.set_item("pathname", &w.pathname)?;
    d.set_item("record_type", w.record_type)?;
    d.set_item("outcome", &w.outcome)?;
    d.set_item("data_type", type_name(w.data_type))?;
    d.set_item("units", &w.units)?;
    d.set_item("nx", w.nx)?;
    d.set_item("ny", w.ny)?;
    d.set_item("lower_left_cell_x", w.lower_left_x)?;
    d.set_item("lower_left_cell_y", w.lower_left_y)?;
    d.set_item("cell_size", w.cell_size)?;
    d.set_item("readers_undetected", w.readers_undetected)?;
    Ok(d)
}

fn records_list<'py>(py: Python<'py>, records: &[TemporalRecord]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for r in records {
        list.append(record_dict(py, r)?)?;
    }
    Ok(list)
}

fn vortex_err(path: &str, e: VortexError) -> PyErr {
    match e {
        VortexError::Io(io) if io.kind() == std::io::ErrorKind::OutOfMemory => {
            PyMemoryError::new_err(io.to_string())
        }
        VortexError::Io(io) => BindErr::Os(io, path.to_string()).into_py(),
        e @ VortexError::Decode { .. } => DssFormatError::new_err(e.to_string()),
        e => DssUnsupportedRecordError::new_err(e.to_string()),
    }
}

fn temporal_err(py: Python<'_>, input: &str, output: &str, e: TemporalError) -> PyErr {
    let message = e.to_string();
    let completed = records_list(py, &e.completed).ok().map(|l| l.unbind());
    let (phase, source, in_doubt) = (e.phase, e.source.clone(), e.in_doubt.clone());
    let raised = match e.failure {
        Failure::Vortex(VortexError::Io(io)) => {
            if io.kind() == std::io::ErrorKind::OutOfMemory {
                PyMemoryError::new_err(message)
            } else {
                BindErr::Os(io, input.to_string()).into_py()
            }
        }
        Failure::Vortex(VortexError::Decode { .. }) => DssFormatError::new_err(message),
        Failure::Vortex(_) => DssUnsupportedRecordError::new_err(message),
        Failure::Write(w) => {
            let original = write_err(py, output, w);
            let v = original.value(py);
            let _ = v.setattr("args", (message,));
            original
        }
        Failure::Cancelled | Failure::Stopped => DssError::new_err(message),
        Failure::BadRequest(m) => PyValueError::new_err(ctx(output, m)),
    };
    let v = raised.value(py);
    let _ = v.setattr("phase", phase);
    let _ = v.setattr("source", source);
    let _ = v.setattr("completed", completed);
    let _ = v.setattr("in_doubt", in_doubt);
    raised
}

struct Callback {
    function: Option<PyObject>,
    error: Mutex<Option<PyErr>>,
}

impl Callback {
    /// `False` stops the run, as the importer's callback does. An exception stops it too, and is
    /// raised as itself.
    fn call(&self, done: usize, total: usize) -> ControlFlow<()> {
        let Some(function) = &self.function else {
            return ControlFlow::Continue(());
        };
        Python::with_gil(|py| match function.call1(py, (done, total)) {
            Ok(value) => match value.extract::<bool>(py) {
                Ok(false) => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            },
            Err(e) => {
                if let Ok(mut slot) = self.error.lock() {
                    *slot = Some(e);
                }
                ControlFlow::Break(())
            }
        })
    }

    fn take_error(&self) -> Option<PyErr> {
        self.error.lock().ok().and_then(|mut slot| slot.take())
    }
}

fn report_dict<'py>(py: Python<'py>, r: &TemporalReport) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("records", records_list(py, &r.records)?)?;
    d.set_item("skipped", r.skipped.clone())?;
    d.set_item("dropped_tail_seconds", r.dropped_tail_seconds)?;
    d.set_item("windows_every_cell_missing", r.windows_every_cell_missing)?;
    Ok(d)
}

fn finish<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    result: Result<TemporalReport, TemporalError>,
    callback: &Callback,
) -> PyResult<Bound<'py, PyDict>> {
    match result {
        Ok(report) => {
            // An exception in the last callback still counts.
            if let Some(raised) = callback.take_error() {
                let completed = records_list(py, &report.records)?.unbind();
                let _ = raised.value(py).setattr("completed", completed);
                let _ = raised.value(py).setattr("in_doubt", py.None());
                return Err(raised);
            }
            report_dict(py, &report)
        }
        Err(e) => {
            if let Some(raised) = callback.take_error() {
                let completed = records_list(py, &e.completed)?.unbind();
                let _ = raised.value(py).setattr("completed", completed);
                let _ = raised.value(py).setattr("in_doubt", e.in_doubt.clone());
                return Err(raised);
            }
            Err(temporal_err(py, input, output, e))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn request_of(
    input: &str,
    output: &str,
    select: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<TemporalRequest> {
    Ok(TemporalRequest {
        input: PathBuf::from(input),
        output: PathBuf::from(output),
        selection: select,
        parts: Parts {
            a: part_a.to_string(),
            b: part_b.to_string(),
            f: part_f.to_string(),
        },
        c_part: part_c.map(str::to_string),
        units: units.map(str::to_string),
        data_type: data_type_of(data_type)?,
        version,
        sync,
        allow_undetectable_readers,
        fault: None,
        limits: TemporalLimits::default(),
    })
}

/// Shift record times by whole seconds. See `gridded.shift_times`.
#[pyfunction]
#[pyo3(signature = (input, output, shift_start, shift_end, select=None, part_a="", part_b="",
                    part_c=None, part_f="", units=None, data_type=None, version=None, sync=true,
                    allow_undetectable_readers=true, progress=None))]
#[allow(clippy::too_many_arguments)]
fn shift_grid_times<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    shift_start: i64,
    shift_end: i64,
    select: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    sync: bool,
    allow_undetectable_readers: bool,
    progress: Option<PyObject>,
) -> PyResult<Bound<'py, PyDict>> {
    let request = request_of(
        input,
        output,
        select,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        sync,
        allow_undetectable_readers,
    )?;
    let callback = Callback {
        function: progress,
        error: Mutex::new(None),
    };
    let result = py.allow_threads(|| {
        temporal::shift_times(&request, shift_start, shift_end, &mut |d, t| {
            callback.call(d, t)
        })
    });
    finish(py, input, output, result, &callback)
}

/// Resample a series to a Vortex time step. See `gridded.resample_times`.
#[pyfunction]
#[pyo3(signature = (input, output, step_minutes, rule="strict", select=None, part_a="", part_b="",
                    part_c=None, part_f="", units=None, data_type=None, version=None, sync=true,
                    allow_undetectable_readers=true, progress=None))]
#[allow(clippy::too_many_arguments)]
fn resample_grid_times<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    step_minutes: i64,
    rule: &str,
    select: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    sync: bool,
    allow_undetectable_readers: bool,
    progress: Option<PyObject>,
) -> PyResult<Bound<'py, PyDict>> {
    let rule = rule_of(rule)?;
    let request = request_of(
        input,
        output,
        select,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        sync,
        allow_undetectable_readers,
    )?;
    let callback = Callback {
        function: progress,
        error: Mutex::new(None),
    };
    let result = py.allow_threads(|| {
        temporal::resample_times(&request, step_minutes, rule, &mut |d, t| {
            callback.call(d, t)
        })
    });
    finish(py, input, output, result, &callback)
}

fn stamp(text: &str) -> PyResult<i64> {
    temporal::parse_stamp(text)
        .ok_or_else(|| PyValueError::new_err(format!("{text:?} is not a time like 01JAN2024:2400")))
}

fn read_dict<'py>(py: Python<'py>, input: &str, r: &WindowRead) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    // `new_with` raises MemoryError where a `Vec` would abort.
    let bytes = PyBytes::new_with(py, r.cells.len() * 4, |buf| {
        for (out, cell) in buf.chunks_exact_mut(4).zip(&r.cells) {
            out.copy_from_slice(&cell.to_le_bytes());
        }
        Ok(())
    })?;
    d.set_item("cells", bytes)?;
    d.set_item("nx", r.nx)?;
    d.set_item("ny", r.ny)?;
    let w = Window {
        start: r.start,
        end: r.end,
    };
    let as_type = if r.start == r.end {
        DataType::InstVal
    } else {
        DataType::PerCum
    };
    let (start, end) = temporal::stamps_of(w, as_type).map_err(|e| vortex_err(input, e))?;
    d.set_item("start", start)?;
    d.set_item("end", end)?;
    d.set_item("data_type", type_name(r.data_type))?;
    d.set_item("units", &r.units)?;
    d.set_item("cell_size", r.cell_size)?;
    d.set_item("lower_left_cell_x", r.lower_left.0)?;
    d.set_item("lower_left_cell_y", r.lower_left.1)?;
    Ok(d)
}

/// One window of a series. See `gridded.read_window`.
#[pyfunction]
#[pyo3(signature = (input, pathname, start, end, rule="strict"))]
fn read_grid_window<'py>(
    py: Python<'py>,
    input: &str,
    pathname: &str,
    start: &str,
    end: &str,
    rule: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let (start, end, rule) = (stamp(start)?, stamp(end)?, rule_of(rule)?);
    let r = py
        .allow_threads(|| temporal::read_window(Path::new(input), pathname, start, end, rule))
        .map_err(|e| vortex_err(input, e))?;
    read_dict(py, input, &r)
}

/// The record nearest a time. See `gridded.read_nearest`.
#[pyfunction]
fn read_grid_nearest<'py>(
    py: Python<'py>,
    input: &str,
    pathname: &str,
    time: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let time = stamp(time)?;
    let r = py
        .allow_threads(|| temporal::read_nearest(Path::new(input), pathname, time))
        .map_err(|e| vortex_err(input, e))?;
    read_dict(py, input, &r)
}

/// The minimum and maximum grids over a window. See `gridded.read_min_max`.
#[pyfunction]
fn read_grid_min_max<'py>(
    py: Python<'py>,
    input: &str,
    pathname: &str,
    start: &str,
    end: &str,
) -> PyResult<(Bound<'py, PyDict>, Bound<'py, PyDict>)> {
    let (start, end) = (stamp(start)?, stamp(end)?);
    let (lo, hi) = py
        .allow_threads(|| temporal::read_min_max(Path::new(input), pathname, start, end))
        .map_err(|e| vortex_err(input, e))?;
    Ok((read_dict(py, input, &lo)?, read_dict(py, input, &hi)?))
}

fn info_dict<'py>(py: Python<'py>, input: &str, i: &SeriesInfo) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("series", &i.key)?;
    d.set_item("records", i.records)?;
    let text = |t: Option<i64>, as_end: bool| -> PyResult<Option<String>> {
        let Some(t) = t else { return Ok(None) };
        // At a window's start midnight says 0000, and at its end 2400 of the day before.
        let w = if as_end {
            Window {
                start: t - 60,
                end: t,
            }
        } else {
            Window {
                start: t,
                end: t + 60,
            }
        };
        let (a, b) = temporal::stamps_of(w, DataType::PerCum).map_err(|e| vortex_err(input, e))?;
        Ok(Some(if as_end { b } else { a }))
    };
    d.set_item("start", text(i.start, false)?)?;
    d.set_item("end", text(i.end, true)?)?;
    d.set_item("data_type", type_name(i.data_type))?;
    d.set_item("units", &i.units)?;
    d.set_item("nx", i.nx)?;
    d.set_item("ny", i.ny)?;
    d.set_item("cell_size", i.cell_size)?;
    d.set_item("lower_left_cell_x", i.lower_left.0)?;
    d.set_item("lower_left_cell_y", i.lower_left.1)?;
    d.set_item("srs_definition", &i.srs_definition)?;
    Ok(d)
}

/// What a series states. See `gridded.series_info`.
#[pyfunction]
fn grid_series_info<'py>(
    py: Python<'py>,
    input: &str,
    pathname: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let i = py
        .allow_threads(|| temporal::series_info(Path::new(input), pathname))
        .map_err(|e| vortex_err(input, e))?;
    info_dict(py, input, &i)
}

/// `math/TimeStep.java`, in minutes.
#[pyfunction]
fn grid_time_steps() -> Vec<i64> {
    temporal::TIME_STEP_MINUTES.to_vec()
}

pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(shift_grid_times, m)?)?;
    m.add_function(wrap_pyfunction!(resample_grid_times, m)?)?;
    m.add_function(wrap_pyfunction!(read_grid_window, m)?)?;
    m.add_function(wrap_pyfunction!(read_grid_nearest, m)?)?;
    m.add_function(wrap_pyfunction!(read_grid_min_max, m)?)?;
    m.add_function(wrap_pyfunction!(grid_series_info, m)?)?;
    m.add_function(wrap_pyfunction!(grid_time_steps, m)?)?;
    Ok(())
}

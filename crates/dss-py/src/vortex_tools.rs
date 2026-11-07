//! Bindings for the grid tools (`dss_vortex::grid_tools`, packet P53). The public API is
//! `python/ras_commander_fast_dss/gridded/_tools.py`; this module moves data and maps errors, with
//! the importer's error classes. Tools run with the GIL released.

// `ImportError` is the importer's own, built once on the way out of a run.
#![allow(clippy::result_large_err)]

use super::vortex::{crs_of, data_type_name, data_type_of, import_err, record_dict};
use dss_vortex::crs::Envelope;
use dss_vortex::grid_tools::{
    self, ClipBounds, ClipRequest, NormalizeRequest, Operation, OutputOptions, RasterRequest,
    Thresholds, ToolRecord,
};
use dss_vortex::import::ImportError;
use dss_vortex::warp::{Align, Method};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::path::{Path, PathBuf};

fn operation_of(text: &str) -> PyResult<Operation> {
    match text {
        "multiply" => Ok(Operation::Multiply),
        "divide" => Ok(Operation::Divide),
        "add" => Ok(Operation::Add),
        "subtract" => Ok(Operation::Subtract),
        other => Err(PyValueError::new_err(format!(
            "operation must be multiply, divide, add or subtract, not {other:?}"
        ))),
    }
}

fn method_of(text: &str) -> PyResult<Method> {
    match text {
        "near" => Ok(Method::Near),
        "bilinear" => Ok(Method::Bilinear),
        "average" => Ok(Method::Average),
        other => Err(PyValueError::new_err(format!(
            "method must be near, bilinear or average, not {other:?}"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn output_options(
    output: &str,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<OutputOptions> {
    let align = match align {
        "refuse" => Align::Refuse,
        "vortex" => Align::Vortex,
        other => {
            return Err(PyValueError::new_err(format!(
                "align must be \"refuse\" or \"vortex\", not {other:?}"
            )))
        }
    };
    Ok(OutputOptions {
        part_a: part_a.to_string(),
        part_b: part_b.to_string(),
        part_c: part_c.map(str::to_string),
        part_f: part_f.to_string(),
        units: units.map(str::to_string),
        data_type: data_type_of(data_type)?,
        version,
        align,
        sync,
        allow_undetectable_readers,
        ..OutputOptions::new(output)
    })
}

fn records_of<'py>(py: Python<'py>, records: &[ToolRecord]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for r in records {
        let d = record_dict(py, &r.imported)?;
        d.set_item("source_units", &r.source_units)?;
        d.set_item("source_data_type", r.source_data_type.map(data_type_name))?;
        d.set_item("converted", r.converted)?;
        list.append(d)?;
    }
    Ok(list)
}

fn finish<'py>(
    py: Python<'py>,
    output: &str,
    result: Result<Vec<ToolRecord>, ImportError>,
) -> PyResult<Bound<'py, PyList>> {
    match result {
        Ok(records) => records_of(py, &records),
        Err(e) => Err(import_err(py, output, e)),
    }
}

/// One constant operation; one dict per record written.
#[pyfunction]
#[pyo3(signature = (input, output, operation, value, variables=None, part_a="", part_b="",
                    part_c=None, part_f="", units=None, data_type=None, version=None,
                    align="refuse", sync=true, allow_undetectable_readers=true))]
#[allow(clippy::too_many_arguments)]
fn grid_calculate<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    operation: &str,
    value: f64,
    variables: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<Bound<'py, PyList>> {
    let operation = operation_of(operation)?;
    let o = output_options(
        output,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        sync,
        allow_undetectable_readers,
    )?;
    let result = py.allow_threads(|| {
        grid_tools::calculate(Path::new(input), variables.as_deref(), operation, value, &o)
    });
    finish(py, output, result)
}

/// One raster operation; one dict per record written.
#[pyfunction]
#[pyo3(signature = (input, raster, output, operation, method="bilinear", raster_variable=None,
                    variables=None, part_a="", part_b="", part_c=None, part_f="", units=None,
                    data_type=None, version=None, align="refuse", sync=true,
                    allow_undetectable_readers=true, static_raster=false))]
#[allow(clippy::too_many_arguments)]
fn grid_calculate_raster<'py>(
    py: Python<'py>,
    input: &str,
    raster: &str,
    output: &str,
    operation: &str,
    method: &str,
    raster_variable: Option<String>,
    variables: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    sync: bool,
    allow_undetectable_readers: bool,
    static_raster: bool,
) -> PyResult<Bound<'py, PyList>> {
    let request = RasterRequest {
        raster: PathBuf::from(raster),
        raster_variable,
        operation: operation_of(operation)?,
        method: method_of(method)?,
        static_raster,
    };
    let o = output_options(
        output,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        sync,
        allow_undetectable_readers,
    )?;
    let result = py.allow_threads(|| {
        grid_tools::calculate_raster(Path::new(input), variables.as_deref(), &request, &o)
    });
    finish(py, output, result)
}

/// Replace cells at or beyond thresholds; one dict per record written.
#[pyfunction]
#[pyo3(signature = (input, output, minimum=None, maximum=None, minimum_replacement=f32::NAN,
                    maximum_replacement=f32::NAN, variables=None, part_a="", part_b="", part_c=None,
                    part_f="", units=None, data_type=None, version=None, align="refuse", sync=true,
                    allow_undetectable_readers=true))]
#[allow(clippy::too_many_arguments)]
fn grid_sanitize<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    minimum: Option<f64>,
    maximum: Option<f64>,
    minimum_replacement: f32,
    maximum_replacement: f32,
    variables: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<Bound<'py, PyList>> {
    let defaults = Thresholds::default();
    let thresholds = Thresholds {
        minimum: minimum.unwrap_or(defaults.minimum),
        maximum: maximum.unwrap_or(defaults.maximum),
        minimum_replacement,
        maximum_replacement,
    };
    let o = output_options(
        output,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        sync,
        allow_undetectable_readers,
    )?;
    let result = py.allow_threads(|| {
        grid_tools::sanitize(Path::new(input), variables.as_deref(), thresholds, &o)
    });
    finish(py, output, result)
}

/// Clip to an envelope or a shapefile's; one dict per record written.
#[pyfunction]
#[pyo3(signature = (input, output, envelope=None, envelope_crs=None, shapefile=None, variables=None,
                    part_a="", part_b="", part_c=None, part_f="", units=None, data_type=None,
                    version=None, align="refuse", sync=true, allow_undetectable_readers=true))]
#[allow(clippy::too_many_arguments)]
fn grid_clip<'py>(
    py: Python<'py>,
    input: &str,
    output: &str,
    envelope: Option<(f64, f64, f64, f64)>,
    envelope_crs: Option<&str>,
    shapefile: Option<&str>,
    variables: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<Bound<'py, PyList>> {
    let bounds = match (envelope, envelope_crs, shapefile) {
        (None, _, Some(path)) => ClipBounds::Shapefile(PathBuf::from(path)),
        (Some((min_x, min_y, max_x, max_y)), Some(crs), None) => ClipBounds::Envelope {
            envelope: Envelope {
                min_x,
                max_x,
                min_y,
                max_y,
            },
            crs: crs_of(crs),
        },
        (Some(_), None, None) => {
            return Err(PyValueError::new_err(
                "an envelope needs envelope_crs: a CRS is never assumed",
            ))
        }
        _ => {
            return Err(PyValueError::new_err(
                "give an envelope with envelope_crs, or a shapefile",
            ))
        }
    };
    let request = ClipRequest { bounds };
    let o = output_options(
        output,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        sync,
        allow_undetectable_readers,
    )?;
    let result =
        py.allow_threads(|| grid_tools::clip(Path::new(input), variables.as_deref(), &request, &o));
    finish(py, output, result)
}

// HEC seconds to the crate's (julian day, seconds past midnight).
fn day_and_seconds(hec_seconds: i64) -> (i64, i64) {
    (
        hec_seconds.div_euclid(86_400),
        hec_seconds.rem_euclid(86_400),
    )
}

/// Normalize; a dict of records, intervals and what the windows left out.
#[pyfunction]
#[pyo3(signature = (input, normals, output, start, end, interval_seconds, source_variables=None,
                    normals_variables=None, part_a="", part_b="", part_c=None, part_f="", units=None,
                    data_type=None, version=None, align="refuse", sync=true,
                    allow_undetectable_readers=true))]
#[allow(clippy::too_many_arguments)]
fn grid_normalize<'py>(
    py: Python<'py>,
    input: &str,
    normals: &str,
    output: &str,
    start: i64,
    end: i64,
    interval_seconds: i64,
    source_variables: Option<Vec<String>>,
    normals_variables: Option<Vec<String>>,
    part_a: &str,
    part_b: &str,
    part_c: Option<&str>,
    part_f: &str,
    units: Option<&str>,
    data_type: Option<&str>,
    version: Option<u8>,
    align: &str,
    sync: bool,
    allow_undetectable_readers: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let request = NormalizeRequest {
        normals: PathBuf::from(normals),
        source_variables,
        normals_variables,
        start: day_and_seconds(start),
        end: day_and_seconds(end),
        interval_seconds,
    };
    let o = output_options(
        output,
        part_a,
        part_b,
        part_c,
        part_f,
        units,
        data_type,
        version,
        align,
        sync,
        allow_undetectable_readers,
    )?;
    let outcome = py
        .allow_threads(|| grid_tools::normalize(Path::new(input), &request, &o))
        .map_err(|e| import_err(py, output, e))?;
    let d = PyDict::new(py);
    d.set_item("records", records_of(py, &outcome.records)?)?;
    let intervals = PyList::empty(py);
    for note in &outcome.intervals {
        let row = PyDict::new(py);
        // Back to HEC seconds, whose day 0 is the crate's julian day 0.
        row.set_item("start", note.start as i64)?;
        row.set_item("end", note.end as i64)?;
        row.set_item("sources", note.sources)?;
        row.set_item("normals", note.normals)?;
        intervals.append(row)?;
    }
    d.set_item("intervals", intervals)?;
    d.set_item("sources_left_out", &outcome.sources_left_out)?;
    d.set_item("normals_left_out", &outcome.normals_left_out)?;
    d.set_item("long_hash_bin", outcome.long_hash_bin)?;
    d.set_item(
        "cells_made_missing_by_normal",
        outcome.cells_made_missing_by_normal,
    )?;
    Ok(d)
}

/// Add this module's functions to `_native`.
pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(grid_calculate, m)?)?;
    m.add_function(wrap_pyfunction!(grid_calculate_raster, m)?)?;
    m.add_function(wrap_pyfunction!(grid_sanitize, m)?)?;
    m.add_function(wrap_pyfunction!(grid_clip, m)?)?;
    m.add_function(wrap_pyfunction!(grid_normalize, m)?)?;
    Ok(())
}

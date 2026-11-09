//! Bindings for grid to point (`dss_vortex::zonal`). The public API is
//! `python/ras_commander_fast_dss/gridded/_zonal.py`; this module moves data and maps errors. Work
//! runs with the GIL released.

use super::{ctx, write_err, BindErr, DssFormatError, DssUnsupportedRecordError};
use dss_vortex::crs::DatumPolicy;
use dss_vortex::zonal::{
    self, Compat, GridToPointError, GridToPointFailure, GridToPointRequest, PointParts,
    PointRecord, Statistic, ZonalRequest, ZoneLimits, ZoneMask,
};
use dss_vortex::{Crs, NamedCrs, VortexError};
use numpy::{IntoPyArray, PyArrayMethods};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::path::PathBuf;

fn crs_of(text: &str) -> Crs {
    match text.to_ascii_uppercase().as_str() {
        "SHG" => Crs::Named(NamedCrs::Shg),
        "HRAP" => Crs::Named(NamedCrs::Hrap),
        _ => Crs::Wkt(text.to_string()),
    }
}

fn compat_of(text: &str) -> PyResult<Compat> {
    match text {
        "engine" => Ok(Compat::Engine),
        "vortex" => Ok(Compat::Vortex),
        other => Err(PyValueError::new_err(format!(
            "compatibility must be \"engine\" or \"vortex\", not {other:?}"
        ))),
    }
}

fn statistic_of(text: &str) -> PyResult<Statistic> {
    Ok(match text {
        "average" => Statistic::Average,
        "min" => Statistic::Min,
        "max" => Statistic::Max,
        "median" => Statistic::Median,
        "q1" => Statistic::FirstQuartile,
        "q3" => Statistic::ThirdQuartile,
        "pct_gt_0" => Statistic::PctGtZero,
        "pct_gt_q1" => Statistic::PctGtFirstQuartile,
        "area_weighted_mean" => Statistic::AreaWeightedMean,
        other => {
            return Err(PyValueError::new_err(format!(
                "statistic {other:?} is not one of average, min, max, median, q1, q3, pct_gt_0, \
                 pct_gt_q1, area_weighted_mean"
            )))
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn zonal_request(
    grids: &str,
    zones: &str,
    field: &str,
    variables: Option<Vec<String>>,
    zones_crs: Option<&str>,
    compatibility: &str,
    assume_no_datum_shift: bool,
    area_weighted: bool,
) -> PyResult<ZonalRequest> {
    Ok(ZonalRequest {
        grids: PathBuf::from(grids),
        variables,
        zones: PathBuf::from(zones),
        field: field.to_string(),
        zones_crs: zones_crs.map(crs_of),
        datum_policy: if assume_no_datum_shift {
            DatumPolicy::AssumeNoShift
        } else {
            DatumPolicy::Refuse
        },
        compat: compat_of(compatibility)?,
        limits: ZoneLimits::default(),
        area_weighted,
    })
}

fn vortex_err(path: &str, e: VortexError) -> PyErr {
    let message = ctx(path, &e);
    match e {
        VortexError::Io(io) => BindErr::Os(io, path.to_string()).into_py(),
        VortexError::Decode { .. } => DssFormatError::new_err(message),
        _ => DssUnsupportedRecordError::new_err(message),
    }
}

fn record_dict<'py>(py: Python<'py>, r: &PointRecord) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("zone", &r.zone)?;
    d.set_item("variable", &r.description)?;
    d.set_item("statistic", &r.statistic)?;
    d.set_item("pathname", &r.pathname)?;
    d.set_item("regular", r.regular)?;
    d.set_item("values", r.values)?;
    d.set_item("units", &r.units)?;
    d.set_item("data_type", &r.data_type)?;
    d.set_item("outcome", &r.outcome)?;
    Ok(d)
}

fn records_list<'py>(py: Python<'py>, records: &[PointRecord]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for r in records {
        list.append(record_dict(py, r)?)?;
    }
    Ok(list)
}

fn zones_list<'py>(py: Python<'py>, zones: &[ZoneMask]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for z in zones {
        let d = PyDict::new(py);
        d.set_item("zone", &z.id)?;
        d.set_item("cells", z.cells.clone().into_pyarray(py))?;
        d.set_item("centroid_fallback", z.centroid_fallback)?;
        list.append(d)?;
    }
    Ok(list)
}

fn run_err(py: Python<'_>, grids: &str, output: &str, e: GridToPointError) -> PyErr {
    let message = e.to_string();
    let completed = records_list(py, &e.completed).ok().map(|l| l.unbind());
    let (phase, pathname, in_doubt) = (e.phase, e.pathname.clone(), e.in_doubt.clone());
    let raised = match e.failure {
        GridToPointFailure::Vortex(v) => vortex_err(grids, v),
        GridToPointFailure::Write(w) => {
            let original = write_err(py, output, w);
            let _ = original.value(py).setattr("args", (message,));
            original
        }
        GridToPointFailure::BadRequest(m) => PyValueError::new_err(ctx(output, m)),
    };
    let v = raised.value(py);
    let _ = v.setattr("phase", phase);
    let _ = v.setattr("pathname", pathname);
    let _ = v.setattr("completed", completed);
    let _ = v.setattr("in_doubt", in_doubt);
    raised
}

/// Write statistics as time series. See `gridded.grid_to_point`.
#[pyfunction]
#[pyo3(signature = (grids, zones, field, output, statistics, accumulate=false, part_a=None, part_f=None,
                    variables=None, zones_crs=None, compatibility="engine", version=None,
                    assume_no_datum_shift=false, allow_undetectable_readers=true, allow_in_place=false))]
#[allow(clippy::too_many_arguments)]
fn gridded_grid_to_point<'py>(
    py: Python<'py>,
    grids: &str,
    zones: &str,
    field: &str,
    output: &str,
    statistics: Vec<String>,
    accumulate: bool,
    part_a: Option<String>,
    part_f: Option<String>,
    variables: Option<Vec<String>>,
    zones_crs: Option<&str>,
    compatibility: &str,
    version: Option<u8>,
    assume_no_datum_shift: bool,
    allow_undetectable_readers: bool,
    allow_in_place: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let statistics = statistics
        .iter()
        .map(|s| statistic_of(s))
        .collect::<PyResult<Vec<_>>>()?;
    let area_weighted = statistics.contains(&Statistic::AreaWeightedMean);
    let request = GridToPointRequest {
        zonal: zonal_request(
            grids,
            zones,
            field,
            variables,
            zones_crs,
            compatibility,
            assume_no_datum_shift,
            area_weighted,
        )?,
        output: PathBuf::from(output),
        statistics,
        accumulate,
        parts: PointParts {
            a: part_a,
            f: part_f,
        },
        version,
        allow_undetectable_readers,
        allow_in_place,
        fault: None,
    };
    let outcome = py
        .allow_threads(|| zonal::grid_to_point(&request))
        .map_err(|e| run_err(py, grids, output, e))?;
    let d = PyDict::new(py);
    d.set_item("records", records_list(py, &outcome.records)?)?;
    d.set_item("zones", zones_list(py, &outcome.zones)?)?;
    d.set_item("frames", outcome.frames)?;
    d.set_item("readers_undetected", outcome.readers_undetected)?;
    d.set_item("reserved_values", outcome.reserved_values)?;
    Ok(d)
}

/// Compute without writing. `stats` has shape (frames, zones, 8), in doubles.
#[pyfunction]
#[pyo3(signature = (grids, zones, field, variables=None, zones_crs=None, compatibility="engine",
                    area_weighted=false, assume_no_datum_shift=false))]
#[allow(clippy::too_many_arguments)]
fn gridded_zonal_table<'py>(
    py: Python<'py>,
    grids: &str,
    zones: &str,
    field: &str,
    variables: Option<Vec<String>>,
    zones_crs: Option<&str>,
    compatibility: &str,
    area_weighted: bool,
    assume_no_datum_shift: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let request = zonal_request(
        grids,
        zones,
        field,
        variables,
        zones_crs,
        compatibility,
        assume_no_datum_shift,
        area_weighted,
    )?;
    let table = py
        .allow_threads(|| zonal::compute(&request))
        .map_err(|e| vortex_err(grids, e))?;
    let (n_frames, n_zones) = (table.frames.len(), table.zones.len());
    let mut flat = Vec::new();
    flat.try_reserve_exact(table.stats.len() * 8)
        .map_err(|_| pyo3::exceptions::PyMemoryError::new_err("statistics"))?;
    for s in &table.stats {
        flat.extend_from_slice(&[
            s.average,
            s.min,
            s.max,
            s.median,
            s.first_quartile,
            s.third_quartile,
            s.pct_gt_zero,
            s.pct_gt_first_quartile,
        ]);
    }
    let d = PyDict::new(py);
    d.set_item("zones", zones_list(py, &table.zones)?)?;
    let frames = PyList::empty(py);
    for f in &table.frames {
        let row = PyDict::new(py);
        row.set_item("variable", &f.description)?;
        row.set_item("units", &f.units)?;
        row.set_item("start", f.start)?;
        row.set_item("end", f.end)?;
        frames.append(row)?;
    }
    d.set_item("frames", frames)?;
    d.set_item(
        "stats",
        flat.into_pyarray(py).reshape([n_frames, n_zones, 8])?,
    )?;
    match table.area_weighted {
        Some(a) => d.set_item(
            "area_weighted",
            a.into_pyarray(py).reshape([n_frames, n_zones])?,
        )?,
        None => d.set_item("area_weighted", py.None())?,
    }
    let placements = PyList::empty(py);
    for g in &table.placements {
        let row = PyDict::new(py);
        row.set_item("nx", g.nx)?;
        row.set_item("ny", g.ny)?;
        row.set_item("transform", g.transform.to_vec())?;
        placements.append(row)?;
    }
    d.set_item("placements", placements)?;
    d.set_item("missing", zonal::MISSING)?;
    Ok(d)
}

pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(gridded_grid_to_point, m)?)?;
    m.add_function(wrap_pyfunction!(gridded_zonal_table, m)?)?;
    Ok(())
}

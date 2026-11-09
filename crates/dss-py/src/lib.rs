//! Low-level Python bindings (`ras_commander_fast_dss._native`). The public Python API lives in
//! `python/ras_commander_fast_dss/`; this module only moves data between Rust and Python:
//! NumPy arrays plus plain dicts (SPEC §5). Pandas assembly stays in Python.
//!
//! Reads run with the GIL released. Errors are raised as the exception classes defined
//! in `ras_commander_fast_dss.exceptions`; OS errors become the matching builtin
//! `OSError` subclass.

use dss_core::dss6::{
    self, DatasetIndex, DatasetOptions, Dss6File, FileSource, TimeWindow, TsLayout,
};
use dss_core::dss7::{
    self, CatalogOptions, Dss7Catalog, Dss7File, RecordRef, TsReadOptions, TsWindow,
};
use dss_core::{
    AnomalyKind, ByteOrder, CatalogAnomaly, DssVersion, FileSignature, Grid, GridError,
    GridReadOptions, GridType, IrregularBlock, PairedData, RecordStatus, TimeSeries, TsKind,
    TsValues,
};
use numpy::{IntoPyArray, PyArrayMethods};
use pyo3::exceptions::{PyIndexError, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::fs::File;
use std::io::{self, Read};
use std::sync::Mutex;

pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssError);
pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssFormatError);
pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssUnsupportedRecordError);
pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssPathnameNotFoundError);
// P29: HEC DSS6 "no data" (a DssPathnameNotFoundError subclass).
pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssNoDataError);

// P46: gridded import (`dss-vortex`), behind the `vortex` feature.
#[cfg(feature = "vortex")]
mod vortex;
#[cfg(feature = "vortex")]
mod vortex_temporal;
// P51: grid to point (`dss_vortex::zonal`).
#[cfg(feature = "vortex")]
mod vortex_zonal;
// P53: grid tools (`dss_vortex::grid_tools`).
#[cfg(feature = "vortex")]
mod vortex_tools;
// P49: a local Zarr v2 store (`dss_vortex::source::zarr`).
#[cfg(feature = "vortex")]
mod vortex_zarr;

// ---------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------

/// Error produced while the GIL is released; converted to a Python exception after.
#[derive(Debug)]
enum BindErr {
    /// Operating-system error opening or reading the file.
    Os(io::Error, String),
    /// Damaged or non-DSS file.
    Format(String),
    /// Pathname not in the file (or no data for it).
    NotFound(String),
    /// Record type this reader does not decode as a time series.
    Unsupported(String),
    /// Other DSS failure (limits, password protection).
    Dss(String),
    /// Caller passed an invalid argument.
    Value(String),
    /// Catalog index out of range or not a live entry.
    Index(String),
    /// P29: HEC DSS6 "no data" refusal: message, pathname, HEC status (4).
    NoData(String, String, i32),
}

impl BindErr {
    fn into_py(self) -> PyErr {
        match self {
            BindErr::Os(e, path) => {
                // OSError selects the builtin subclass from errno. On Windows its
                // fourth argument is a Win32 code, which Python translates to errno;
                // raw_os_error must never be mistaken for a POSIX errno there.
                let errno = match e.kind() {
                    io::ErrorKind::NotFound => Some(2),
                    io::ErrorKind::PermissionDenied => Some(13),
                    io::ErrorKind::InvalidInput => Some(22),
                    _ => None,
                };
                #[cfg(windows)]
                let args = (errno, e.to_string(), path, e.raw_os_error());
                #[cfg(not(windows))]
                let args = (e.raw_os_error().or(errno), e.to_string(), path);
                PyOSError::new_err(args)
            }
            BindErr::Format(m) => DssFormatError::new_err(m),
            BindErr::NotFound(m) => DssPathnameNotFoundError::new_err(m),
            BindErr::Unsupported(m) => DssUnsupportedRecordError::new_err(m),
            BindErr::Dss(m) => DssError::new_err(m),
            BindErr::Value(m) => PyValueError::new_err(m),
            BindErr::Index(m) => PyIndexError::new_err(m),
            BindErr::NoData(m, pathname, status) => DssNoDataError::new_err((m, pathname, status)),
        }
    }
}

type BResult<T> = Result<T, BindErr>;

fn ctx(path: &str, m: impl std::fmt::Display) -> String {
    format!("{path}: {m}")
}

fn map_dss7(path: &str, e: dss7::Dss7Error) -> BindErr {
    use dss7::Dss7Error as E;
    match e {
        E::Io(io) => BindErr::Os(io, path.to_string()),
        E::BadPathname => BindErr::Value(ctx(path, &e)),
        E::NotLiveEntry { .. } => BindErr::Index(ctx(path, &e)),
        other => BindErr::Format(ctx(path, other)),
    }
}

fn map_ts7(path: &str, e: dss7::TsError) -> BindErr {
    use dss7::TsError as E;
    match e {
        E::Dss7(inner) => map_dss7(path, inner),
        E::NotFound { .. } | E::BadPathname { .. } => BindErr::NotFound(ctx(path, &e)),
        E::UnsupportedRecordType { .. } => BindErr::Unsupported(ctx(path, &e)),
        E::BadWindow => BindErr::Value(ctx(path, &e)),
        E::Corrupt { .. } => BindErr::Format(ctx(path, &e)),
        E::TooManyValues { .. } => BindErr::Dss(ctx(path, &e)),
    }
}

fn map_dss6(path: &str, e: dss6::Dss6Error) -> BindErr {
    match e {
        dss6::Dss6Error::Io(io) => BindErr::Os(io, path.to_string()),
        other => BindErr::Format(ctx(path, other)),
    }
}

fn map_ts6(path: &str, e: dss6::TsError) -> BindErr {
    use dss6::TsError as E;
    match e {
        E::Dss6(inner) => map_dss6(path, inner),
        E::NotFound { .. } | E::BadPathname { .. } => BindErr::NotFound(ctx(path, &e)),
        E::UnsupportedRecordType { .. } | E::UnsupportedCompression { .. } => {
            BindErr::Unsupported(ctx(path, &e))
        }
        E::PasswordProtected | E::TooLarge { .. } => BindErr::Dss(ctx(path, &e)),
        E::Corrupt { .. } => BindErr::Format(ctx(path, &e)),
        E::NoData {
            ref pathname,
            status,
        } => BindErr::NoData(ctx(path, &e), lossy(pathname), status),
    }
}

/// A malformed pathname is "no data" for the record readers, as for time series.
fn map_lookup7(path: &str, e: dss7::Dss7Error) -> BindErr {
    match e {
        dss7::Dss7Error::BadPathname => BindErr::NotFound(ctx(path, &e)),
        other => map_dss7(path, other),
    }
}

fn map_pd7(path: &str, e: dss7::PdError) -> BindErr {
    use dss7::PdError as E;
    match e {
        E::Dss7(inner) => map_lookup7(path, inner),
        E::NotFound { .. } => BindErr::NotFound(ctx(path, &e)),
        E::UnsupportedRecordType { .. } => BindErr::Unsupported(ctx(path, &e)),
        E::Corrupt { .. } => BindErr::Format(ctx(path, &e)),
    }
}

fn map_pd6(path: &str, pathname: &str, e: dss6::PdError) -> BindErr {
    use dss6::PdError as E;
    match e {
        E::Dss6(inner) => map_dss6(path, inner),
        E::NotFound { .. } => BindErr::NotFound(ctx(path, &e)),
        // The DSS6 message has no pathname; add it.
        E::UnsupportedRecordType { .. } => {
            BindErr::Unsupported(ctx(path, format!("{pathname}: {e}")))
        }
        E::PasswordProtected => BindErr::Dss(ctx(path, format!("{pathname}: {e}"))),
        E::Corrupt { .. } => BindErr::Format(ctx(path, format!("{pathname}: {e}"))),
    }
}

fn map_grid(path: &str, pathname: &str, e: GridError) -> BindErr {
    use GridError as E;
    let m = |e: &GridError| ctx(path, format!("{pathname}: {e}"));
    match e {
        E::Dss6(inner) => map_dss6(path, inner),
        E::Dss7(inner) => map_lookup7(path, inner),
        E::NotFound => BindErr::NotFound(ctx(path, format!("grid record not found: {pathname}"))),
        E::UnsupportedRecordType { .. }
        | E::UnsupportedStructVersion { .. }
        | E::UnsupportedCompression { .. }
        | E::UnsupportedStorageType { .. } => BindErr::Unsupported(m(&e)),
        E::PasswordProtected | E::LimitExceeded { .. } => BindErr::Dss(m(&e)),
        E::Corrupt { .. } => BindErr::Format(m(&e)),
    }
}

// ---------------------------------------------------------------------------------
// Signature
// ---------------------------------------------------------------------------------

fn read_signature(path: &str) -> BResult<FileSignature> {
    let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
    let mut file = File::open(path).map_err(|e| BindErr::Os(e, path.to_string()))?;
    let mut n = 0;
    // Short reads are legal; fill as much of the header as the file has.
    loop {
        let k = file
            .read(&mut head[n..])
            .map_err(|e| BindErr::Os(e, path.to_string()))?;
        if k == 0 {
            break;
        }
        n += k;
        if n == head.len() {
            break;
        }
    }
    head.truncate(n);
    FileSignature::from_bytes(&head).map_err(|e| BindErr::Format(ctx(path, e)))
}

fn signature_dict<'py>(py: Python<'py>, sig: &FileSignature) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item(
        "version",
        match sig.version {
            DssVersion::V6 => 6,
            DssVersion::V7 => 7,
        },
    )?;
    d.set_item("version_string", &sig.version_string)?;
    d.set_item(
        "byte_order",
        match sig.byte_order {
            ByteOrder::Little => "little",
            ByteOrder::Big => "big",
        },
    )?;
    d.set_item("number_records", sig.number_records)?;
    d.set_item("file_size_bytes", sig.file_size_bytes())?;
    d.set_item("dead_words", sig.dead_words)?;
    d.set_item("number_deletes", sig.number_deletes)?;
    Ok(d)
}

/// Read the header of a DSS file and return its summary as a dict.
#[pyfunction]
fn file_signature<'py>(py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyDict>> {
    let p = path.to_string();
    let sig = py
        .allow_threads(move || read_signature(&p))
        .map_err(BindErr::into_py)?;
    signature_dict(py, &sig)
}

// ---------------------------------------------------------------------------------
// Plain-data results built without the GIL
// ---------------------------------------------------------------------------------

/// Pathname bytes for display: UTF-8 when valid, otherwise Latin-1 (DSS6 files can hold
/// Latin-1 pathnames).
fn lossy(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

/// Candidate byte encodings of a pathname given as text: UTF-8 first, then Latin-1 when
/// it differs (round trip of [`lossy`]).
fn encodings(path: &str) -> Vec<Vec<u8>> {
    let mut v = vec![path.as_bytes().to_vec()];
    if path.chars().all(|c| (c as u32) < 256) {
        let l1: Vec<u8> = path.chars().map(|c| c as u8).collect();
        if l1 != v[0] {
            v.push(l1);
        }
    }
    v
}

fn status_name(s: RecordStatus) -> &'static str {
    match s {
        RecordStatus::Live => "live",
        RecordStatus::Alias => "alias",
        RecordStatus::Deleted => "deleted",
        RecordStatus::Renamed => "renamed",
        RecordStatus::RewrittenOld => "rewritten_old",
        RecordStatus::Other => "other",
    }
}

fn anomaly_name(k: AnomalyKind) -> &'static str {
    match k {
        AnomalyKind::HashMismatch => "hash_mismatch",
        AnomalyKind::OrphanBin => "orphan_bin",
        AnomalyKind::BadInfoBlock => "bad_info_block",
        AnomalyKind::DuplicatePathname => "duplicate_pathname",
        AnomalyKind::RecordCountMismatch => "record_count_mismatch",
        AnomalyKind::OutOfBounds => "out_of_bounds",
        AnomalyKind::CycleDetected => "cycle_detected",
        AnomalyKind::Other => "other",
    }
}

/// P62: which catalog rows are exported to Python. The native catalog (cached for
/// DSS7) always holds every physical entry; only the export is narrowed, so anomalies,
/// duplicate/recovered flags and forensic catalog indices are unchanged.
#[derive(Clone, Copy)]
struct CatalogSelect {
    /// Export only live and alias entries (`DssFile.catalog()`'s default rows), without
    /// the `stored_pathname` and `raw_status` columns that only `all_entries=True` shows.
    live_only: bool,
}

impl CatalogSelect {
    fn keep(self, s: RecordStatus) -> bool {
        !self.live_only || matches!(s, RecordStatus::Live | RecordStatus::Alias)
    }
}

struct CatalogData {
    /// P62: physical entries in the native catalog, before the export selection.
    entries: usize,
    /// P62: `stored_pathname` and `raw_status` are exported (every row selected).
    full: bool,
    pathname: Vec<String>,
    stored_pathname: Vec<String>,
    record_type: Vec<i32>,
    status: Vec<&'static str>,
    raw_status: Vec<i64>,
    last_write_ms: Vec<i64>,
    duplicate: Vec<bool>,
    recovered: Vec<bool>,
    anomalies: Vec<CatalogAnomaly>,
}

impl CatalogData {
    fn with_capacity(n: usize) -> Self {
        Self {
            entries: n,
            full: true,
            pathname: Vec::with_capacity(n),
            stored_pathname: Vec::with_capacity(n),
            record_type: Vec::with_capacity(n),
            status: Vec::with_capacity(n),
            raw_status: Vec::with_capacity(n),
            last_write_ms: Vec::with_capacity(n),
            duplicate: Vec::with_capacity(n),
            recovered: Vec::with_capacity(n),
            anomalies: Vec::new(),
        }
    }

    fn from_dss7(cat: &Dss7Catalog, sel: CatalogSelect) -> Self {
        let mut d = Self::with_capacity(cat.catalog.entries.len());
        d.full = !sel.live_only;
        for (e, m) in cat.catalog.entries.iter().zip(cat.meta.iter()) {
            // P62: unselected rows are skipped before their pathname is decoded.
            if !sel.keep(e.status) {
                continue;
            }
            let p = lossy(&e.pathname);
            if d.full {
                d.stored_pathname.push(p.clone());
            }
            d.pathname.push(p);
            d.record_type.push(e.record_type);
            d.status.push(status_name(e.status));
            if d.full {
                d.raw_status.push(i64::from(e.raw_status));
            }
            d.last_write_ms.push(e.last_write_ms.unwrap_or(-1));
            d.duplicate.push(m.duplicate_of.is_some());
            d.recovered.push(m.recovered);
        }
        d.anomalies = cat.catalog.anomalies.clone();
        d
    }

    fn from_dss6(walk: &dss6::BinWalk, sel: CatalogSelect) -> Self {
        let mut d = Self::with_capacity(walk.entries.len());
        d.full = !sel.live_only;
        let mut seen = std::collections::HashSet::new();
        for e in &walk.entries {
            let st = e.record_status();
            // The duplicate rule only counts live entries, which every selection keeps,
            // so evaluating it on selected rows gives the full catalog's flags.
            if !sel.keep(st) {
                continue;
            }
            d.pathname.push(lossy(&e.display_pathname()));
            d.record_type.push(e.record_type);
            d.status.push(status_name(st));
            if d.full {
                d.raw_status.push(i64::from(e.status));
            }
            d.last_write_ms.push(-1);
            d.duplicate
                .push(st == RecordStatus::Live && !seen.insert(e.pathname.clone()));
            d.recovered.push(false);
            if d.full {
                d.stored_pathname.push(lossy(&e.pathname));
            }
        }
        d.anomalies = walk.anomalies.clone();
        d
    }

    fn into_py<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("entries", self.entries)?;
        d.set_item("pathname", PyList::new(py, self.pathname)?)?;
        if self.full {
            d.set_item("stored_pathname", PyList::new(py, self.stored_pathname)?)?;
        }
        d.set_item("record_type", self.record_type.into_pyarray(py))?;
        d.set_item("status", PyList::new(py, self.status)?)?;
        if self.full {
            d.set_item("raw_status", self.raw_status.into_pyarray(py))?;
        }
        d.set_item("last_write_ms", self.last_write_ms.into_pyarray(py))?;
        d.set_item("duplicate", PyList::new(py, self.duplicate)?)?;
        d.set_item("recovered", PyList::new(py, self.recovered)?)?;
        let an = PyList::empty(py);
        for a in &self.anomalies {
            an.append((anomaly_name(a.kind), a.address, a.detail.as_str()))?;
        }
        d.set_item("anomalies", an)?;
        Ok(d)
    }
}

/// A decoded time series plus the metadata the Python layer needs.
struct TsData {
    ts: TimeSeries,
    time_offset_seconds: i64,
    pattern: bool,
    blocks_found: usize,
    blocks_missing: Option<usize>,
    warnings: Vec<String>,
    precision: Option<i32>,
    /// P26: DSS6 user header (`None` for DSS7, not decoded there yet).
    user_header: Option<UserHeaderData>,
    /// P25: pattern time-origin facts (`None` for other records).
    pattern_origin: Option<dss_core::timeseries::PatternOrigin>,
    /// P38: DSS7 quality words and notes (`None` for DSS6, which has no notes).
    variants: Option<TsVariantArrays>,
}

/// P38: the optional arrays a DSS7 read returns beyond single-word quality.
struct TsVariantArrays {
    quality_element_size: usize,
    quality_words: Option<Vec<i32>>,
    inote_element_size: usize,
    inotes: Option<Vec<i32>>,
    cnotes: Option<Vec<String>>,
}

fn irregular_block_name(b: IrregularBlock) -> &'static str {
    match b {
        IrregularBlock::Day => "IR-Day",
        IrregularBlock::Month => "IR-Month",
        IrregularBlock::Year => "IR-Year",
        IrregularBlock::Decade => "IR-Decade",
        IrregularBlock::Century => "IR-Century",
    }
}

impl TsData {
    fn into_py<'py>(self, py: Python<'py>, version: u8) -> PyResult<Bound<'py, PyDict>> {
        let TsData {
            ts,
            time_offset_seconds,
            pattern,
            blocks_found,
            blocks_missing,
            warnings,
            precision,
            user_header,
            pattern_origin,
            variants,
        } = self;
        let d = PyDict::new(py);
        d.set_item("version", version)?;
        d.set_item("pathname", lossy(&ts.pathname))?;
        d.set_item("record_type", ts.record_type)?;
        match ts.kind {
            TsKind::Regular { interval_seconds } => {
                d.set_item("regular", true)?;
                d.set_item("interval_seconds", interval_seconds)?;
                d.set_item("irregular_block", py.None())?;
            }
            TsKind::Irregular { block } => {
                d.set_item("regular", false)?;
                d.set_item("interval_seconds", py.None())?;
                d.set_item("irregular_block", irregular_block_name(block))?;
            }
        }
        d.set_item("times", ts.times.into_pyarray(py))?;
        match ts.values {
            TsValues::F32(v) => d.set_item("values", v.into_pyarray(py))?,
            TsValues::F64(v) => d.set_item("values", v.into_pyarray(py))?,
        }
        match ts.quality {
            Some(q) => d.set_item("quality", q.into_pyarray(py))?,
            None => d.set_item("quality", py.None())?,
        }
        d.set_item("units", ts.units)?;
        d.set_item("data_type", ts.data_type)?;
        d.set_item("time_zone", ts.time_zone)?;
        d.set_item("time_offset_seconds", time_offset_seconds)?;
        d.set_item("pattern", pattern)?;
        d.set_item("blocks_found", blocks_found)?;
        d.set_item("blocks_missing", blocks_missing)?;
        d.set_item("warnings", PyList::new(py, warnings)?)?;
        d.set_item("precision", precision)?;
        user_header_into_py(py, &d, user_header)?;
        // P25: pattern time-origin fields; None for non-pattern records.
        d.set_item(
            "pattern_time_convention",
            pattern_origin.map(|o| o.time_convention()),
        )?;
        d.set_item(
            "pattern_stored_offset_seconds",
            pattern_origin.and_then(|o| o.stored_offset_seconds),
        )?;
        d.set_item(
            "pattern_stored_offset_source",
            pattern_origin.map(|o| o.stored_offset_source.as_str()),
        )?;
        d.set_item(
            "pattern_heclib_first_seconds",
            pattern_origin.and_then(|o| o.heclib_first_seconds),
        )?;
        // P38: flat words (`n × element size`); notes hold one char per stored byte.
        let v = variants.unwrap_or(TsVariantArrays {
            quality_element_size: 0,
            quality_words: None,
            inote_element_size: 0,
            inotes: None,
            cnotes: None,
        });
        d.set_item("quality_element_size", v.quality_element_size)?;
        d.set_item("quality_words", v.quality_words.map(|q| q.into_pyarray(py)))?;
        d.set_item("inote_element_size", v.inote_element_size)?;
        d.set_item("inotes", v.inotes.map(|q| q.into_pyarray(py)))?;
        d.set_item("cnotes", v.cnotes)?;
        Ok(d)
    }
}

fn from_dss7_ts(r: dss7::Dss7Ts) -> TsData {
    let missing = r.meta.blocks_in_window.saturating_sub(r.meta.blocks_found);
    TsData {
        pattern_origin: r.pattern_origin(),
        variants: Some(TsVariantArrays {
            quality_element_size: r.meta.quality_element_size,
            quality_words: r.meta.quality_words.clone(),
            inote_element_size: r.meta.inotes_element_size,
            inotes: r.meta.inotes.clone(),
            cnotes: r.meta.cnotes.clone(),
        }),
        time_offset_seconds: r.meta.time_offset_seconds,
        pattern: r.meta.pattern,
        blocks_found: r.meta.blocks_found,
        blocks_missing: Some(missing),
        warnings: Vec::new(),
        precision: Some(r.meta.precision),
        user_header: None,
        ts: r.ts,
    }
}

fn from_dss6_record(r: dss6::Dss6TsRecord) -> TsData {
    let pattern = matches!(
        r.layout,
        TsLayout::RegularPattern | TsLayout::IrregularPattern
    );
    TsData {
        variants: None,
        pattern_origin: r.pattern_origin(),
        time_offset_seconds: i64::from(r.time_offset_minutes) * 60,
        pattern,
        blocks_found: 1,
        blocks_missing: Some(0),
        warnings: Vec::new(),
        precision: Some(r.precision),
        user_header: Some(UserHeaderData {
            words: r.user_header.clone(),
            block: Some(r.pathname.clone()),
            conflicting_blocks: 0,
        }),
        ts: r.to_timeseries(),
    }
}

fn from_dss6_dataset(d: dss6::Dss6Dataset) -> TsData {
    // heclib_f/src/zrrtsi.f:ZRRTSI6 identifies patterns by the D part only.
    let pattern = dss6::pathname::split_parts(&d.series.pathname)
        .map(|parts| dss6::ts::is_pattern_d_part(&d.series.pathname[parts[3].clone()]))
        .unwrap_or(false);
    TsData {
        pattern_origin: d.report.pattern_origin,
        variants: None,
        time_offset_seconds: i64::from(d.report.time_offset_minutes.unwrap_or(0)) * 60,
        pattern,
        blocks_found: d.report.blocks_found,
        blocks_missing: Some(d.report.blocks_missing),
        user_header: Some(UserHeaderData {
            words: d.report.user_header,
            block: d.report.user_header_block,
            conflicting_blocks: d.report.user_header_conflicts,
        }),
        warnings: d.report.warnings,
        precision: None,
        ts: d.series,
    }
}

/// Flat values as a C-ordered `(rows, cols)` array. Extra values are dropped (the
/// readers ignore them too); fewer values than the shape needs is a format error.
fn array_2d<'py, T: numpy::Element>(
    py: Python<'py>,
    mut v: Vec<T>,
    rows: usize,
    cols: usize,
    what: &str,
) -> PyResult<Bound<'py, numpy::PyArray2<T>>> {
    let n = rows
        .checked_mul(cols)
        .ok_or_else(|| DssFormatError::new_err(format!("{what}: shape overflow")))?;
    if v.len() < n {
        return Err(DssFormatError::new_err(format!(
            "{what}: {} values stored, {rows} x {cols} needed",
            v.len()
        )));
    }
    v.truncate(n);
    v.into_pyarray(py).reshape([rows, cols])
}

/// A decoded paired-data record, version-neutral.
struct PdData {
    pd: PairedData,
    version: u8,
}

impl PdData {
    fn into_py<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let PdData { pd, version } = self;
        let (independent, dependent) = pd.parameters();
        let d = PyDict::new(py);
        d.set_item("version", version)?;
        d.set_item("pathname", lossy(&pd.pathname))?;
        d.set_item("record_type", pd.record_type)?;
        d.set_item("number_ordinates", pd.number_ordinates)?;
        d.set_item("number_curves", pd.number_curves)?;
        let (nc, no) = (pd.number_curves, pd.number_ordinates);
        let what = "paired-data values";
        match pd.ordinates {
            TsValues::F32(v) => d.set_item("ordinates", v.into_pyarray(py))?,
            TsValues::F64(v) => d.set_item("ordinates", v.into_pyarray(py))?,
        }
        match pd.values {
            TsValues::F32(v) => d.set_item("values", array_2d(py, v, nc, no, what)?)?,
            TsValues::F64(v) => d.set_item("values", array_2d(py, v, nc, no, what)?)?,
        }
        d.set_item("units_independent", pd.units_independent)?;
        d.set_item("type_independent", pd.type_independent)?;
        d.set_item("units_dependent", pd.units_dependent)?;
        d.set_item("type_dependent", pd.type_dependent)?;
        d.set_item("labels", PyList::new(py, pd.labels)?)?;
        d.set_item("independent_is_x_axis", pd.independent_is_x_axis)?;
        d.set_item("x_precision", pd.x_precision)?;
        d.set_item("y_precision", pd.y_precision)?;
        d.set_item("time_zone", pd.time_zone)?;
        d.set_item("user_header", pd.user_header.into_pyarray(py))?;
        d.set_item("parameters", (independent, dependent))?;
        Ok(d)
    }
}

fn grid_type_name(t: GridType) -> &'static str {
    match t {
        GridType::Undefined => "undefined",
        GridType::Hrap => "hrap",
        GridType::Albers => "albers",
        GridType::Specified => "specified",
    }
}

/// A decoded grid record, version-neutral.
struct GridData {
    grid: Grid,
    version: u8,
}

impl GridData {
    fn into_py<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let GridData { grid: g, version } = self;
        let d = PyDict::new(py);
        d.set_item("version", version)?;
        d.set_item("pathname", lossy(&g.pathname))?;
        d.set_item("record_type", g.record_type)?;
        d.set_item("grid_type", grid_type_name(g.grid_type))?;
        d.set_item("struct_version", g.struct_version)?;
        d.set_item("grid_version", g.version)?;
        d.set_item("info_size", g.info_size)?;
        d.set_item("grid_info_size", g.grid_info_size)?;
        d.set_item("data_units", g.data_units)?;
        d.set_item("data_type", g.data_type)?;
        d.set_item("data_source", g.data_source)?;
        d.set_item("lower_left_cell_x", g.lower_left_cell_x)?;
        d.set_item("lower_left_cell_y", g.lower_left_cell_y)?;
        d.set_item("nx", g.nx)?;
        d.set_item("ny", g.ny)?;
        d.set_item("cell_size", g.cell_size)?;
        d.set_item("x_coord_of_grid_cell_zero", g.x_coord_of_grid_cell_zero)?;
        d.set_item("y_coord_of_grid_cell_zero", g.y_coord_of_grid_cell_zero)?;
        d.set_item("srs_name", g.srs_name)?;
        d.set_item("srs_definition_type", g.srs_definition_type)?;
        d.set_item("srs_definition", g.srs_definition)?;
        d.set_item("stored_srs_definition", g.stored_srs_definition)?;
        match g.albers {
            Some(a) => {
                let p = PyDict::new(py);
                p.set_item("datum", a.datum)?;
                p.set_item("units", a.units)?;
                p.set_item("first_standard_parallel", a.first_standard_parallel)?;
                p.set_item("second_standard_parallel", a.second_standard_parallel)?;
                p.set_item("central_meridian", a.central_meridian)?;
                p.set_item(
                    "latitude_of_projection_origin",
                    a.latitude_of_projection_origin,
                )?;
                p.set_item("false_easting", a.false_easting)?;
                p.set_item("false_northing", a.false_northing)?;
                d.set_item("albers", p)?;
            }
            None => d.set_item("albers", py.None())?,
        }
        d.set_item("null_value", g.null_value)?;
        d.set_item("time_zone_id", g.time_zone_id)?;
        d.set_item("time_zone_raw_offset", g.time_zone_raw_offset)?;
        d.set_item("is_interval", g.is_interval)?;
        d.set_item("is_time_stamped", g.is_time_stamped)?;
        d.set_item("start_time", g.start_time)?;
        d.set_item("end_time", g.end_time)?;
        d.set_item("stored_start_time", g.stored_start_time)?;
        d.set_item("stored_end_time", g.stored_end_time)?;
        d.set_item("compression_method", g.compression_method)?;
        d.set_item("sizeof_compressed_elements", g.sizeof_compressed_elements)?;
        d.set_item("compression_scale_factor", g.compression_scale_factor)?;
        d.set_item("compression_base", g.compression_base)?;
        d.set_item("storage_data_type", g.storage_data_type)?;
        d.set_item("min_data_value", g.min_data_value)?;
        d.set_item("max_data_value", g.max_data_value)?;
        d.set_item("mean_data_value", g.mean_data_value)?;
        d.set_item("range_limit_table", g.range_limit_table.into_pyarray(py))?;
        d.set_item(
            "range_counts",
            g.number_equal_or_exceeding_range_limit.into_pyarray(py),
        )?;
        // Native order: x fastest, lowest row first (index = y * nx + x), so the C-order
        // (ny, nx) view has row 0 at the bottom (Java heclib GridData.getValue).
        d.set_item("data", array_2d(py, g.data, g.ny, g.nx, "grid cells")?)?;
        Ok(d)
    }
}

// ---------------------------------------------------------------------------------
// Open file
// ---------------------------------------------------------------------------------

enum Inner {
    V6 {
        file: Box<Dss6File<FileSource>>,
        index: Option<DatasetIndex>,
    },
    V7 {
        file: Box<Dss7File<File>>,
        catalog: Option<Box<dss7::IndexedDss7Catalog>>,
        recover_catalog: Option<Box<dss7::IndexedDss7Catalog>>,
    },
    Closed,
}

/// An open DSS6 or DSS7 file (read-only). Created by [`open`].
#[pyclass(frozen, module = "ras_commander_fast_dss._native", name = "NativeFile")]
struct NativeFile {
    path: String,
    sig: FileSignature,
    recover: bool,
    inner: Mutex<Inner>,
}

fn record_is_ts(t: i32) -> bool {
    (100..200).contains(&t)
}

fn v7_catalog<'a>(
    file: &mut Dss7File<File>,
    slot: &'a mut Option<Box<dss7::IndexedDss7Catalog>>,
    recover: bool,
    path: &str,
) -> BResult<&'a Dss7Catalog> {
    if slot.is_none() {
        let opts = CatalogOptions {
            recover_unlinked: recover,
            ..CatalogOptions::default()
        };
        *slot = Some(Box::new(dss7::IndexedDss7Catalog::new(
            file.catalog_with(&opts).map_err(|e| map_dss7(path, e))?,
        )));
    }
    Ok(slot.as_deref().expect("catalog just set").catalog())
}

/// P61: the cached catalog snapshot with its whole-dataset range index (built on the
/// first whole read and dropped with the cached catalog).
fn v7_indexed<'a>(
    file: &mut Dss7File<File>,
    slot: &'a mut Option<Box<dss7::IndexedDss7Catalog>>,
    recover: bool,
    path: &str,
) -> BResult<&'a dss7::IndexedDss7Catalog> {
    v7_catalog(file, slot, recover, path)?;
    Ok(slot.as_deref().expect("catalog just set"))
}

impl Inner {
    fn catalog(&mut self, recover: bool, path: &str, sel: CatalogSelect) -> BResult<CatalogData> {
        match self {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                Ok(CatalogData::from_dss7(
                    v7_catalog(file, slot, recover, path)?,
                    sel,
                ))
            }
            Inner::V6 { file, .. } => {
                let walk = file
                    .walk_bins(&dss6::CatalogOptions::default())
                    .map_err(|e| map_dss6(path, e))?;
                Ok(CatalogData::from_dss6(&walk, sel))
            }
            Inner::Closed => Err(BindErr::Value("I/O operation on closed DSS file".into())),
        }
    }

    /// Record type of `pathname` when the file holds it, for typed errors.
    fn record_type_of(&mut self, pathname: &str) -> Option<i32> {
        for p in encodings(pathname) {
            let t = match self {
                Inner::V7 { file, .. } => file.lookup(&p).ok().flatten().map(|r| r.record_type),
                Inner::V6 { file, .. } => {
                    file.lookup(&p).ok().flatten().map(|r| r.entry.record_type)
                }
                Inner::Closed => None,
            };
            if t.is_some() {
                return t;
            }
        }
        None
    }

    /// Turn a not-found failure into the typed unsupported-record error when the
    /// pathname names a record that is not a time series.
    fn refine(&mut self, err: BindErr, pathname: &str, path: &str) -> BindErr {
        if let BindErr::NotFound(_) = err {
            if let Some(t) = self.record_type_of(pathname) {
                if !record_is_ts(t) {
                    return BindErr::Unsupported(ctx(
                        path,
                        format!("record {pathname} has type {t}, which is not a time series"),
                    ));
                }
            }
        }
        err
    }

    fn read_record(
        &mut self,
        pathname: &str,
        recover: bool,
        path: &str,
        no_data: dss6::NoDataPolicy,
    ) -> BResult<TsData> {
        let res = self.read_record_inner(pathname, recover, path, no_data);
        res.map_err(|e| self.refine(e, pathname, path))
    }

    fn read_record_inner(
        &mut self,
        pathname: &str,
        recover: bool,
        path: &str,
        no_data: dss6::NoDataPolicy,
    ) -> BResult<TsData> {
        let mut last = None;
        for p in encodings(pathname) {
            let r = match self {
                Inner::V7 {
                    file,
                    recover_catalog,
                    ..
                } => {
                    let rec = if recover {
                        let cat = v7_catalog(file, recover_catalog, true, path)?;
                        match cat.find(&p) {
                            Some(i) => Some(file.record_at(cat, i).map_err(|e| map_dss7(path, e))?),
                            None => None,
                        }
                    } else {
                        file.lookup(&p).map_err(|e| map_dss7(path, e))?
                    };
                    match rec {
                        Some(rec) => file
                            .read_ts_record(&rec)
                            .map(from_dss7_ts)
                            .map_err(|e| map_ts7_hint(path, e)),
                        None => Err(BindErr::NotFound(ctx(
                            path,
                            format!("record not found: {pathname}"),
                        ))),
                    }
                }
                // P29: the DSS6 "no data" policy (DSS7 has no such rule).
                Inner::V6 { file, .. } => file
                    .read_ts_record_with(&p, &dss6::TsReadOptions::default().with_no_data(no_data))
                    .map(from_dss6_record)
                    .map_err(|e| map_ts6(path, e)),
                Inner::Closed => {
                    return Err(BindErr::Value("I/O operation on closed DSS file".into()))
                }
            };
            match r {
                Err(BindErr::NotFound(m)) => last = Some(BindErr::NotFound(m)),
                other => return other,
            }
        }
        Err(last.expect("at least one encoding"))
    }

    fn read_dataset(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim: bool,
        recover: bool,
        path: &str,
        no_data: dss6::NoDataPolicy,
    ) -> BResult<TsData> {
        let res = self.read_dataset_inner(pathname, window, trim, recover, path, no_data);
        res.map_err(|e| self.refine(e, pathname, path))
    }

    fn read_dataset_inner(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim: bool,
        recover: bool,
        path: &str,
        no_data: dss6::NoDataPolicy,
    ) -> BResult<TsData> {
        if let Some((s, e)) = window {
            if e < s {
                return Err(BindErr::Value("time window end is before its start".into()));
            }
        }
        let mut last = None;
        for p in encodings(pathname) {
            let r = match self {
                Inner::V7 {
                    file,
                    catalog,
                    recover_catalog,
                } => {
                    let opts = TsReadOptions {
                        window: window.map(|(start, end)| TsWindow { start, end }),
                        trim_missing: trim,
                    };
                    if window.is_none() {
                        let slot = if recover { recover_catalog } else { catalog };
                        let cat = v7_indexed(file, slot, recover, path)?;
                        file.read_ts_with_indexed_catalog(cat, &p, &opts)
                    } else {
                        file.read_ts(&p, &opts)
                    }
                    .map(from_dss7_ts)
                    .map_err(|e| map_ts7_hint(path, e))
                }
                Inner::V6 { file, index } => {
                    let opts = DatasetOptions {
                        window: window.map(|(s, e)| TimeWindow::new(s, e)),
                        trim_missing: trim,
                        read: dss6::TsReadOptions::default().with_no_data(no_data),
                        ..DatasetOptions::whole()
                    };
                    if index.is_none() {
                        *index =
                            Some(DatasetIndex::build(file.as_ref()).map_err(|e| map_ts6(path, e))?);
                    }
                    let idx = index.as_ref().expect("index just set");
                    file.read_ts_dataset_indexed(idx, &p, &opts)
                        .map(from_dss6_dataset)
                        .map_err(|e| map_ts6(path, e))
                }
                Inner::Closed => {
                    return Err(BindErr::Value("I/O operation on closed DSS file".into()))
                }
            };
            match r {
                Err(BindErr::NotFound(m)) => last = Some(BindErr::NotFound(m)),
                other => return other,
            }
        }
        Err(last.expect("at least one encoding"))
    }

    /// Read one record by pathname with `read7`/`read6`, trying each encoding of the
    /// pathname as the time-series reads do. DSS7 recovery mode reads the copy the
    /// recovery catalog selects.
    fn read_by_pathname<T>(
        &mut self,
        pathname: &str,
        recover: bool,
        path: &str,
        mut read7: impl FnMut(&mut Dss7File<File>, &RecordRef) -> BResult<T>,
        mut read6: impl FnMut(&Dss6File<FileSource>, &[u8]) -> BResult<T>,
    ) -> BResult<T> {
        let mut last = None;
        for p in encodings(pathname) {
            let r = match self {
                Inner::V7 {
                    file,
                    recover_catalog,
                    ..
                } => {
                    let rec = if recover {
                        let cat = v7_catalog(file, recover_catalog, true, path)?;
                        match cat.find(&p) {
                            Some(i) => Some(file.record_at(cat, i).map_err(|e| map_dss7(path, e))?),
                            None => None,
                        }
                    } else {
                        file.lookup(&p).map_err(|e| map_lookup7(path, e))?
                    };
                    match rec {
                        Some(rec) => read7(file, &rec),
                        None => Err(BindErr::NotFound(ctx(
                            path,
                            format!("record not found: {pathname}"),
                        ))),
                    }
                }
                Inner::V6 { file, .. } => read6(file, &p),
                Inner::Closed => return Err(closed_err()),
            };
            match r {
                Err(BindErr::NotFound(m)) => last = Some(BindErr::NotFound(m)),
                other => return other,
            }
        }
        Err(last.expect("at least one encoding"))
    }

    /// Read the record behind catalog entry `index` (an index into the full native
    /// catalog, every status, in catalog order).
    fn read_at<T>(
        &mut self,
        index: usize,
        recover: bool,
        path: &str,
        read7: impl FnOnce(&mut Dss7File<File>, &RecordRef) -> BResult<T>,
        read6: impl FnOnce(&Dss6File<FileSource>, &dss_core::CatalogEntry) -> BResult<T>,
    ) -> BResult<T> {
        match self {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover, path)?;
                let rec = file.record_at(cat, index).map_err(|e| map_dss7(path, e))?;
                read7(file, &rec)
            }
            Inner::V6 { file, .. } => {
                let walk = file
                    .walk_bins(&dss6::CatalogOptions::default())
                    .map_err(|e| map_dss6(path, e))?;
                let entry = walk.entries.get(index).ok_or_else(|| {
                    BindErr::Index(ctx(path, format!("catalog index {index} out of range")))
                })?;
                if !matches!(
                    entry.record_status(),
                    RecordStatus::Live | RecordStatus::Alias
                ) {
                    return Err(BindErr::Index(ctx(
                        path,
                        format!("catalog entry {index} is not a live record"),
                    )));
                }
                read6(file, &entry.to_catalog_entry())
            }
            Inner::Closed => Err(closed_err()),
        }
    }
}

/// P29: the DSS6 "no data" policy from the Python flag.
fn no_data_policy(refuse: bool) -> dss6::NoDataPolicy {
    if refuse {
        dss6::NoDataPolicy::Refuse
    } else {
        dss6::NoDataPolicy::Return
    }
}

fn grid_options(max_cells: Option<usize>, preserve_legacy_missing: bool) -> GridReadOptions {
    let mut o = GridReadOptions {
        preserve_legacy_missing,
        ..GridReadOptions::default()
    };
    if let Some(n) = max_cells {
        o.max_cells = n;
    }
    o
}

fn closed_err() -> BindErr {
    BindErr::Value("I/O operation on closed DSS file".into())
}

fn read_pd7(file: &mut Dss7File<File>, rec: &RecordRef, path: &str) -> BResult<PdData> {
    file.read_pd_record(rec)
        .map(|r| PdData {
            pd: r.pd,
            version: 7,
        })
        .map_err(|e| map_pd7(path, e))
}

fn read_grid7(
    file: &mut Dss7File<File>,
    rec: &RecordRef,
    opts: &GridReadOptions,
    path: &str,
) -> BResult<GridData> {
    file.read_grid_record_with(rec, opts)
        .map(|grid| GridData { grid, version: 7 })
        .map_err(|e| map_grid(path, &lossy(&rec.pathname), e))
}

/// DSS6 grid behind a catalog entry: re-read and check its info block, as
/// `Dss6File::read_pd_entry` does for paired data.
fn read_grid6_entry(
    file: &Dss6File<FileSource>,
    entry: &dss_core::CatalogEntry,
    opts: &GridReadOptions,
    path: &str,
) -> BResult<GridData> {
    let name = lossy(&entry.pathname);
    let addr = dss6::source::byte_offset_word(entry.info_address).ok_or_else(|| {
        BindErr::Format(ctx(
            path,
            format!("{name}: catalog info offset is not a word"),
        ))
    })?;
    let npath = i32::try_from(entry.pathname.len()).unwrap_or(i32::MAX);
    let info = file.read_info(addr, npath).map_err(|e| map_dss6(path, e))?;
    if !info.has_valid_flag() || info.pathname != entry.pathname {
        return Err(BindErr::Format(ctx(
            path,
            format!("{name}: info block does not match catalog entry"),
        )));
    }
    file.decode_grid_info_with(&info, opts)
        .map(|grid| GridData { grid, version: 6 })
        .map_err(|e| map_grid(path, &name, e))
}

fn open_inner(path: &str) -> BResult<(FileSignature, Inner)> {
    let sig = read_signature(path)?;
    let inner = match sig.version {
        DssVersion::V7 => Inner::V7 {
            file: Box::new(Dss7File::open(path).map_err(|e| map_dss7(path, e))?),
            catalog: None,
            recover_catalog: None,
        },
        DssVersion::V6 => Inner::V6 {
            file: Box::new(Dss6File::open(path).map_err(|e| map_dss6(path, e))?),
            index: None,
        },
    };
    Ok((sig, inner))
}

/// Open a DSS6 or DSS7 file read-only. `recover` (DSS7 only) makes catalogs and record
/// reads use the recovery catalog (unlinked bins, best-preserved duplicate copies).
#[pyfunction]
#[pyo3(signature = (path, recover = false))]
fn open(py: Python<'_>, path: &str, recover: bool) -> PyResult<NativeFile> {
    let p = path.to_string();
    let (sig, inner) = py
        .allow_threads(move || open_inner(&p))
        .map_err(BindErr::into_py)?;
    Ok(NativeFile {
        path: path.to_string(),
        sig,
        recover,
        inner: Mutex::new(inner),
    })
}

impl NativeFile {
    fn with_inner<T: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut Inner, &str) -> BResult<T> + Send,
    ) -> PyResult<T> {
        py.allow_threads(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| BindErr::Dss("DSS file state poisoned by an earlier panic".into()))?;
            f(&mut guard, &self.path)
        })
        .map_err(BindErr::into_py)
    }
}

#[pymethods]
impl NativeFile {
    /// File path as given to `open`.
    #[getter]
    fn path(&self) -> &str {
        &self.path
    }

    /// DSS major version, 6 or 7.
    #[getter]
    fn version(&self) -> u8 {
        match self.sig.version {
            DssVersion::V6 => 6,
            DssVersion::V7 => 7,
        }
    }

    /// Version string from the header (for example `7-HL` or `6-UF`).
    #[getter]
    fn version_string(&self) -> &str {
        &self.sig.version_string
    }

    /// Whether the file was opened in recovery mode.
    #[getter]
    fn recover(&self) -> bool {
        self.recover
    }

    /// Whether `close()` has been called.
    #[getter]
    fn closed(&self) -> bool {
        matches!(
            *self.inner.lock().unwrap_or_else(|e| e.into_inner()),
            Inner::Closed
        )
    }

    /// Header summary as a dict (same keys as `file_signature`).
    fn signature<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        signature_dict(py, &self.sig)
    }

    /// Release the file handle. Further reads raise `ValueError`.
    fn close(&self, py: Python<'_>) {
        py.allow_threads(|| {
            *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Inner::Closed;
        });
    }

    /// Every catalog entry (all statuses), as a dict of parallel lists and arrays.
    /// `recover` (DSS7) overrides the open-time setting; it is ignored for DSS6.
    /// `live_only` exports only live and alias rows, without the
    /// `stored_pathname` and `raw_status` keys; `entries` is always the physical count.
    #[pyo3(signature = (recover = None, live_only = false))]
    fn catalog<'py>(
        &self,
        py: Python<'py>,
        recover: Option<bool>,
        live_only: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let rec = recover.unwrap_or(self.recover);
        let sel = CatalogSelect { live_only };
        let data = self.with_inner(py, |inner, path| inner.catalog(rec, path, sel))?;
        data.into_py(py)
    }

    /// P62: live and alias pathnames in catalog order, as `catalog(live_only=True)`
    /// lists them. Only the pathname column is exported to Python and no DataFrame is
    /// built; the native side still assembles the selected rows' other columns.
    #[pyo3(signature = (recover = None))]
    fn pathnames<'py>(
        &self,
        py: Python<'py>,
        recover: Option<bool>,
    ) -> PyResult<Bound<'py, PyList>> {
        let rec = recover.unwrap_or(self.recover);
        let sel = CatalogSelect { live_only: true };
        let data = self.with_inner(py, |inner, path| inner.catalog(rec, path, sel))?;
        PyList::new(py, data.pathname)
    }

    /// Read one record (one block for regular data) by its full pathname, values as
    /// stored. Missing markers are not converted.
    /// `refuse_no_data`: a DSS6 regular record whose values are all -901/-902
    /// raises `DssNoDataError` (HEC status 4); ignored for DSS7.
    #[pyo3(signature = (pathname, refuse_no_data = false))]
    fn read_record<'py>(
        &self,
        py: Python<'py>,
        pathname: &str,
        refuse_no_data: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let rec = self.recover;
        let pn = pathname.to_string();
        let version = self.version();
        let nd = no_data_policy(refuse_no_data);
        let data = self.with_inner(py, move |inner, path| inner.read_record(&pn, rec, path, nd))?;
        data.into_py(py, version)
    }

    /// Read a dataset (any D part) stitched from its blocks. `start`/`end` are seconds
    /// since 1899-12-31 00:00, both inclusive; both `None` reads the whole dataset (HEC's
    /// "retrieve all times").
    /// `trim_missing` removes leading and trailing missing values (regular data) as
    /// heclib's `ztsTrim` does: -901, -902 and -FLT_MAX are trimmed, a stored NaN is kept.
    /// `refuse_no_data`: a DSS6 regular window with no value other than
    /// -901/-902 raises `DssNoDataError` (HEC status 4); ignored for DSS7.
    #[pyo3(signature = (pathname, start = None, end = None, trim_missing = true, refuse_no_data = false))]
    fn read_dataset<'py>(
        &self,
        py: Python<'py>,
        pathname: &str,
        start: Option<i64>,
        end: Option<i64>,
        trim_missing: bool,
        refuse_no_data: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let window = match (start, end) {
            (None, None) => None,
            (Some(s), Some(e)) => Some((s, e)),
            _ => {
                return Err(PyValueError::new_err(
                    "give both start and end, or neither (whole dataset)",
                ))
            }
        };
        let rec = self.recover;
        let pn = pathname.to_string();
        let version = self.version();
        let nd = no_data_policy(refuse_no_data);
        let data = self.with_inner(py, move |inner, path| {
            inner.read_dataset(&pn, window, trim_missing, rec, path, nd)
        })?;
        data.into_py(py, version)
    }

    /// Read one paired-data record (types 200/205) by its pathname (no D-part
    /// matching; the Python layer does that). Values keep their stored width; missing
    /// markers are not converted.
    fn read_paired_data<'py>(
        &self,
        py: Python<'py>,
        pathname: &str,
    ) -> PyResult<Bound<'py, PyDict>> {
        let rec = self.recover;
        let pn = pathname.to_string();
        let data = self.with_inner(py, move |inner, path| {
            inner.read_by_pathname(
                &pn,
                rec,
                path,
                |f, r| read_pd7(f, r, path),
                |f, p| {
                    f.read_pd(p)
                        .map(|r| PdData {
                            pd: r.pd,
                            version: 6,
                        })
                        .map_err(|e| map_pd6(path, &pn, e))
                },
            )
        })?;
        data.into_py(py)
    }

    /// Read the paired-data record behind catalog entry `index` (row of `catalog()`,
    /// every status). Non-live entries raise `IndexError`.
    fn read_paired_data_at<'py>(
        &self,
        py: Python<'py>,
        index: usize,
    ) -> PyResult<Bound<'py, PyDict>> {
        let rec = self.recover;
        let data = self.with_inner(py, move |inner, path| {
            inner.read_at(
                index,
                rec,
                path,
                |f, r| read_pd7(f, r, path),
                |f, e| {
                    f.read_pd_entry(e)
                        .map(|r| PdData {
                            pd: r.pd,
                            version: 6,
                        })
                        .map_err(|err| map_pd6(path, &lossy(&e.pathname), err))
                },
            )
        })?;
        data.into_py(py)
    }

    /// Read one grid record (types 400-431) by its full pathname. Cells are float32 as
    /// decoded (HEC missing sentinels kept), shaped `(ny, nx)` with row 0 at the bottom.
    /// `max_cells` overrides the 16,777,216-cell limit; `preserve_legacy_missing` keeps
    /// raw legacy ZLIB bits instead of Java heclib's missing normalization (audits).
    #[pyo3(signature = (pathname, max_cells = None, preserve_legacy_missing = false))]
    fn read_grid<'py>(
        &self,
        py: Python<'py>,
        pathname: &str,
        max_cells: Option<usize>,
        preserve_legacy_missing: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let opts = grid_options(max_cells, preserve_legacy_missing);
        let rec = self.recover;
        let pn = pathname.to_string();
        let data = self.with_inner(py, move |inner, path| {
            inner.read_by_pathname(
                &pn,
                rec,
                path,
                |f, r| read_grid7(f, r, &opts, path),
                |f, p| {
                    f.read_grid_with(p, &opts)
                        .map(|grid| GridData { grid, version: 6 })
                        .map_err(|e| map_grid(path, &pn, e))
                },
            )
        })?;
        data.into_py(py)
    }

    /// Read the grid behind catalog entry `index` (row of `catalog()`, every status).
    #[pyo3(signature = (index, max_cells = None, preserve_legacy_missing = false))]
    fn read_grid_at<'py>(
        &self,
        py: Python<'py>,
        index: usize,
        max_cells: Option<usize>,
        preserve_legacy_missing: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let opts = grid_options(max_cells, preserve_legacy_missing);
        let rec = self.recover;
        let data = self.with_inner(py, move |inner, path| {
            inner.read_at(
                index,
                rec,
                path,
                |f, r| read_grid7(f, r, &opts, path),
                |f, e| read_grid6_entry(f, e, &opts, path),
            )
        })?;
        data.into_py(py)
    }

    fn __repr__(&self) -> String {
        format!(
            "<NativeFile {:?} DSS{} {}>",
            self.path,
            self.version(),
            self.sig.version_string
        )
    }

    /// P14: read a DSS7 regular profile dataset (types 102/107; any D part) stitched
    /// from its blocks. `start`/`end` as for `read_dataset`. Values (2-D, times x
    /// depths) and depths keep their stored width; missing markers are not converted.
    /// DSS6 files cannot hold profiles: an existing DSS6 record raises
    /// `DssUnsupportedRecordError`.
    #[pyo3(signature = (pathname, start = None, end = None, trim_missing = true))]
    fn read_profile<'py>(
        &self,
        py: Python<'py>,
        pathname: &str,
        start: Option<i64>,
        end: Option<i64>,
        trim_missing: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        let window = match (start, end) {
            (None, None) => None,
            (Some(s), Some(e)) => Some((s, e)),
            _ => {
                return Err(PyValueError::new_err(
                    "give both start and end, or neither (whole dataset)",
                ))
            }
        };
        let rec = self.recover;
        let pn = pathname.to_string();
        let data = self.with_inner(py, move |inner, path| {
            inner.read_profile(&pn, window, trim_missing, rec, path)
        })?;
        data.into_py(py)
    }

    /// P14: read one DSS7 profile record (one block) by its full pathname, every
    /// interval of the block (missing where nothing is stored). Same dict as
    /// `read_profile`.
    fn read_profile_record<'py>(
        &self,
        py: Python<'py>,
        pathname: &str,
    ) -> PyResult<Bound<'py, PyDict>> {
        let rec = self.recover;
        let pn = pathname.to_string();
        let data = self.with_inner(py, move |inner, path| {
            inner.read_profile_record(&pn, rec, path)
        })?;
        data.into_py(py)
    }
}

// ---------------------------------------------------------------------------------
// P14: regular profile records (DSS7 types 102/107)
// ---------------------------------------------------------------------------------

/// Whether a record type is a DSS7 profile (`zdataTypeDescriptions.h`: 102, 107, 112,
/// 117).
fn record_is_profile(t: i32) -> bool {
    matches!(
        t,
        dss7::ts::RTS_PROFILE
            | dss7::ts::RTD_PROFILE
            | dss7::ts::ITS_PROFILE
            | dss7::ts::ITD_PROFILE
    )
}

/// `map_ts7` for the time-series calls, pointing profile records at `read_profile`.
fn map_ts7_hint(path: &str, e: dss7::TsError) -> BindErr {
    if let dss7::TsError::UnsupportedRecordType { record_type, .. } = &e {
        if record_is_profile(*record_type) {
            return BindErr::Unsupported(ctx(
                path,
                format!("{e}: it is a profile record; read it with read_profile()"),
            ));
        }
    }
    map_ts7(path, e)
}

/// A decoded DSS7 regular profile dataset.
struct ProfileData {
    p: dss7::Dss7Profile,
}

impl ProfileData {
    fn into_py<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dss7::Dss7Profile {
            pathname,
            record_type,
            times,
            depths,
            values,
            depth_units,
            value_units,
            data_type,
            time_zone,
            meta,
        } = self.p;
        let rows = times.len();
        let cols = match &depths {
            TsValues::F32(v) => v.len(),
            TsValues::F64(v) => v.len(),
        };
        let d = PyDict::new(py);
        d.set_item("version", 7u8)?;
        d.set_item("pathname", lossy(&pathname))?;
        d.set_item("record_type", record_type)?;
        d.set_item("regular", meta.interval.code > 0)?;
        d.set_item("interval_seconds", meta.interval.code)?;
        d.set_item("times", times.into_pyarray(py))?;
        match depths {
            TsValues::F32(v) => d.set_item("depths", v.into_pyarray(py))?,
            TsValues::F64(v) => d.set_item("depths", v.into_pyarray(py))?,
        }
        let what = "profile values";
        match values {
            TsValues::F32(v) => d.set_item("values", array_2d(py, v, rows, cols, what)?)?,
            TsValues::F64(v) => d.set_item("values", array_2d(py, v, rows, cols, what)?)?,
        }
        // ztsRetrieveReg7(): `numberToRead * qualityElementSize` quality words, one entry
        // per time. `quality` is the first word per time (as for scalar series);
        // `quality_words` keeps every word.
        let qes = meta.quality_element_size;
        if let (true, Some(words)) = (qes > 0, meta.quality_words) {
            let q: Vec<i32> = (0..rows)
                .map(|i| words.get(i * qes).copied().unwrap_or(0))
                .collect();
            d.set_item("quality_element_size", qes)?;
            d.set_item("quality", q.into_pyarray(py))?;
            d.set_item("quality_words", words.into_pyarray(py))?;
        } else {
            d.set_item("quality_words", py.None())?;
            d.set_item("quality_element_size", 0usize)?;
            d.set_item("quality", py.None())?;
        }
        d.set_item("depth_units", depth_units)?;
        d.set_item("value_units", value_units)?;
        d.set_item("data_type", data_type)?;
        d.set_item("time_zone", time_zone)?;
        d.set_item("precision", meta.precision)?;
        d.set_item("time_offset_seconds", meta.time_offset_seconds)?;
        d.set_item("time_granularity_seconds", meta.time_granularity_seconds)?;
        d.set_item("julian_base_date", meta.julian_base_date)?;
        d.set_item("interval_name", meta.interval.name)?;
        d.set_item("blocks_found", meta.blocks_found)?;
        d.set_item(
            "blocks_missing",
            meta.blocks_in_window.saturating_sub(meta.blocks_found),
        )?;
        d.set_item(
            "block_pathnames",
            PyList::new(py, meta.block_pathnames.iter().map(|p| lossy(p)))?,
        )?;
        let ih = &meta.internal_header;
        d.set_item("internal_header", ih.words.clone().into_pyarray(py))?;
        d.set_item("units_padded", ih.units_padded)?;
        d.set_item("user_header", meta.user_header.into_pyarray(py))?;
        Ok(d)
    }
}

impl Inner {
    fn read_profile(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim: bool,
        recover: bool,
        path: &str,
    ) -> BResult<ProfileData> {
        let res = self.read_profile_inner(pathname, window, trim, recover, path);
        res.map_err(|e| self.refine(e, pathname, path))
    }

    fn read_profile_inner(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim: bool,
        recover: bool,
        path: &str,
    ) -> BResult<ProfileData> {
        if let Some((s, e)) = window {
            if e < s {
                return Err(BindErr::Value("time window end is before its start".into()));
            }
        }
        if let Inner::V6 { .. } = self {
            // heclib Public/ztsStore.c: "Profiles are not implemented in version 6".
            // A DSS6 dataset that exists is therefore never a profile.
            return match self.read_dataset_inner(
                pathname,
                window,
                trim,
                recover,
                path,
                dss6::NoDataPolicy::Return,
            ) {
                Ok(_) => Err(BindErr::Unsupported(ctx(
                    path,
                    format!(
                        "record {pathname} is a time series, not a profile (DSS6 files cannot \
                         store profiles); read it with read_timeseries()"
                    ),
                ))),
                Err(e) => Err(e),
            };
        }
        let mut last = None;
        for p in encodings(pathname) {
            let r = match self {
                Inner::V7 {
                    file,
                    catalog,
                    recover_catalog,
                } => {
                    let opts = TsReadOptions {
                        window: window.map(|(start, end)| TsWindow { start, end }),
                        trim_missing: trim,
                    };
                    if window.is_none() {
                        let slot = if recover { recover_catalog } else { catalog };
                        let cat = v7_indexed(file, slot, recover, path)?;
                        file.read_ts_profile_with_indexed_catalog(cat, &p, &opts)
                    } else {
                        file.read_ts_profile(&p, &opts)
                    }
                    .map(|p| ProfileData { p })
                    .map_err(|e| map_profile7(path, e))
                }
                // DSS6 returned above.
                Inner::V6 { .. } | Inner::Closed => return Err(closed_err()),
            };
            match r {
                Err(BindErr::NotFound(m)) => last = Some(BindErr::NotFound(m)),
                other => return other,
            }
        }
        Err(last.expect("at least one encoding"))
    }

    /// One stored profile record (one block) by its full pathname, through
    /// `Dss7File::read_ts_profile_record` (the whole block, as
    /// `hec_dss_tsRetrieve`/Monolith `HecTimeSeries.read` with no window).
    fn read_profile_record(
        &mut self,
        pathname: &str,
        recover: bool,
        path: &str,
    ) -> BResult<ProfileData> {
        let res = self.read_by_pathname(
            pathname,
            recover,
            path,
            |f, r| {
                f.read_ts_profile_record(r)
                    .map(|p| ProfileData { p })
                    .map_err(|e| map_profile7(path, e))
            },
            |f, p| match f.lookup(p) {
                // heclib Public/ztsStore.c: "Profiles are not implemented in version 6".
                Ok(Some(r)) => Err(BindErr::Unsupported(ctx(
                    path,
                    format!(
                        "record {pathname} has type {}: DSS6 files cannot store profiles",
                        r.entry.record_type
                    ),
                ))),
                Ok(None) => Err(BindErr::NotFound(ctx(
                    path,
                    format!("record not found: {pathname}"),
                ))),
                Err(e) => Err(map_dss6(path, e)),
            },
        );
        res.map_err(|e| self.refine(e, pathname, path))
    }
}

/// `map_ts7` for `read_profile`: a non-profile record says what to call instead.
fn map_profile7(path: &str, e: dss7::TsError) -> BindErr {
    if let dss7::TsError::UnsupportedRecordType { record_type, .. } = &e {
        let t = *record_type;
        let hint = if t == dss7::ts::ITS_PROFILE || t == dss7::ts::ITD_PROFILE {
            "irregular-interval profiles are not decoded"
        } else if matches!(
            t,
            dss7::ts::RTS_PATTERN
                | dss7::ts::RTD_PATTERN
                | dss7::ts::ITS_PATTERN
                | dss7::ts::ITD_PATTERN
        ) {
            "it is a time-series pattern record, not a profile; read it with read_timeseries()"
        } else if record_is_ts(t) {
            "it is not a profile record; read it with read_timeseries()"
        } else {
            "it is not a profile record"
        };
        return BindErr::Unsupported(ctx(
            path,
            format!(
                "record {} has type {t}: {hint}",
                match &e {
                    dss7::TsError::UnsupportedRecordType { pathname, .. } => lossy(pathname),
                    _ => String::new(),
                }
            ),
        ));
    }
    map_ts7(path, e)
}

// ---------------------------------------------------------------------------------
// Location records. Module-level functions taking a NativeFile, so
// this block stays self-contained.
// ---------------------------------------------------------------------------------

/// A decoded location (DSS7 type 20 record, or DSS6 time-series header coordinates).
struct LocData {
    loc: dss_core::Location,
    version: u8,
    /// Type of the record read: 20, or the DSS6 time-series type.
    record_type: i32,
    values1: Option<Vec<i32>>,
    program: Option<String>,
    last_write_ms: Option<i64>,
    /// P27: DSS6 block date (julian) from the D part (`dss6::interval::parse_d_part`, as
    /// `DatasetIndex` sorts blocks), so the Python dataset fallback can pick the block
    /// whose user header HEC reports (dss6.md §14.4). `None` for DSS7.
    block_julian: Option<i64>,
}

impl LocData {
    fn from_dss7(r: dss7::Dss7Location) -> Self {
        LocData {
            loc: r.location,
            version: 7,
            record_type: dss_core::location::LOCATION,
            values1: Some(r.values1),
            program: Some(r.program),
            last_write_ms: Some(r.last_write_ms),
            block_julian: None,
        }
    }

    fn into_py<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let l = self.loc;
        let supplemental_source = l.supplemental_source().map(|s| s.as_str());
        // P28: the decoded text and its decoding, from dss-core (DSS6 user-header text:
        // UTF-8, else UTF-8 with U+FFFD; DSS7: Latin-1). `None` when
        // `supplemental` is `None`.
        let supplemental_text = l.supplemental_text();
        let supplemental_encoding = l.supplemental_encoding();
        let d = PyDict::new(py);
        d.set_item("version", self.version)?;
        d.set_item("pathname", lossy(&l.pathname))?;
        d.set_item("record_type", self.record_type)?;
        d.set_item(
            "source",
            match l.source {
                dss_core::LocationSource::Dss7Record => "dss7_location_record",
                dss_core::LocationSource::Dss6TsHeader => "dss6_ts_header",
            },
        )?;
        d.set_item("x", l.x)?;
        d.set_item("y", l.y)?;
        d.set_item("z", l.z)?;
        d.set_item("descriptors_stored", l.descriptors_stored)?;
        d.set_item("coordinate_system", l.coordinate_system_code)?;
        d.set_item("coordinate_id", l.coordinate_id)?;
        d.set_item("horizontal_units", l.horizontal_units_code)?;
        d.set_item("horizontal_datum", l.horizontal_datum_code)?;
        d.set_item("vertical_units", l.vertical_units_code)?;
        d.set_item("vertical_datum", l.vertical_datum_code)?;
        d.set_item("time_zone", l.time_zone_name)?;
        d.set_item(
            "supplemental",
            l.supplemental
                .as_deref()
                .map(|b| pyo3::types::PyBytes::new(py, b)),
        )?;
        d.set_item("values1", self.values1)?;
        d.set_item("program", self.program)?;
        d.set_item("last_write_ms", self.last_write_ms)?;
        // P27.
        d.set_item("supplemental_source", supplemental_source)?;
        d.set_item("block_julian", self.block_julian)?;
        d.set_item("supplemental_text", supplemental_text)?;
        d.set_item("supplemental_encoding", supplemental_encoding)?;
        Ok(d)
    }
}

fn map_loc7(path: &str, e: dss7::LocationError) -> BindErr {
    use dss7::LocationError as E;
    match e {
        E::Dss7(inner) => map_lookup7(path, inner),
        E::NotFound { .. } | E::BadPathname { .. } => BindErr::NotFound(ctx(path, &e)),
        E::UnsupportedRecordType { .. } => BindErr::Unsupported(ctx(path, &e)),
        E::Corrupt { .. } => BindErr::Format(ctx(path, &e)),
    }
}

/// DSS6 has no location records (`zlocationRetrieve()` returns INCOMPATIBLE_VERSION).
fn dss6_no_location_records(path: &str, pathname: &str) -> BindErr {
    BindErr::Unsupported(ctx(
        path,
        format!(
            "{pathname}: DSS6 files have no location records (type 20 is DSS7 only); \
             use read_associated_location for time-series coordinates"
        ),
    ))
}

/// Read the location record (type 20) under `pathname`. DSS6 raises
/// `DssUnsupportedRecordError`.
#[pyfunction]
fn location_read<'py>(
    py: Python<'py>,
    file: &Bound<'py, NativeFile>,
    pathname: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let f = file.get();
    let rec = f.recover;
    let pn = pathname.to_string();
    let data = f.with_inner(py, move |inner, path| {
        inner.read_by_pathname(
            &pn,
            rec,
            path,
            |file, r| {
                file.read_location_record(r)
                    .map(LocData::from_dss7)
                    .map_err(|e| map_loc7(path, e))
            },
            |_, _| Err(dss6_no_location_records(path, &pn)),
        )
    })?;
    data.into_py(py)
}

/// DSS7 associated location. Only "no record at `/A/B/Location Info////`" gives
/// `Ok(None)`; a pathname without A/B parts or a location pathname that is not a valid
/// DSS7 pathname (over 393 bytes) raises `DssPathnameNotFoundError`, and every other
/// failure keeps its own error.
fn v7_associated(
    inner: &mut Inner,
    pn: &str,
    recover: bool,
    path: &str,
) -> BResult<Option<LocData>> {
    let Inner::V7 {
        file,
        recover_catalog,
        ..
    } = inner
    else {
        return Err(closed_err());
    };
    let lp = dss_core::location::location_pathname(pn.as_bytes()).ok_or_else(|| {
        BindErr::NotFound(ctx(path, format!("pathname {pn} has no A and B parts")))
    })?;
    for p in encodings(&lossy(&lp)) {
        if p.is_empty() || p.len() > dss7::MAX_PATHNAME_BYTES {
            return Err(BindErr::NotFound(ctx(
                path,
                format!(
                    "location pathname for {pn} is {} bytes; DSS7 pathnames are 1..393 bytes",
                    p.len()
                ),
            )));
        }
        let rec = if recover {
            let cat = v7_catalog(file, recover_catalog, true, path)?;
            match cat.find(&p) {
                Some(i) => Some(file.record_at(cat, i).map_err(|e| map_dss7(path, e))?),
                None => None,
            }
        } else {
            file.lookup(&p).map_err(|e| map_dss7(path, e))?
        };
        if let Some(r) = rec {
            return file
                .read_location_record(&r)
                .map(|l| Some(LocData::from_dss7(l)))
                .map_err(|e| map_loc7(path, e));
        }
    }
    Ok(None)
}

/// The location associated with `pathname`, or `None`.
///
/// DSS7: the record `/A/B/Location Info////` (`zlocationPath()`); the data record need
/// not exist. DSS6: the coordinates in the time-series record's internal header (the
/// record must exist; non-time-series records have no location and give `None`), with
/// the supplemental text taken from the same record's user header (P27, Monolith
/// `supplementalInfo` behavior; informational).
#[pyfunction]
fn location_read_associated<'py>(
    py: Python<'py>,
    file: &Bound<'py, NativeFile>,
    pathname: &str,
) -> PyResult<Option<Bound<'py, PyDict>>> {
    let f = file.get();
    let rec = f.recover;
    let pn = pathname.to_string();
    let data = f.with_inner(py, move |inner, path| {
        if let Inner::V7 { .. } = inner {
            return v7_associated(inner, &pn, rec, path);
        }
        inner.read_by_pathname(
            &pn,
            rec,
            path,
            |_, _| Err(closed_err()),
            |file, p| match file.read_ts_record(p) {
                Ok(ts) => Ok(dss_core::Location::from_dss6_ts(
                    &ts.pathname,
                    ts.coordinates,
                    ts.coordinate_descriptors,
                )
                .map(|loc| LocData {
                    // P27: supplemental text from this record's user header.
                    loc: loc.with_dss6_user_header(&ts.user_header),
                    version: 6,
                    record_type: ts.record_type,
                    values1: None,
                    program: None,
                    last_write_ms: None,
                    block_julian: dss6::pathname::split_parts(&ts.pathname)
                        .and_then(|p| dss6::interval::parse_d_part(&ts.pathname[p[3].clone()])),
                })),
                Err(dss6::TsError::UnsupportedRecordType { .. }) => Ok(None),
                Err(e) => Err(map_ts6(path, e)),
            },
        )
    })?;
    data.map(|d| d.into_py(py)).transpose()
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(file_signature, m)?)?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_class::<NativeFile>()?;
    // P19a: lock detection.
    m.add_function(wrap_pyfunction!(lock_status, m)?)?;
    m.add_function(wrap_pyfunction!(location_read, m)?)?;
    m.add_function(wrap_pyfunction!(location_read_associated, m)?)?;
    // Catalog-index time-series route.
    m.add_function(wrap_pyfunction!(ts_entry_provenance, m)?)?;
    m.add_function(wrap_pyfunction!(read_record_at, m)?)?;
    // P20: DSS7 regular time-series writer.
    m.add_function(wrap_pyfunction!(create_dss7, m)?)?;
    m.add_function(wrap_pyfunction!(repair_dss7, m)?)?;
    m.add_function(wrap_pyfunction!(write_rts, m)?)?;
    m.add_function(wrap_pyfunction!(rts_value_times, m)?)?;
    // P21: DSS7 irregular time-series writer.
    m.add_function(wrap_pyfunction!(write_its, m)?)?;
    // P22: DSS6 time-series writer.
    m.add_function(wrap_pyfunction!(create_dss6, m)?)?;
    m.add_function(wrap_pyfunction!(write_rts6, m)?)?;
    m.add_function(wrap_pyfunction!(rts6_value_times, m)?)?;
    m.add_function(wrap_pyfunction!(write_its6, m)?)?;
    // P23: paired-data writer (DSS7 and DSS6).
    m.add_function(wrap_pyfunction!(write_pd, m)?)?;
    // P38: time-series variants.
    m.add_function(wrap_pyfunction!(write_rts_variants, m)?)?;
    m.add_function(wrap_pyfunction!(write_its_variants, m)?)?;
    m.add_function(wrap_pyfunction!(write_pattern, m)?)?;
    // P24: grid writer (DSS7 and DSS6).
    m.add_function(wrap_pyfunction!(write_grid, m)?)?;
    // P39: profile and location writers.
    m.add_function(wrap_pyfunction!(write_profile, m)?)?;
    m.add_function(wrap_pyfunction!(write_location, m)?)?;
    #[cfg(feature = "vortex")]
    vortex::register(m)?;
    #[cfg(feature = "vortex")]
    vortex_temporal::register(m)?;
    #[cfg(feature = "vortex")]
    vortex_zonal::register(m)?;
    #[cfg(feature = "vortex")]
    vortex_tools::register(m)?;
    #[cfg(feature = "vortex")]
    vortex_zarr::register(m)?;
    Ok(())
}

// =================================================================================
// Explicit catalog-index route for time series.
//
// Forensic reads of one catalog entry (any live or alias row of the full catalog,
// including duplicates, orphan-bin entries and DSS7 recovery-mode entries), with
// catalog-entry provenance. Integrity checks are those of `Dss7File::record_at`
// (info flag and info hash) and `Dss6File::read_ts_entry`; nothing is
// bypassed. A zeroed or invalid info block raises `DssRecordUnavailableError`, a
// `DssFormatError` subclass. The ordinary pathname reads above are not
// changed.
// =================================================================================

pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssRecordUnavailableError);

/// Where a catalog entry sits and whether an ordinary pathname read reaches it.
struct EntryProvenance {
    index: usize,
    version: u8,
    pathname: String,
    record_type: i32,
    status: &'static str,
    raw_status: i64,
    recover_mode: bool,
    recovered: bool,
    duplicate: bool,
    duplicate_of: Option<usize>,
    hash_chain: &'static str,
    /// Byte offsets (as in the catalog anomalies).
    bin_entry_address: u64,
    info_address: u64,
    /// A HEC-style hash lookup of the stored pathname lands on this entry.
    reachable_by_lookup: bool,
    /// The ordinary pathname read of this file, in its open mode, reads this entry
    /// (default mode: the hash lookup; recovery mode: the recovery catalog's choice).
    selected_by_pathname: bool,
    /// Why the info block cannot be used (`None` when it verifies or the entry is not
    /// live): zeroed, bad_flag, hash_mismatch, status, pathname_mismatch,
    /// out_of_bounds, unreadable.
    info_problem: Option<&'static str>,
    info_detail: Option<String>,
    /// The time-series record decoder accepts this record type ([`ts_supported7`],
    /// [`ts_supported6`]).
    ts_supported: bool,
    /// DSS6: HEC denies access to the record (password set; `zrdinf6.f:ZRDINF6` lines
    /// 65-70, `Dss6File::is_record_denied`). Always false for DSS7.
    password_protected: bool,
}

impl EntryProvenance {
    fn into_py<'py>(self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("index", self.index)?;
        d.set_item("dss_version", self.version)?;
        d.set_item("pathname", self.pathname)?;
        d.set_item("record_type", self.record_type)?;
        d.set_item("status", self.status)?;
        d.set_item("raw_status", self.raw_status)?;
        d.set_item("recover_mode", self.recover_mode)?;
        d.set_item("recovered", self.recovered)?;
        d.set_item("duplicate", self.duplicate)?;
        d.set_item("duplicate_of", self.duplicate_of)?;
        d.set_item("hash_chain", self.hash_chain)?;
        d.set_item("bin_entry_address", self.bin_entry_address)?;
        d.set_item("info_address", self.info_address)?;
        d.set_item("reachable_by_lookup", self.reachable_by_lookup)?;
        d.set_item("selected_by_pathname", self.selected_by_pathname)?;
        d.set_item("info_problem", self.info_problem)?;
        d.set_item("info_detail", self.info_detail)?;
        d.set_item("ts_supported", self.ts_supported)?;
        d.set_item("password_protected", self.password_protected)?;
        Ok(d)
    }
}

/// Errors of the catalog-index route: the shared mapping, or a typed unavailable
/// record carrying its provenance facts.
enum AtErr {
    Bind(BindErr),
    Unavailable {
        msg: String,
        problem: &'static str,
        index: usize,
        info_address: u64,
    },
}

impl From<BindErr> for AtErr {
    fn from(e: BindErr) -> Self {
        AtErr::Bind(e)
    }
}

impl AtErr {
    fn into_py(self) -> PyErr {
        match self {
            AtErr::Bind(e) => e.into_py(),
            AtErr::Unavailable {
                msg,
                problem,
                index,
                info_address,
            } => DssRecordUnavailableError::new_err((msg, problem, index, info_address)),
        }
    }
}

fn chain_name(c: dss7::ChainState) -> &'static str {
    match c {
        dss7::ChainState::NotChecked => "not_checked",
        dss7::ChainState::OwnChain(_) => "own_chain",
        dss7::ChainState::OtherSlot => "other_slot",
        dss7::ChainState::Orphan => "orphan",
    }
}

fn info_problem_code(p: dss7::InfoProblem) -> &'static str {
    use dss7::InfoProblem as P;
    match p {
        P::Zeroed => "zeroed",
        P::BadFlag(_) => "bad_flag",
        P::HashMismatch => "hash_mismatch",
        P::Status(_) => "status",
        P::PathnameMismatch => "pathname_mismatch",
        P::OutOfBounds => "out_of_bounds",
    }
}

/// DSS7 record types `Dss7File::read_ts_record` decodes as one record: regular and
/// irregular, float and double, series and patterns (heclib_c `headers/zdssKeys.h`
/// data types 100/101/105/106/110/111/115/116; `ztsRetrieveReg`/`ztsRetrieveIrreg`,
/// `ztsRetrievePattern`). Profiles (102/107/112/117) and other 1xx codes are refused.
fn ts_supported7(t: i32) -> bool {
    matches!(t, 100 | 101 | 105 | 106 | 110 | 111 | 115 | 116)
}

/// DSS6 record types `Dss6File::decode_ts_info` decodes (`zrrtsx6.f`/`zritsx6.f`
/// via `zrrtsc6`/`zritsc6`: 100/101/105/110/111/115). Checked against the info block's
/// `KITYPE`, which is what the decoder reads.
fn ts_supported6(t: i32) -> bool {
    matches!(t, 100 | 101 | 105 | 110 | 111 | 115)
}

fn is_live_or_alias7(s: i64) -> bool {
    (dss7::status::VALID..=dss7::status::PRIMARY).contains(&s) || s == dss7::status::ALIAS
}

/// DSS7 provenance of catalog entry `index`; also returns the verified record when
/// its info block passes `record_at`'s checks.
fn provenance7(
    file: &mut Dss7File<File>,
    cat: &Dss7Catalog,
    index: usize,
    recover: bool,
    path: &str,
) -> BResult<(EntryProvenance, Option<RecordRef>)> {
    let (Some(e), Some(m)) = (cat.catalog.entries.get(index), cat.meta.get(index)) else {
        return Err(BindErr::Index(ctx(
            path,
            format!("catalog index {index} out of range"),
        )));
    };
    let live = is_live_or_alias7(m.raw_status);
    let entry_word = m.bin_address + i64::from(m.entry_offset);
    // Hash lookup of the stored pathname (`zcheck()`, Dss7File::lookup). An entry whose
    // info block is bad is still "reached" when the lookup fails on its info block.
    let reachable_by_lookup = live
        && match file.lookup(&e.pathname) {
            Ok(Some(r)) => r.bin_entry_address == entry_word,
            Ok(None) => false,
            Err(dss7::Dss7Error::BadInfoBlock { info_address, .. }) => {
                info_address == m.info_address
            }
            Err(dss7::Dss7Error::Io(io)) => return Err(BindErr::Os(io, path.to_string())),
            Err(_) => false,
        };
    let selected_by_pathname = live
        && if recover {
            // Equivalent to `cat.find(&e.pathname) == Some(index)` without its linear
            // scan: duplicate groups are case-insensitive over live and alias entries
            // (walk.rs `duplicate_groups`), and `find` returns the one entry of a group
            // whose `duplicate_of` is unset.
            m.duplicate_of.is_none()
        } else {
            reachable_by_lookup
        };
    let (mut info_problem, mut info_detail, mut rec) = (None, None, None);
    if live {
        match file.record_at(cat, index) {
            Ok(r) => rec = Some(r),
            Err(dss7::Dss7Error::BadInfoBlock { problem, .. }) => {
                info_problem = Some(info_problem_code(problem));
                info_detail = Some(problem.to_string());
            }
            Err(dss7::Dss7Error::Io(io)) => return Err(BindErr::Os(io, path.to_string())),
            Err(other) => {
                info_problem = Some("unreadable");
                info_detail = Some(other.to_string());
            }
        }
    }
    let p = EntryProvenance {
        index,
        version: 7,
        pathname: lossy(&e.pathname),
        record_type: e.record_type,
        status: status_name(e.status),
        raw_status: m.raw_status,
        recover_mode: recover,
        recovered: m.recovered,
        duplicate: m.duplicate_of.is_some(),
        duplicate_of: m.duplicate_of,
        hash_chain: chain_name(m.chain),
        bin_entry_address: e.bin_address,
        info_address: e.info_address,
        reachable_by_lookup,
        selected_by_pathname,
        info_problem,
        info_detail,
        ts_supported: ts_supported7(e.record_type),
        password_protected: false,
    };
    Ok((p, rec))
}

/// DSS6 provenance of catalog entry `index` (bin-walk order, as `catalog()`); also
/// returns the catalog entry when its info block verifies. DSS6 has no recovery mode
/// and no orphan check, so `hash_chain` is `not_checked`.
fn provenance6(
    file: &Dss6File<FileSource>,
    index: usize,
    path: &str,
) -> BResult<(EntryProvenance, Option<dss_core::CatalogEntry>)> {
    let walk = file
        .walk_bins(&dss6::CatalogOptions::default())
        .map_err(|e| map_dss6(path, e))?;
    let entry = walk
        .entries
        .get(index)
        .ok_or_else(|| BindErr::Index(ctx(path, format!("catalog index {index} out of range"))))?;
    let st = entry.record_status();
    let live = matches!(st, RecordStatus::Live | RecordStatus::Alias);
    // Same duplicate rule as CatalogData::from_dss6: a later live copy of a pathname.
    let duplicate_of = if st == RecordStatus::Live {
        walk.entries[..index]
            .iter()
            .position(|o| o.record_status() == RecordStatus::Live && o.pathname == entry.pathname)
    } else {
        None
    };
    let reachable_by_lookup = live
        && match file.lookup(&entry.pathname) {
            Ok(Some(r)) => r.entry.entry_address == entry.entry_address,
            Ok(None) => false,
            Err(dss6::Dss6Error::Io(io)) => return Err(BindErr::Os(io, path.to_string())),
            Err(_) => false,
        };
    let ce = entry.to_catalog_entry();
    let (mut info_problem, mut info_detail) = (None, None);
    let (mut info_type, mut password_protected) = (None, false);
    if live {
        let npath = i32::try_from(entry.pathname.len()).unwrap_or(i32::MAX);
        // Same checks as Dss6File::read_ts_entry (zrdinf6.f: flag and pathname).
        match file.read_info(i64::from(entry.info_address), npath) {
            Ok(info) if !info.has_valid_flag() => {
                info_problem = Some(if info.flag == 0 { "zeroed" } else { "bad_flag" });
                info_detail = Some(format!("info flag {}", info.flag));
            }
            Ok(info) if info.pathname != entry.pathname => {
                info_problem = Some("pathname_mismatch");
                info_detail = Some("info pathname differs from bin pathname".into());
            }
            Ok(info) => {
                info_type = Some(info.record_type);
                password_protected = file.is_record_denied(&info);
            }
            Err(dss6::Dss6Error::Io(io)) => return Err(BindErr::Os(io, path.to_string())),
            Err(e @ (dss6::Dss6Error::OutOfBounds { .. } | dss6::Dss6Error::LockRecord { .. })) => {
                info_problem = Some("out_of_bounds");
                info_detail = Some(e.to_string());
            }
            Err(e) => {
                // An all-zero block fails decoding on its zero pathname length (word 2,
                // `KINPAT`); report it as zeroed, like the DSS7 flag check.
                let zero = file
                    .read_words(i64::from(entry.info_address), 3)
                    .is_ok_and(|w| w.iter().all(|&x| x == 0));
                info_problem = Some(if zero { "zeroed" } else { "unreadable" });
                info_detail = Some(if zero {
                    "info block is zeroed".to_string()
                } else {
                    e.to_string()
                });
            }
        }
    }
    let p = EntryProvenance {
        index,
        version: 6,
        pathname: lossy(&entry.pathname),
        record_type: entry.record_type,
        status: status_name(st),
        raw_status: i64::from(entry.status),
        recover_mode: false,
        recovered: false,
        duplicate: duplicate_of.is_some(),
        duplicate_of,
        hash_chain: chain_name(dss7::ChainState::NotChecked),
        bin_entry_address: ce.bin_address,
        info_address: ce.info_address,
        reachable_by_lookup,
        selected_by_pathname: reachable_by_lookup,
        info_problem,
        info_detail,
        ts_supported: ts_supported6(info_type.unwrap_or(entry.record_type)),
        password_protected,
    };
    let ok = (live && p.info_problem.is_none()).then_some(ce);
    Ok((p, ok))
}

impl Inner {
    fn entry_provenance(
        &mut self,
        index: usize,
        recover: bool,
        path: &str,
    ) -> BResult<EntryProvenance> {
        match self {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover, path)?;
                Ok(provenance7(file, cat, index, recover, path)?.0)
            }
            Inner::V6 { file, .. } => Ok(provenance6(file, index, path)?.0),
            Inner::Closed => Err(closed_err()),
        }
    }

    /// One stored time-series record behind catalog entry `index`.
    fn read_ts_at(
        &mut self,
        index: usize,
        recover: bool,
        path: &str,
    ) -> Result<(EntryProvenance, TsData), AtErr> {
        let unavailable = |p: &EntryProvenance| {
            AtErr::Unavailable {
            msg: ctx(
                path,
                format!(
                    "catalog entry {} ({}) is unavailable: {} (info block at byte {}); no data is returned",
                    p.index,
                    p.pathname,
                    p.info_detail.as_deref().unwrap_or("invalid info block"),
                    p.info_address
                ),
            ),
            problem: p.info_problem.unwrap_or("unreadable"),
            index: p.index,
            info_address: p.info_address,
        }
        };
        let unsupported = |p: &EntryProvenance| {
            AtErr::Bind(BindErr::Unsupported(ctx(
                path,
                format!(
                    "catalog entry {} ({}) has record type {}, which the time-series record reader does not decode",
                    p.index, p.pathname, p.record_type
                ),
            )))
        };
        let not_live = |p: &EntryProvenance| {
            AtErr::Bind(BindErr::Index(ctx(
                path,
                format!(
                    "catalog entry {} is not a live record (status {})",
                    p.index, p.status
                ),
            )))
        };
        match self {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover, path)?;
                let (p, rec) = provenance7(file, cat, index, recover, path)?;
                if !is_live_or_alias7(p.raw_status) {
                    return Err(not_live(&p));
                }
                let Some(rec) = rec else {
                    return Err(unavailable(&p));
                };
                if !p.ts_supported {
                    return Err(unsupported(&p));
                }
                let ts = file
                    .read_ts_record(&rec)
                    .map(from_dss7_ts)
                    .map_err(|e| map_ts7(path, e))?;
                Ok((p, ts))
            }
            Inner::V6 { file, .. } => {
                let (p, ce) = provenance6(file, index, path)?;
                if !matches!(p.status, "live" | "alias") {
                    return Err(not_live(&p));
                }
                let Some(ce) = ce else {
                    return Err(unavailable(&p));
                };
                // Password-protected records fall through to the decoder, which raises
                // the documented DssError.
                if !p.ts_supported {
                    return Err(unsupported(&p));
                }
                let ts = file
                    .read_ts_entry(&ce, &dss6::TsReadOptions::default())
                    .map(from_dss6_record)
                    .map_err(|e| {
                        let name = lossy(&ce.pathname);
                        match map_ts6(path, e) {
                            BindErr::Unsupported(m) => {
                                BindErr::Unsupported(format!("{m} ({name})"))
                            }
                            other => other,
                        }
                    })?;
                Ok((p, ts))
            }
            Inner::Closed => Err(closed_err().into()),
        }
    }
}

fn with_inner_at<T: Send>(
    py: Python<'_>,
    nf: &NativeFile,
    f: impl FnOnce(&mut Inner, &str) -> Result<T, AtErr> + Send,
) -> PyResult<T> {
    py.allow_threads(|| {
        let mut guard = nf
            .inner
            .lock()
            .map_err(|_| BindErr::Dss("DSS file state poisoned by an earlier panic".into()))?;
        f(&mut guard, &nf.path)
    })
    .map_err(AtErr::into_py)
}

/// Provenance of catalog entry `index` (row of `NativeFile.catalog()`, every status, in
/// the file's open mode) without decoding values: addresses, recovered/duplicate
/// flags, hash-chain state, whether ordinary pathname reads reach it, and the
/// info-block problem if any.
#[pyfunction]
fn ts_entry_provenance<'py>(
    py: Python<'py>,
    file: &Bound<'py, NativeFile>,
    index: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let nf = file.get();
    let rec = nf.recover;
    let p = with_inner_at(py, nf, move |inner, path| {
        inner
            .entry_provenance(index, rec, path)
            .map_err(AtErr::from)
    })?;
    p.into_py(py)
}

/// Read the stored time-series record behind catalog entry `index`; returns
/// `(provenance, record)`, the record dict shaped like `NativeFile.read_record`.
/// Raises `DssRecordUnavailableError` for a zeroed or invalid info block,
/// `DssUnsupportedRecordError` for non-time-series records and `IndexError` for an
/// index out of range or a non-live entry.
#[pyfunction]
fn read_record_at<'py>(
    py: Python<'py>,
    file: &Bound<'py, NativeFile>,
    index: usize,
) -> PyResult<(Bound<'py, PyDict>, Bound<'py, PyDict>)> {
    let nf = file.get();
    let rec = nf.recover;
    let version = nf.version();
    let (p, ts) = with_inner_at(py, nf, move |inner, path| {
        inner.read_ts_at(index, rec, path)
    })?;
    Ok((p.into_py(py)?, ts.into_py(py, version)?))
}

// =================================================================================
// P19a : read-only HEC lock detection.
//
// `lock_status(path)` reports which HEC lock words another handle holds
// (`dss_core::lock`). It is a diagnostic: reads never call it and never raise
// `DssLockedError`. No writer exists yet.
// =================================================================================

fn map_lock(path: &str, e: dss_core::lock::LockError) -> BindErr {
    use dss_core::lock::LockError as E;
    match e {
        E::Io(io) => BindErr::Os(io, path.to_string()),
        E::Header(_) | E::BadLockLayout { .. } => BindErr::Format(ctx(path, e)),
        E::Unsupported(_) | E::Probe { .. } => BindErr::Dss(ctx(path, e)),
    }
}

/// Probe the HEC lock words of a DSS file; returns a dict (see `ras_commander_fast_dss.lock_status`).
#[pyfunction]
fn lock_status<'py>(py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyDict>> {
    let p = path.to_string();
    let r = py
        .allow_threads(move || dss_core::lock::lock_status(std::path::Path::new(&p)))
        .map_err(|e| map_lock(path, e).into_py())?;
    let d = PyDict::new(py);
    d.set_item("version", r.version)?;
    d.set_item("version_string", &r.version_string)?;
    d.set_item("locked", r.locked())?;
    let reasons: Vec<&str> = r.reasons().iter().map(|x| x.as_str()).collect();
    d.set_item("reasons", reasons)?;
    let held = PyList::empty(py);
    for (reason, byte, length) in &r.held {
        let h = PyDict::new(py);
        h.set_item("reason", reason.as_str())?;
        h.set_item("byte", *byte)?;
        h.set_item("length", *length)?;
        held.append(h)?;
    }
    d.set_item("held", held)?;
    d.set_item("readers_detectable", r.readers_detectable)?;
    d.set_item("holder_pids", r.holder_pids.clone())?;
    d.set_item("method", r.method)?;
    d.set_item("ranges_tested", r.ranges_tested)?;
    Ok(d)
}

// ---------------------------------------------------------------------------------
// P26: DSS6 time-series user header
// ---------------------------------------------------------------------------------

/// User header of a DSS6 time-series read: the stored words of the block HEC reports,
/// that block's pathname, and how many blocks of a dataset read disagree with it
/// (`dss_core::dss6::DatasetReport::user_header*`).
struct UserHeaderData {
    words: Vec<i32>,
    block: Option<Vec<u8>>,
    conflicting_blocks: usize,
}

/// Add the `user_header_*` keys (docs/format/dss6.md §14). DSS7 reads (`None`) get
/// `None` for every key: DSS7 user headers are not decoded by this reader.
fn user_header_into_py<'py>(
    py: Python<'py>,
    d: &Bound<'py, PyDict>,
    uh: Option<UserHeaderData>,
) -> PyResult<()> {
    let Some(uh) = uh else {
        for k in [
            "user_header_words",
            "user_header_bytes",
            "user_header_text",
            "user_header_items",
            "user_header_block",
            "user_header_conflict",
            "user_header_encoding",
        ] {
            d.set_item(k, py.None())?;
        }
        return Ok(());
    };
    let text = dss_core::user_header::user_header_text(&uh.words);
    let encoding = dss_core::user_header::user_header_text_encoding(&uh.words);
    let items = PyDict::new(py);
    // hec.io.DataContainer.getSupplementalInfo() puts items in a HashMap: last wins.
    for (k, v) in dss_core::user_header::parse_user_header_items(&text) {
        items.set_item(k, v)?;
    }
    let bytes = dss_core::user_header::user_header_bytes(&uh.words);
    d.set_item("user_header_bytes", pyo3::types::PyBytes::new(py, &bytes))?;
    d.set_item("user_header_words", uh.words.into_pyarray(py))?;
    d.set_item("user_header_text", text)?;
    d.set_item("user_header_items", items)?;
    d.set_item("user_header_block", uh.block.as_deref().map(lossy))?;
    d.set_item("user_header_conflict", uh.conflicting_blocks > 0)?;
    // P28, P31: "utf-8" | "modified-utf-8" | "utf-8-replaced"; None when there is no text.
    d.set_item("user_header_encoding", encoding.map(|e| e.as_str()))?;
    Ok(())
}

// =================================================================================
// P20: DSS7 regular time-series writer (`dss_core::dss7::write`).
//
// `create_dss7(path)` creates an empty DSS7 file; `write_rts(...)` stores one
// regular-interval series (float32 -> RTS 100, float64 -> RTD 105) with HEC's storage
// flag 0 semantics; `rts_value_times(pathname, start, n)` returns the HEC-second stamps
// the writer assigns, so the Python layer can check an explicit `times=` array.
// =================================================================================

pyo3::import_exception!(ras_commander_fast_dss.exceptions, DssLockedError);

fn write_err(py: Python<'_>, path: &str, e: dss7::write::WriteError) -> PyErr {
    use dss7::write::WriteError as E;
    match e {
        E::Io(io) => BindErr::Os(io, path.to_string()).into_py(),
        E::AlreadyExists(_) => pyo3::exceptions::PyFileExistsError::new_err(ctx(path, &e)),
        E::Dss7(d) => map_dss7(path, d).into_py(),
        E::Ts(t) => map_ts7(path, t).into_py(),
        E::Lock(l) => map_lock(path, l).into_py(),
        E::Locked(ref r) => {
            // HEC programs release their locks late (Monolith about 6 s after done()).
            let err = DssLockedError::new_err(ctx(
                path,
                format!(
                    "{e}; close the HEC program that has the file open, or wait a few \
                     seconds and retry (Monolith releases a file about 6 s after done())"
                ),
            ));
            let v = err.value(py);
            let reasons: Vec<&str> = r.reasons.iter().map(|x| x.as_str()).collect();
            if let Ok(t) = pyo3::types::PyTuple::new(py, reasons) {
                let _ = v.setattr("reasons", t);
            }
            if let Ok(t) = pyo3::types::PyTuple::new(py, r.holder_pids.clone()) {
                let _ = v.setattr("holder_pids", t);
            }
            let _ = v.setattr("path", path);
            let _ = v.setattr("readers_undetectable", r.readers_undetectable);
            err
        }
        E::BadPathname(_) | E::BadInput(_) => PyValueError::new_err(ctx(path, &e)),
        E::Unsupported(_) => DssUnsupportedRecordError::new_err(ctx(path, &e)),
        E::Corrupt(_) => DssFormatError::new_err(ctx(path, &e)),
        E::InjectedFault { .. } | E::Internal(_) => DssError::new_err(ctx(path, &e)),
    }
}

/// Copy every readable record that the documented discovery procedure reaches in the
/// DSS7 file `src` into a new file `dest` (`src` is only read; an existing `dest` raises
/// `FileExistsError`). A bin block whose declared span does not fit the file is reported
/// as unclassified in `not_copied`, not salvaged, so check `complete`. Returns the report
/// as a dict.
#[pyfunction]
fn repair_dss7(py: Python<'_>, src: &str, dest: &str) -> PyResult<PyObject> {
    let (s, d) = (src.to_string(), dest.to_string());
    let r = py
        .allow_threads(move || {
            dss7::write::repair_dss7(std::path::Path::new(&s), std::path::Path::new(&d))
        })
        .map_err(|e| {
            // Name the file the error is about: a missing source is not the destination.
            let ctx = if std::path::Path::new(src).exists() {
                dest
            } else {
                src
            };
            write_err(py, ctx, e)
        })?;
    let out = PyDict::new(py);
    out.set_item("records_copied", r.records_copied)?;
    out.set_item("hidden_records_copied", r.hidden_records_copied)?;
    let nc = PyList::empty(py);
    for (p, why) in &r.not_copied {
        nc.append((String::from_utf8_lossy(p).into_owned(), why.clone()))?;
    }
    out.set_item("not_copied", nc)?;
    out.set_item("aliases_skipped", r.aliases_skipped)?;
    out.set_item("duplicates_skipped", r.duplicates_skipped)?;
    let fb = PyList::empty(py);
    for (p, info, why) in &r.fallback_copies {
        fb.append((String::from_utf8_lossy(p).into_owned(), *info, why.clone()))?;
    }
    out.set_item("fallback_copies", fb)?;
    let vc = PyList::empty(py);
    for (a, b, why) in &r.variants_copied {
        vc.append((
            String::from_utf8_lossy(a).into_owned(),
            String::from_utf8_lossy(b).into_owned(),
            why.clone(),
        ))?;
    }
    out.set_item("variants_copied", vc)?;
    let lg = PyList::empty(py);
    for c in &r.ledger {
        use dss7::write::CandidateOutcome as O;
        let (outcome, detail): (&str, PyObject) = match &c.outcome {
            O::Copied => ("copied", py.None()),
            O::EqualToCopied { copied_info } => (
                "equal_to_copied",
                copied_info.into_pyobject(py)?.into_any().unbind(),
            ),
            O::UnreadableCoveredBy { copied_info } => (
                "unreadable_covered",
                copied_info.into_pyobject(py)?.into_any().unbind(),
            ),
            O::NotCopied(why) => ("not_copied", why.into_pyobject(py)?.into_any().unbind()),
        };
        lg.append((
            String::from_utf8_lossy(&c.pathname).into_owned(),
            c.info_address,
            c.alias,
            outcome,
            detail,
        ))?;
    }
    out.set_item("ledger", lg)?;
    out.set_item("src_listed_records", r.src_listed_records)?;
    out.set_item("src_interrupted_write", r.src_interrupted_write)?;
    out.set_item("complete", r.complete())?;
    Ok(out.into_any().unbind())
}

/// Create an empty DSS7 file (refuses an existing path with `FileExistsError`).
#[pyfunction]
fn create_dss7(py: Python<'_>, path: &str) -> PyResult<()> {
    let p = path.to_string();
    py.allow_threads(move || dss7::write::create_dss7(std::path::Path::new(&p)))
        .map_err(|e| write_err(py, path, e))
}

/// Store one regular-interval series; returns the write report as a dict.
#[pyfunction]
#[pyo3(signature = (path, pathname, start, values, units, data_type, quality=None,
                    time_zone=None, precision=-1, allow_undetectable_readers=true,
                    sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_rts<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    start: i64,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    time_zone: Option<&str>,
    precision: i32,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    enum Owned {
        F32(Vec<f32>),
        F64(Vec<f64>),
    }
    let owned = if let Ok(a) = values.extract::<numpy::PyReadonlyArray1<'py, f32>>() {
        Owned::F32(a.as_array().to_vec())
    } else if let Ok(a) = values.extract::<numpy::PyReadonlyArray1<'py, f64>>() {
        Owned::F64(a.as_array().to_vec())
    } else {
        return Err(PyValueError::new_err(
            "values must be a 1-D float32 or float64 numpy array",
        ));
    };
    let q: Option<Vec<i32>> = quality.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let result = py.allow_threads(move || {
        let vals = match &owned {
            Owned::F32(v) => dss7::write::RtsValues::F32(v),
            Owned::F64(v) => dss7::write::RtsValues::F64(v),
        };
        let mut input = dss7::write::RtsInput::new(&pn, start, vals, &u, &dt);
        input.quality = q.as_deref();
        input.time_zone = tz.as_deref();
        input.precision = precision;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss7::write::write_rts(std::path::Path::new(&p), &input, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    let d = PyDict::new(py);
    let blocks = PyList::empty(py);
    for b in &r.blocks {
        let x = PyDict::new(py);
        x.set_item("pathname", lossy(&b.pathname))?;
        x.set_item("outcome", b.outcome.as_str())?;
        x.set_item("record_type", b.record_type)?;
        x.set_item("number_data", b.number_data)?;
        x.set_item("logical_number", b.logical_number)?;
        blocks.append(x)?;
    }
    d.set_item("blocks", blocks)?;
    d.set_item("readers_undetected", r.readers_undetected)?;
    d.set_item("lock_method", r.lock_method)?;
    d.set_item(
        "reconciled_interrupted_write",
        r.reconciled_interrupted_write,
    )?;
    Ok(d)
}

/// HEC-second stamps of `n` values starting at `start` for the pathname's E part, as the
/// writer assigns them (`incrementTime`); validates the pathname and start like a write.
#[pyfunction]
fn rts_value_times<'py>(
    py: Python<'py>,
    pathname: &str,
    start: i64,
    n: usize,
) -> PyResult<Bound<'py, numpy::PyArray1<i64>>> {
    let v = dss7::write::value_times(pathname.as_bytes(), start, n)
        .map_err(|e| write_err(py, pathname, e))?;
    Ok(v.into_pyarray(py))
}

// =================================================================================
// P21: DSS7 irregular time-series writer (`dss_core::dss7::write::write_its`).
//
// `write_its(...)` stores one irregular-interval series (float32 -> ITS 110, float64 ->
// ITD 115) with heclib's storage flag 1 ("replace") semantics. `times` are whole HEC
// seconds (int64), strictly ascending.
// =================================================================================

/// Store one irregular-interval series; returns the write report as a dict.
#[pyfunction]
#[pyo3(signature = (path, pathname, times, values, units, data_type, quality=None,
                    time_zone=None, precision=-1, time_granularity=None,
                    allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_its<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    times: numpy::PyReadonlyArray1<'py, i64>,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    time_zone: Option<&str>,
    precision: i32,
    time_granularity: Option<i64>,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    enum Owned {
        F32(Vec<f32>),
        F64(Vec<f64>),
    }
    let owned = if let Ok(a) = values.extract::<numpy::PyReadonlyArray1<'py, f32>>() {
        Owned::F32(a.as_array().to_vec())
    } else if let Ok(a) = values.extract::<numpy::PyReadonlyArray1<'py, f64>>() {
        Owned::F64(a.as_array().to_vec())
    } else {
        return Err(PyValueError::new_err(
            "values must be a 1-D float32 or float64 numpy array",
        ));
    };
    let t: Vec<i64> = times.as_array().to_vec();
    let q: Option<Vec<i32>> = quality.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let result = py.allow_threads(move || {
        let vals = match &owned {
            Owned::F32(v) => dss7::write::RtsValues::F32(v),
            Owned::F64(v) => dss7::write::RtsValues::F64(v),
        };
        let mut input = dss7::write::ItsInput::new(&pn, &t, vals, &u, &dt);
        input.quality = q.as_deref();
        input.time_zone = tz.as_deref();
        input.precision = precision;
        input.time_granularity = time_granularity;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss7::write::write_its(std::path::Path::new(&p), &input, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    let d = PyDict::new(py);
    let blocks = PyList::empty(py);
    for b in &r.blocks {
        let x = PyDict::new(py);
        x.set_item("pathname", lossy(&b.pathname))?;
        x.set_item("outcome", b.outcome.as_str())?;
        x.set_item("record_type", b.record_type)?;
        x.set_item("number_data", b.number_data)?;
        x.set_item("logical_number", b.logical_number)?;
        blocks.append(x)?;
    }
    d.set_item("blocks", blocks)?;
    d.set_item("readers_undetected", r.readers_undetected)?;
    d.set_item("lock_method", r.lock_method)?;
    d.set_item(
        "reconciled_interrupted_write",
        r.reconciled_interrupted_write,
    )?;
    Ok(d)
}

// =================================================================================
// P22: DSS6 time-series writer (`dss_core::dss6::write`).
//
// `create_dss6(path)` creates an empty DSS6 file (Monolith 3.3.27 layout, `6-YO`);
// `write_rts6(...)` stores a regular-interval series and `write_its6(...)` an
// irregular one (mode "replace" or "merge"); `rts6_value_times(pathname, start, n)`
// returns the HEC-second stamps the DSS6 writer assigns. Errors map as for P20.
// =================================================================================

type Owned6 = (Option<Vec<f32>>, Option<Vec<f64>>);

fn values_owned6(values: &Bound<'_, PyAny>) -> PyResult<Owned6> {
    if let Ok(a) = values.extract::<numpy::PyReadonlyArray1<'_, f32>>() {
        Ok((Some(a.as_array().to_vec()), None))
    } else if let Ok(a) = values.extract::<numpy::PyReadonlyArray1<'_, f64>>() {
        Ok((None, Some(a.as_array().to_vec())))
    } else {
        Err(PyValueError::new_err(
            "values must be a 1-D float32 or float64 numpy array",
        ))
    }
}

fn values_ref6(o: &Owned6) -> dss7::write::RtsValues<'_> {
    match o {
        (Some(v), _) => dss7::write::RtsValues::F32(v),
        (None, Some(v)) => dss7::write::RtsValues::F64(v),
        (None, None) => dss7::write::RtsValues::F64(&[]),
    }
}

fn report_dict6<'py>(
    py: Python<'py>,
    r: &dss7::write::WriteReport,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    let blocks = PyList::empty(py);
    for b in &r.blocks {
        let x = PyDict::new(py);
        x.set_item("pathname", lossy(&b.pathname))?;
        x.set_item("outcome", b.outcome.as_str())?;
        x.set_item("record_type", b.record_type)?;
        x.set_item("number_data", b.number_data)?;
        x.set_item("logical_number", b.logical_number)?;
        blocks.append(x)?;
    }
    d.set_item("blocks", blocks)?;
    d.set_item("readers_undetected", r.readers_undetected)?;
    d.set_item("lock_method", r.lock_method)?;
    d.set_item(
        "reconciled_interrupted_write",
        r.reconciled_interrupted_write,
    )?;
    d.set_item("version", 6)?;
    Ok(d)
}

/// Create an empty DSS6 file (refuses an existing path with `FileExistsError`).
#[pyfunction]
fn create_dss6(py: Python<'_>, path: &str) -> PyResult<()> {
    let p = path.to_string();
    py.allow_threads(move || dss_core::dss6::write::create_dss6(std::path::Path::new(&p)))
        .map_err(|e| write_err(py, path, e))
}

/// Store one regular-interval series in a DSS6 file; returns the write report as a dict.
#[pyfunction]
#[pyo3(signature = (path, pathname, start, values, units, data_type, quality=None,
                    time_zone=None, precision=-1, allow_undetectable_readers=true,
                    sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_rts6<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    start: i64,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    time_zone: Option<&str>,
    precision: i32,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let owned = values_owned6(values)?;
    let q: Option<Vec<i32>> = quality.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let result = py.allow_threads(move || {
        let mut input = dss7::write::RtsInput::new(&pn, start, values_ref6(&owned), &u, &dt);
        input.quality = q.as_deref();
        input.time_zone = tz.as_deref();
        input.precision = precision;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss_core::dss6::write::write_rts6(std::path::Path::new(&p), &input, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    report_dict6(py, &r)
}

/// HEC-second stamps of `n` regular values starting at `start`, as the DSS6 writer
/// assigns them (`INCTIM` and the offset); validates the pathname and start like a write.
#[pyfunction]
fn rts6_value_times<'py>(
    py: Python<'py>,
    pathname: &str,
    start: i64,
    n: usize,
) -> PyResult<Bound<'py, numpy::PyArray1<i64>>> {
    let v = dss_core::dss6::write::value_times6(pathname.as_bytes(), start, n)
        .map_err(|e| write_err(py, pathname, e))?;
    Ok(v.into_pyarray(py))
}

/// Store one irregular-interval series in a DSS6 file; `times` are HEC seconds (whole
/// minutes); `mode` is "replace" (zsitsi6 INFLAG 1) or "merge" (INFLAG 0).
#[pyfunction]
#[pyo3(signature = (path, pathname, times, values, units, data_type, quality=None,
                    mode="replace", allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_its6<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    times: numpy::PyReadonlyArray1<'py, i64>,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    mode: &str,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let mode = match mode {
        "replace" => dss_core::dss6::write::ItsMode::Replace,
        "merge" => dss_core::dss6::write::ItsMode::Merge,
        other => {
            return Err(PyValueError::new_err(format!(
                "mode must be \"replace\" or \"merge\", not {other:?}"
            )))
        }
    };
    let owned = values_owned6(values)?;
    let t: Vec<i64> = times.as_array().to_vec();
    let q: Option<Vec<i32>> = quality.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let result = py.allow_threads(move || {
        let input = dss_core::dss6::write::ItsInput {
            pathname: &pn,
            times: &t,
            values: values_ref6(&owned),
            quality: q.as_deref(),
            units: &u,
            data_type: &dt,
            mode,
        };
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss_core::dss6::write::write_its6(std::path::Path::new(&p), &input, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    report_dict6(py, &r)
}

// =================================================================================
// P23: paired-data writer (`dss_core::dss7::write::write_pd`, `dss6::write::write_pd6`).
//
// `write_pd(...)` stores one full paired-data record: float32 ordinates and values make
// PD (200), float64 PDD (205). `values` is curve-major (`number_curves` curves of
// `len(ordinates)` values). `version` selects the DSS7 or DSS6 writer; the caller passes
// the file's version.
// =================================================================================

/// Store one paired-data record; returns the write report as a dict.
#[pyfunction]
#[pyo3(signature = (path, pathname, ordinates, values, number_curves, units_independent,
                    type_independent, units_dependent, type_dependent, labels=None,
                    independent_is_x_axis=1, x_precision=-1, y_precision=-1, time_zone=None,
                    version=7, allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_pd<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    ordinates: &Bound<'py, PyAny>,
    values: &Bound<'py, PyAny>,
    number_curves: usize,
    units_independent: &str,
    type_independent: &str,
    units_dependent: &str,
    type_dependent: &str,
    labels: Option<Vec<String>>,
    independent_is_x_axis: i32,
    x_precision: i32,
    y_precision: i32,
    time_zone: Option<&str>,
    version: i32,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let o = values_owned6(ordinates)?;
    let v = values_owned6(values)?;
    if o.0.is_some() != v.0.is_some() {
        return Err(PyValueError::new_err(
            "ordinates and values must have the same dtype (both float32 or both float64)",
        ));
    }
    if version != 6 && version != 7 {
        return Err(PyValueError::new_err(format!(
            "version must be 6 or 7, not {version}"
        )));
    }
    let p = path.to_string();
    let pn = pathname.as_bytes().to_vec();
    let strings: Vec<Vec<u8>> = [
        units_independent,
        type_independent,
        units_dependent,
        type_dependent,
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    let ls: Option<Vec<Vec<u8>>> = labels.map(|l| l.into_iter().map(|x| x.into_bytes()).collect());
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let result = py.allow_threads(move || {
        let data = match (&o, &v) {
            ((Some(a), _), (Some(b), _)) => dss7::write::PdArrays::F32 {
                ordinates: a,
                values: b,
            },
            ((_, Some(a)), (_, Some(b))) => dss7::write::PdArrays::F64 {
                ordinates: a,
                values: b,
            },
            _ => dss7::write::PdArrays::F64 {
                ordinates: &[],
                values: &[],
            },
        };
        let mut input = dss7::write::PdInput::new(
            &pn,
            data,
            number_curves,
            &strings[0],
            &strings[1],
            &strings[2],
            &strings[3],
        );
        input.labels = ls.as_deref();
        input.independent_is_x_axis = independent_is_x_axis;
        input.x_precision = x_precision;
        input.y_precision = y_precision;
        input.time_zone = tz.as_deref();
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        if version == 6 {
            dss_core::dss6::write::write_pd6(std::path::Path::new(&p), &input, &opts)
        } else {
            dss7::write::write_pd(std::path::Path::new(&p), &input, &opts)
        }
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    let d = report_dict6(py, &r)?;
    d.set_item("version", version)?;
    Ok(d)
}

// =================================================================================
// P24: grid writer (`dss_core::dss7::write::write_grid`, `dss6::write::write_grid6`).
//
// `write_grid(...)` stores one grid record (float32 cells, x fastest, southern row
// first). The Python layer (`ras_commander_fast_dss.write_grid`) builds the pathname, maps NaN
// and checks the arguments; this binding passes them through. `validate_only` runs
// every check and returns the plan without opening the file.
// =================================================================================

/// Albers parameters from Python: datum, units, the two standard parallels, central
/// meridian, latitude of origin, false easting and false northing.
type AlbersTuple = (i32, String, f32, f32, f32, f32, f32, f32);

/// Store one grid; returns the write report and the stored statistics as a dict.
#[pyfunction]
#[pyo3(signature = (path, pathname, data, nx, ny, grid_type, time_stamped,
                    lower_left_cell_x, lower_left_cell_y, cell_size, x0, y0, units,
                    data_type, compression, scale_factor=None, base=None, data_source="",
                    albers=None, srs_name="", srs_definition="", srs_definition_type=0,
                    null_value=-f32::MAX, time_zone_id="UTC", time_zone_raw_offset=0,
                    is_interval=true, version=7, allow_undetectable_readers=true,
                    sync=true, validate_only=false))]
#[allow(clippy::too_many_arguments)]
fn write_grid<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    data: numpy::PyReadonlyArray1<'py, f32>,
    nx: usize,
    ny: usize,
    grid_type: &str,
    time_stamped: bool,
    lower_left_cell_x: i32,
    lower_left_cell_y: i32,
    cell_size: f32,
    x0: f32,
    y0: f32,
    units: &str,
    data_type: i32,
    compression: &str,
    scale_factor: Option<f32>,
    base: Option<f32>,
    data_source: &str,
    albers: Option<AlbersTuple>,
    srs_name: &str,
    srs_definition: &str,
    srs_definition_type: i32,
    null_value: f32,
    time_zone_id: &str,
    time_zone_raw_offset: i32,
    is_interval: bool,
    version: u8,
    allow_undetectable_readers: bool,
    sync: bool,
    validate_only: bool,
) -> PyResult<Bound<'py, PyDict>> {
    use dss7::write::{
        AlbersParameters, GridCompression, GridInput, GridProjection, SpecifiedReference,
    };
    let projection = match grid_type {
        "hrap" => GridProjection::Hrap {
            data_source: data_source.as_bytes().to_vec(),
        },
        "albers" => {
            let a = albers.ok_or_else(|| PyValueError::new_err("albers parameters missing"))?;
            GridProjection::Albers(AlbersParameters {
                datum: a.0,
                units: a.1.into_bytes(),
                first_standard_parallel: a.2,
                second_standard_parallel: a.3,
                central_meridian: a.4,
                latitude_of_projection_origin: a.5,
                false_easting: a.6,
                false_northing: a.7,
            })
        }
        "specified" => GridProjection::Specified(SpecifiedReference {
            srs_name: srs_name.as_bytes().to_vec(),
            srs_definition: srs_definition.as_bytes().to_vec(),
            srs_definition_type,
            null_value,
            time_zone_id: time_zone_id.as_bytes().to_vec(),
            time_zone_raw_offset,
            is_interval,
        }),
        other => {
            return Err(PyValueError::new_err(format!(
                "grid_type must be \"hrap\", \"albers\" or \"specified\", not {other:?}"
            )))
        }
    };
    let compression = match compression {
        "zlib" => GridCompression::Zlib,
        "none" => GridCompression::Uncompressed,
        "precip_2_byte" => GridCompression::Precip2Byte { scale_factor, base },
        other => {
            return Err(PyValueError::new_err(format!(
                "compression must be \"zlib\", \"none\" or \"precip_2_byte\", not {other:?}"
            )))
        }
    };
    if version != 6 && version != 7 {
        return Err(PyValueError::new_err("version must be 6 or 7"));
    }
    let cells: Vec<f32> = data.as_array().to_vec();
    let (p, pn, u) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
    );
    let result = py.allow_threads(move || {
        let input = GridInput {
            pathname: &pn,
            projection,
            time_stamped,
            data: &cells,
            nx,
            ny,
            lower_left_cell_x,
            lower_left_cell_y,
            cell_size,
            x_coord_of_grid_cell_zero: x0,
            y_coord_of_grid_cell_zero: y0,
            units: &u,
            data_type,
            compression,
        };
        let plan = if version == 6 {
            dss_core::dss6::write::validate_grid6_input(&input)?
        } else {
            dss7::write::validate_grid_input(&input)?
        };
        if validate_only {
            return Ok((plan, None));
        }
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        let path = std::path::Path::new(&p);
        let r = if version == 6 {
            dss_core::dss6::write::write_grid6(path, &input, &opts)?
        } else {
            dss7::write::write_grid(path, &input, &opts)?
        };
        Ok((plan, Some(r)))
    });
    let (plan, report) = result.map_err(|e| write_err(py, path, e))?;
    let d = PyDict::new(py);
    d.set_item("pathname", lossy(&plan.pathname))?;
    d.set_item("record_type", plan.record_type)?;
    let st = PyDict::new(py);
    st.set_item("min", plan.stats.min)?;
    st.set_item("max", plan.stats.max)?;
    st.set_item("mean", plan.stats.mean)?;
    st.set_item("range_limits", plan.stats.range_limits.clone())?;
    st.set_item("range_counts", plan.stats.range_counts.clone())?;
    d.set_item("statistics", st)?;
    if let Some(r) = report {
        let b = r.blocks.first();
        d.set_item("outcome", b.map_or("unchanged", |b| b.outcome.as_str()))?;
        d.set_item("readers_undetected", r.readers_undetected)?;
        d.set_item("lock_method", r.lock_method)?;
        d.set_item(
            "reconciled_interrupted_write",
            r.reconciled_interrupted_write,
        )?;
    }
    Ok(d)
}

// =================================================================================
// P39: profile and location writers (`dss_core::dss7::write::{write_profile,
// write_location}`, `dss6::write::write_location6`). The Python layer
// (`_write_profile_location.py`) checks shapes and maps NaN.
// =================================================================================

/// Store one regular-interval profile (DSS7); returns the write report as a dict.
#[pyfunction]
#[pyo3(signature = (path, pathname, start, depths, values, depth_units, value_units,
                    data_type, quality=None, time_zone=None, precision=-1,
                    allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_profile<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    start: i64,
    depths: &Bound<'py, PyAny>,
    values: &Bound<'py, PyAny>,
    depth_units: &str,
    value_units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    time_zone: Option<&str>,
    precision: i32,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let d = values_owned6(depths)?;
    let v = values_owned6(values)?;
    if d.0.is_some() != v.0.is_some() {
        return Err(PyValueError::new_err(
            "depths and values must have the same dtype (both float32 or both float64)",
        ));
    }
    let q: Option<Vec<i32>> = match quality {
        Some(a) => Some(a.as_slice()?.to_vec()),
        None => None,
    };
    let p = path.to_string();
    let pn = pathname.as_bytes().to_vec();
    let du = depth_units.as_bytes().to_vec();
    let vu = value_units.as_bytes().to_vec();
    let ty = data_type.as_bytes().to_vec();
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let result = py.allow_threads(move || {
        let data = match (&d, &v) {
            ((Some(a), _), (Some(b), _)) => dss7::write::ProfileArrays::F32 {
                depths: a,
                values: b,
            },
            ((_, Some(a)), (_, Some(b))) => dss7::write::ProfileArrays::F64 {
                depths: a,
                values: b,
            },
            _ => dss7::write::ProfileArrays::F64 {
                depths: &[],
                values: &[],
            },
        };
        let mut input = dss7::write::ProfileInput::new(&pn, start, data, &du, &vu, &ty);
        input.quality = q.as_deref();
        input.time_zone = tz.as_deref();
        input.precision = precision;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss7::write::write_profile(std::path::Path::new(&p), &input, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    report_dict6(py, &r)
}

/// Store location data (DSS7 record, or DSS6 time-series headers).
#[pyfunction]
#[pyo3(signature = (path, pathname, x=None, y=None, z=None, coordinate_system=None,
                    coordinate_id=None, horizontal_units=None, horizontal_datum=None,
                    vertical_units=None, vertical_datum=None, time_zone=None,
                    supplemental=None, keep_existing=true, version=7,
                    allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_location<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    coordinate_system: Option<i32>,
    coordinate_id: Option<i32>,
    horizontal_units: Option<i32>,
    horizontal_datum: Option<i32>,
    vertical_units: Option<i32>,
    vertical_datum: Option<i32>,
    time_zone: Option<Vec<u8>>,
    supplemental: Option<Vec<u8>>,
    keep_existing: bool,
    version: i32,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    if version != 6 && version != 7 {
        return Err(PyValueError::new_err(format!(
            "version must be 6 or 7, not {version}"
        )));
    }
    let p = path.to_string();
    let pn = pathname.as_bytes().to_vec();
    let result = py.allow_threads(move || {
        let input = dss7::write::LocationInput {
            pathname: &pn,
            x,
            y,
            z,
            coordinate_system,
            coordinate_id,
            horizontal_units,
            horizontal_datum,
            vertical_units,
            vertical_datum,
            time_zone: time_zone.as_deref(),
            supplemental: supplemental.as_deref(),
            keep_existing,
        };
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        if version == 6 {
            dss_core::dss6::write::write_location6(std::path::Path::new(&p), &input, &opts)
        } else {
            dss7::write::write_location(std::path::Path::new(&p), &input, &opts)
        }
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    let d = report_dict6(py, &r)?;
    d.set_item("version", version)?;
    Ok(d)
}

// =================================================================================
// P38: time-series variants (`dss_core::dss7::write::write_rts_variants`,
// `write_its_variants`, `write_pattern`, `dss6::write::write_pattern6`). Quality and
// integer notes are flat int32 arrays of `n × element size`; character notes one bytes
// object per value.
// =================================================================================

fn report_dict_p38<'py>(
    py: Python<'py>,
    r: &dss7::write::WriteReport,
    version: u8,
) -> PyResult<Bound<'py, PyDict>> {
    let d = report_dict6(py, r)?;
    d.set_item("version", version)?;
    Ok(d)
}

fn variants_of<'a>(
    q: &'a Option<Vec<i32>>,
    qes: usize,
    inotes: &'a Option<Vec<i32>>,
    nes: usize,
    refs: &'a Option<Vec<&'a [u8]>>,
) -> PyResult<dss7::write::TsVariants<'a>> {
    if inotes.is_some() && refs.is_some() {
        return Err(PyValueError::new_err(
            "give integer notes or character notes, not both (they share a storage area)",
        ));
    }
    Ok(dss7::write::TsVariants {
        quality_words: q.as_deref().map(|w| (w, qes)),
        notes: if let Some(c) = refs {
            Some(dss7::write::TsNotes::Char(c))
        } else {
            inotes.as_deref().map(|w| dss7::write::TsNotes::Int {
                words: w,
                element_size: nes,
            })
        },
    })
}

/// [`write_rts`] with multi-word quality or notes.
#[pyfunction]
#[pyo3(signature = (path, pathname, start, values, units, data_type, quality=None,
                    quality_element_size=1, inotes=None, inote_element_size=0, cnotes=None,
                    time_zone=None, precision=-1, allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_rts_variants<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    start: i64,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    quality_element_size: usize,
    inotes: Option<numpy::PyReadonlyArray1<'py, i32>>,
    inote_element_size: usize,
    cnotes: Option<Vec<Vec<u8>>>,
    time_zone: Option<&str>,
    precision: i32,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let owned = values_owned6(values)?;
    let q: Option<Vec<i32>> = quality.map(|a| a.as_array().to_vec());
    let n: Option<Vec<i32>> = inotes.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let refs: Option<Vec<&[u8]>> = cnotes
        .as_ref()
        .map(|c| c.iter().map(Vec::as_slice).collect());
    let v = variants_of(&q, quality_element_size, &n, inote_element_size, &refs)?;
    let result = py.allow_threads(|| {
        let mut input = dss7::write::RtsInput::new(&pn, start, values_ref6(&owned), &u, &dt);
        input.time_zone = tz.as_deref();
        input.precision = precision;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss7::write::write_rts_variants(std::path::Path::new(&p), &input, &v, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    report_dict_p38(py, &r, 7)
}

/// [`write_its`] with multi-word quality or notes.
#[pyfunction]
#[pyo3(signature = (path, pathname, times, values, units, data_type, quality=None,
                    quality_element_size=1, inotes=None, inote_element_size=0, cnotes=None,
                    time_zone=None, precision=-1, time_granularity=None,
                    allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_its_variants<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    times: numpy::PyReadonlyArray1<'py, i64>,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    quality: Option<numpy::PyReadonlyArray1<'py, i32>>,
    quality_element_size: usize,
    inotes: Option<numpy::PyReadonlyArray1<'py, i32>>,
    inote_element_size: usize,
    cnotes: Option<Vec<Vec<u8>>>,
    time_zone: Option<&str>,
    precision: i32,
    time_granularity: Option<i64>,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let owned = values_owned6(values)?;
    let t: Vec<i64> = times.as_array().to_vec();
    let q: Option<Vec<i32>> = quality.map(|a| a.as_array().to_vec());
    let n: Option<Vec<i32>> = inotes.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let refs: Option<Vec<&[u8]>> = cnotes
        .as_ref()
        .map(|c| c.iter().map(Vec::as_slice).collect());
    let v = variants_of(&q, quality_element_size, &n, inote_element_size, &refs)?;
    let result = py.allow_threads(|| {
        let mut input = dss7::write::ItsInput::new(&pn, &t, values_ref6(&owned), &u, &dt);
        input.time_zone = tz.as_deref();
        input.precision = precision;
        input.time_granularity = time_granularity;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        dss7::write::write_its_variants(std::path::Path::new(&p), &input, &v, &opts)
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    report_dict_p38(py, &r, 7)
}

/// Store one pattern record; `version` selects the DSS7 or DSS6 writer.
#[pyfunction]
#[pyo3(signature = (path, pathname, values, units, data_type, times=None, time_zone=None,
                    precision=-1, time_granularity=None, version=7,
                    allow_undetectable_readers=true, sync=true))]
#[allow(clippy::too_many_arguments)]
fn write_pattern<'py>(
    py: Python<'py>,
    path: &str,
    pathname: &str,
    values: &Bound<'py, PyAny>,
    units: &str,
    data_type: &str,
    times: Option<numpy::PyReadonlyArray1<'py, i64>>,
    time_zone: Option<&str>,
    precision: i32,
    time_granularity: Option<i64>,
    version: u8,
    allow_undetectable_readers: bool,
    sync: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let owned = values_owned6(values)?;
    let t: Option<Vec<i64>> = times.map(|a| a.as_array().to_vec());
    let (p, pn, u, dt) = (
        path.to_string(),
        pathname.as_bytes().to_vec(),
        units.as_bytes().to_vec(),
        data_type.as_bytes().to_vec(),
    );
    let tz: Option<Vec<u8>> = time_zone.map(|t| t.as_bytes().to_vec());
    let result = py.allow_threads(move || {
        let mut input = dss7::write::PatternInput::new(&pn, values_ref6(&owned), &u, &dt);
        input.times = t.as_deref();
        input.time_zone = tz.as_deref();
        input.precision = precision;
        input.time_granularity = time_granularity;
        let mut opts = dss7::write::WriteOptions::default();
        opts.lock_policy.allow_undetectable_readers = allow_undetectable_readers;
        opts.sync = sync;
        let f = std::path::Path::new(&p);
        if version == 6 {
            dss_core::dss6::write::write_pattern6(f, &input, &opts)
        } else {
            dss7::write::write_pattern(f, &input, &opts)
        }
    });
    let r = result.map_err(|e| write_err(py, path, e))?;
    report_dict_p38(py, &r, version)
}

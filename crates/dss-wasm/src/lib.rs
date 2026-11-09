//! Read-only WebAssembly bindings. Inputs are in-memory byte buffers; there is no
//! file access, locking, threading or writing in WASM.
//!
//! JavaScript API:
//!
//! - `dssVersion(head)`: DSS major version (6 or 7) from the file header.
//! - `fileSignature(bytes)`: header summary object.
//! - `isHecMissing(value)`: HEC's missing-marker test (-901, -902, -FLT_MAX).
//! - `new DssFile(bytes)`: a whole file held in WASM memory, with `catalog()`,
//!   `readRecord()`, `readDataset()`, `readPairedData()` and `readGrid()`.
//!
//! P40 adds `readProfile()`, `readProfileRecord()`, `readLocation()`,
//! `readAssociatedLocation()`, `entryProvenance()`, `readRecordAt()` and the explicit
//! recovery-mode constructor `DssFile.openRecovery(bytes)`.
//!
//! Values keep their stored width and bits; HEC missing markers are not converted
//! (only the Python layer maps them to NaN). Errors are thrown as JavaScript `Error`
//! objects whose `name` is the Python exception class (`DssFormatError`, ...) and whose
//! `kind` property is the typed-error kind (`format`, `not_found`, `unsupported`,
//! `dss`, `invalid_argument`, `index`). All format logic lives in `dss-core`; the target-
//! independent dispatch is in [`reader`].

// The `#[wasm_bindgen]` expansion contains `unsafe` blocks, so this file cannot forbid
// unsafe code; it writes none itself. `reader` (all the logic) forbids it.
#![deny(unsafe_code)]

pub mod reader;

use dss_core::{GridReadOptions, GridType, TsKind, TsValues};
use js_sys::{Array, Float32Array, Float64Array, Int32Array, Object, Reflect, Uint8Array};
use reader::{
    CatalogData, DssBuffer, EntryProvenance, GridData, LocationData, PdData, ProfileData,
    ReadError, TsData,
};
use wasm_bindgen::prelude::*;

// ---------------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------------

/// Largest integer a JavaScript number holds exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// Largest `maxCells` accepted: 2^30 cells (4 GiB of float32), the whole wasm32
/// address space; larger values could never be allocated.
pub const MAX_CELLS_ARG: u64 = 1 << 30;

/// Turn a read error into a thrown JavaScript `Error` with `name` and `kind`.
fn js_error(e: ReadError) -> JsValue {
    let err = js_sys::Error::new(&e.message);
    err.set_name(e.kind.error_name());
    let _ = Reflect::set(&err, &"kind".into(), &e.kind.as_str().into());
    if let Some(u) = &e.unavailable {
        let _ = Reflect::set(&err, &"problem".into(), &u.problem.into());
        let _ = Reflect::set(&err, &"index".into(), &(u.index as f64).into());
        let _ = Reflect::set(&err, &"infoAddress".into(), &safe_u64(u.info_address));
    }
    err.into()
}

/// A `u64` as a JavaScript number, or `null` beyond the exact range (hostile addresses
/// must not hide the rest of a forensic result).
fn safe_u64(v: u64) -> JsValue {
    if v <= MAX_SAFE_INTEGER as u64 {
        JsValue::from_f64(v as f64)
    } else {
        JsValue::NULL
    }
}

fn invalid(message: impl Into<String>) -> JsValue {
    js_error(ReadError::new(reader::ErrorKind::InvalidArgument, message))
}

/// Set a property on a plain object (cannot fail for a fresh `Object`).
fn set(o: &Object, key: &str, value: impl Into<JsValue>) {
    let _ = Reflect::set(o, &key.into(), &value.into());
}

fn opt_num<T: Into<f64>>(v: Option<T>) -> JsValue {
    v.map_or(JsValue::NULL, |x| JsValue::from_f64(x.into()))
}

/// An `i64` as a JavaScript number; refuses values a double cannot hold exactly.
fn i64_num(v: i64, what: &str) -> Result<f64, JsValue> {
    if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&v) {
        Ok(v as f64)
    } else {
        Err(js_error(ReadError::new(
            reader::ErrorKind::Dss,
            format!("{what} {v} is outside the exact JavaScript number range"),
        )))
    }
}

fn opt_i64(v: Option<i64>, what: &str) -> Result<JsValue, JsValue> {
    match v {
        Some(x) => Ok(JsValue::from_f64(i64_num(x, what)?)),
        None => Ok(JsValue::NULL),
    }
}

/// An `i64` column where one unrepresentable entry must not deny the whole result
/// (catalog `rawStatus`, `lastWriteMs`): such an entry becomes NaN.
fn i64_column(v: &[i64]) -> Float64Array {
    let out: Vec<f64> = v
        .iter()
        .map(|&x| {
            if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&x) {
                x as f64
            } else {
                f64::NAN
            }
        })
        .collect();
    Float64Array::from(&out[..])
}

fn i64_array(v: &[i64], what: &str) -> Result<Float64Array, JsValue> {
    let mut out = Vec::with_capacity(v.len());
    for &x in v {
        out.push(i64_num(x, what)?);
    }
    Ok(Float64Array::from(&out[..]))
}

fn string_array<S: AsRef<str>>(v: &[S]) -> Array {
    v.iter().map(|s| JsValue::from_str(s.as_ref())).collect()
}

fn opt_string(v: Option<&str>) -> JsValue {
    v.map_or(JsValue::NULL, JsValue::from_str)
}

/// Values as a typed array of their stored width, and that width in bits.
fn values_array(v: &TsValues) -> (JsValue, u8) {
    match v {
        TsValues::F32(x) => (Float32Array::from(&x[..]).into(), 32),
        TsValues::F64(x) => (Float64Array::from(&x[..]).into(), 64),
    }
}

/// A JavaScript number argument as whole seconds.
fn seconds_arg(v: f64, what: &str) -> Result<i64, JsValue> {
    if v.fract() != 0.0 || !v.is_finite() || v.abs() > MAX_SAFE_INTEGER as f64 {
        return Err(invalid(format!("{what} must be a whole number of seconds")));
    }
    Ok(v as i64)
}

fn signature_object(sig: &dss_core::FileSignature) -> Result<Object, JsValue> {
    let o = Object::new();
    set(&o, "version", reader::version_number(sig.version));
    set(&o, "versionString", sig.version_string.as_str());
    set(
        &o,
        "byteOrder",
        match sig.byte_order {
            dss_core::ByteOrder::Little => "little",
            dss_core::ByteOrder::Big => "big",
        },
    );
    set(
        &o,
        "numberRecords",
        i64_num(sig.number_records, "number_records")?,
    );
    set(
        &o,
        "fileSizeBytes",
        i64_num(sig.file_size_bytes(), "file size")?,
    );
    set(&o, "deadWords", i64_num(sig.dead_words, "dead_words")?);
    set(
        &o,
        "numberDeletes",
        opt_i64(sig.number_deletes, "number_deletes")?,
    );
    Ok(o)
}

fn catalog_object(c: CatalogData) -> Result<Object, JsValue> {
    let o = Object::new();
    set(&o, "pathname", string_array(&c.pathname));
    set(&o, "storedPathname", string_array(&c.stored_pathname));
    set(&o, "recordType", Int32Array::from(&c.record_type[..]));
    set(&o, "status", string_array(&c.status));
    // Per-entry fallback: a hostile status or write time must not hide the catalog.
    set(&o, "rawStatus", i64_column(&c.raw_status));
    set(&o, "lastWriteMs", i64_column(&c.last_write_ms));
    let bools = |v: &[bool]| -> Array { v.iter().map(|&b| JsValue::from_bool(b)).collect() };
    set(&o, "duplicate", bools(&c.duplicate));
    set(&o, "recovered", bools(&c.recovered));
    let an = Array::new();
    for a in &c.anomalies {
        let x = Object::new();
        set(&x, "kind", reader::anomaly_name(a.kind));
        // `null` when absent or beyond the exact JavaScript number range.
        let addr = a.address.filter(|&v| v <= MAX_SAFE_INTEGER as u64);
        set(&x, "address", opt_num(addr.map(|v| v as f64)));
        set(&x, "detail", a.detail.as_str());
        an.push(&x);
    }
    set(&o, "anomalies", an);
    Ok(o)
}

fn ts_object(d: TsData) -> Result<Object, JsValue> {
    let ts = d.ts;
    let o = Object::new();
    set(&o, "version", d.version);
    set(&o, "pathname", reader::lossy(&ts.pathname));
    set(&o, "recordType", ts.record_type);
    match ts.kind {
        TsKind::Regular { interval_seconds } => {
            set(&o, "regular", true);
            set(
                &o,
                "intervalSeconds",
                i64_num(interval_seconds, "interval")?,
            );
            set(&o, "irregularBlock", JsValue::NULL);
        }
        TsKind::Irregular { block } => {
            set(&o, "regular", false);
            set(&o, "intervalSeconds", JsValue::NULL);
            set(&o, "irregularBlock", irregular_block_name(block));
        }
    }
    set(&o, "times", i64_array(&ts.times, "time")?);
    let (values, width) = values_array(&ts.values);
    set(&o, "values", values);
    set(&o, "valueBits", width);
    set(
        &o,
        "quality",
        ts.quality
            .map_or(JsValue::NULL, |q| Int32Array::from(&q[..]).into()),
    );
    set(&o, "units", ts.units);
    set(&o, "dataType", ts.data_type);
    set(&o, "timeZone", opt_string(ts.time_zone.as_deref()));
    set(
        &o,
        "timeOffsetSeconds",
        i64_num(d.time_offset_seconds, "time offset")?,
    );
    set(&o, "pattern", d.pattern);
    set(&o, "blocksFound", d.blocks_found as f64);
    set(&o, "blocksMissing", d.blocks_missing as f64);
    set(&o, "warnings", string_array(&d.warnings));
    set(&o, "precision", opt_num(d.precision));
    Ok(o)
}

fn irregular_block_name(b: dss_core::IrregularBlock) -> &'static str {
    use dss_core::IrregularBlock as B;
    match b {
        B::Day => "IR-Day",
        B::Month => "IR-Month",
        B::Year => "IR-Year",
        B::Decade => "IR-Decade",
        B::Century => "IR-Century",
    }
}

fn pd_object(d: PdData) -> Object {
    let pd = d.pd;
    let (independent, dependent) = pd.parameters();
    let o = Object::new();
    set(&o, "version", d.version);
    set(&o, "pathname", reader::lossy(&pd.pathname));
    set(&o, "recordType", pd.record_type);
    set(&o, "numberOrdinates", pd.number_ordinates as f64);
    set(&o, "numberCurves", pd.number_curves as f64);
    let (ordinates, width) = values_array(&pd.ordinates);
    set(&o, "ordinates", ordinates);
    set(&o, "valueBits", width);
    // Curve-major as stored: curve 0 (all ordinates), then curve 1, ...
    set(&o, "values", values_array(&pd.values).0);
    set(&o, "unitsIndependent", pd.units_independent);
    set(&o, "typeIndependent", pd.type_independent);
    set(&o, "unitsDependent", pd.units_dependent);
    set(&o, "typeDependent", pd.type_dependent);
    set(&o, "labels", string_array(&pd.labels));
    set(&o, "independentIsXAxis", pd.independent_is_x_axis);
    set(&o, "xPrecision", pd.x_precision);
    set(&o, "yPrecision", pd.y_precision);
    set(&o, "timeZone", opt_string(pd.time_zone.as_deref()));
    set(&o, "userHeader", Int32Array::from(&pd.user_header[..]));
    set(&o, "parameters", string_array(&[independent, dependent]));
    o
}

fn grid_type_name(t: GridType) -> &'static str {
    match t {
        GridType::Undefined => "undefined",
        GridType::Hrap => "hrap",
        GridType::Albers => "albers",
        GridType::Specified => "specified",
    }
}

fn grid_object(d: GridData) -> Result<Object, JsValue> {
    let g = d.grid;
    let o = Object::new();
    set(&o, "version", d.version);
    set(&o, "pathname", reader::lossy(&g.pathname));
    set(&o, "recordType", g.record_type);
    set(&o, "gridType", grid_type_name(g.grid_type));
    set(&o, "structVersion", g.struct_version);
    set(&o, "gridVersion", g.version);
    set(&o, "infoSize", opt_num(g.info_size));
    set(&o, "gridInfoSize", opt_num(g.grid_info_size));
    set(&o, "dataUnits", g.data_units);
    set(&o, "dataType", g.data_type);
    set(&o, "dataSource", g.data_source);
    set(&o, "lowerLeftCellX", g.lower_left_cell_x);
    set(&o, "lowerLeftCellY", g.lower_left_cell_y);
    set(&o, "nx", g.nx as f64);
    set(&o, "ny", g.ny as f64);
    set(&o, "cellSize", g.cell_size);
    set(&o, "xCoordOfGridCellZero", g.x_coord_of_grid_cell_zero);
    set(&o, "yCoordOfGridCellZero", g.y_coord_of_grid_cell_zero);
    set(&o, "srsName", g.srs_name);
    set(&o, "srsDefinitionType", g.srs_definition_type);
    set(&o, "srsDefinition", g.srs_definition);
    set(
        &o,
        "storedSrsDefinition",
        opt_string(g.stored_srs_definition.as_deref()),
    );
    match g.albers {
        Some(a) => {
            let p = Object::new();
            set(&p, "datum", a.datum);
            set(&p, "units", a.units);
            set(&p, "firstStandardParallel", a.first_standard_parallel);
            set(&p, "secondStandardParallel", a.second_standard_parallel);
            set(&p, "centralMeridian", a.central_meridian);
            set(
                &p,
                "latitudeOfProjectionOrigin",
                a.latitude_of_projection_origin,
            );
            set(&p, "falseEasting", a.false_easting);
            set(&p, "falseNorthing", a.false_northing);
            set(&o, "albers", p);
        }
        None => set(&o, "albers", JsValue::NULL),
    }
    set(&o, "nullValue", g.null_value);
    set(&o, "timeZoneId", g.time_zone_id);
    set(&o, "timeZoneRawOffset", g.time_zone_raw_offset);
    set(&o, "isInterval", g.is_interval);
    set(&o, "isTimeStamped", g.is_time_stamped);
    set(&o, "startTime", opt_i64(g.start_time, "grid start time")?);
    set(&o, "endTime", opt_i64(g.end_time, "grid end time")?);
    set(&o, "storedStartTime", opt_num(g.stored_start_time));
    set(&o, "storedEndTime", opt_num(g.stored_end_time));
    set(&o, "compressionMethod", g.compression_method);
    set(
        &o,
        "sizeofCompressedElements",
        g.sizeof_compressed_elements as f64,
    );
    set(&o, "compressionScaleFactor", g.compression_scale_factor);
    set(&o, "compressionBase", g.compression_base);
    set(&o, "storageDataType", g.storage_data_type);
    set(&o, "minDataValue", opt_num(g.min_data_value));
    set(&o, "maxDataValue", opt_num(g.max_data_value));
    set(&o, "meanDataValue", opt_num(g.mean_data_value));
    set(
        &o,
        "rangeLimitTable",
        Float32Array::from(&g.range_limit_table[..]),
    );
    set(
        &o,
        "rangeCounts",
        Int32Array::from(&g.number_equal_or_exceeding_range_limit[..]),
    );
    // x fastest, lowest row first (index = y * nx + x); Java heclib GridData.getValue.
    let n = g.nx.checked_mul(g.ny).filter(|&n| n <= g.data.len());
    let Some(n) = n else {
        return Err(js_error(ReadError::new(
            reader::ErrorKind::Format,
            format!(
                "grid cells: {} values stored, {} x {} needed",
                g.data.len(),
                g.ny,
                g.nx
            ),
        )));
    };
    set(&o, "data", Float32Array::from(&g.data[..n]));
    Ok(o)
}

fn profile_object(d: ProfileData) -> Result<Object, JsValue> {
    let p = d.profile;
    let rows = p.times.len();
    let cols = p.depths.len();
    // A profile is a time x depth matrix; JavaScript has no 2-D typed array, so `values`
    // is row-major with the two extents given. A mismatch never reaches the caller.
    if p.values.len() != rows.saturating_mul(cols) {
        return Err(js_error(ReadError::new(
            reader::ErrorKind::Format,
            format!(
                "profile values: {} stored, {rows} times x {cols} depths needed",
                p.values.len()
            ),
        )));
    }
    let m = p.meta;
    let o = Object::new();
    set(&o, "version", 7u8);
    set(&o, "pathname", reader::lossy(&p.pathname));
    set(&o, "recordType", p.record_type);
    set(&o, "regular", m.interval.code > 0);
    set(&o, "intervalSeconds", i64_num(m.interval.code, "interval")?);
    set(&o, "intervalName", m.interval.name);
    set(&o, "numberTimes", rows as f64);
    set(&o, "numberDepths", cols as f64);
    set(&o, "times", i64_array(&p.times, "time")?);
    let (depths, width) = values_array(&p.depths);
    set(&o, "depths", depths);
    set(&o, "valueBits", width);
    set(&o, "values", values_array(&p.values).0);
    // ztsRetrieveReg7(): `quality` is the first word per time, `qualityWords` every word.
    let qes = m.quality_element_size;
    match (qes > 0, m.quality_words) {
        (true, Some(words)) => {
            let q: Vec<i32> = (0..rows)
                .map(|i| words.get(i * qes).copied().unwrap_or(0))
                .collect();
            set(&o, "qualityElementSize", qes as f64);
            set(&o, "quality", Int32Array::from(&q[..]));
            set(&o, "qualityWords", Int32Array::from(&words[..]));
        }
        _ => {
            set(&o, "qualityElementSize", 0.0);
            set(&o, "quality", JsValue::NULL);
            set(&o, "qualityWords", JsValue::NULL);
        }
    }
    set(&o, "depthUnits", p.depth_units);
    set(&o, "valueUnits", p.value_units);
    set(&o, "dataType", p.data_type);
    set(&o, "timeZone", opt_string(p.time_zone.as_deref()));
    set(&o, "precision", m.precision);
    set(
        &o,
        "timeOffsetSeconds",
        i64_num(m.time_offset_seconds, "time offset")?,
    );
    set(
        &o,
        "timeGranularitySeconds",
        i64_num(m.time_granularity_seconds, "time granularity")?,
    );
    set(
        &o,
        "julianBaseDate",
        i64_num(m.julian_base_date, "julian base date")?,
    );
    set(&o, "blocksFound", m.blocks_found as f64);
    set(
        &o,
        "blocksMissing",
        m.blocks_in_window.saturating_sub(m.blocks_found) as f64,
    );
    let blocks: Vec<String> = m.block_pathnames.iter().map(|b| reader::lossy(b)).collect();
    set(&o, "blockPathnames", string_array(&blocks));
    set(
        &o,
        "internalHeader",
        Int32Array::from(&m.internal_header.words[..]),
    );
    set(&o, "unitsPadded", m.internal_header.units_padded);
    set(&o, "userHeader", Int32Array::from(&m.user_header[..]));
    Ok(o)
}

fn location_object(d: LocationData) -> Result<Object, JsValue> {
    let l = d.location;
    let o = Object::new();
    set(&o, "version", d.version);
    set(&o, "pathname", reader::lossy(&l.pathname));
    set(&o, "recordType", d.record_type);
    set(&o, "source", reader::location_source_name(l.source));
    set(&o, "x", l.x);
    set(&o, "y", l.y);
    set(&o, "z", l.z);
    // The same three doubles in a typed array, where their stored bits are kept exactly.
    set(&o, "coordinates", Float64Array::from(&[l.x, l.y, l.z][..]));
    set(&o, "descriptorsStored", l.descriptors_stored);
    set(&o, "coordinateSystem", l.coordinate_system_code);
    set(&o, "coordinateId", l.coordinate_id);
    set(&o, "horizontalUnits", l.horizontal_units_code);
    set(&o, "horizontalDatum", l.horizontal_datum_code);
    set(&o, "verticalUnits", l.vertical_units_code);
    set(&o, "verticalDatum", l.vertical_datum_code);
    set(&o, "timeZone", opt_string(l.time_zone_name.as_deref()));
    set(
        &o,
        "supplemental",
        l.supplemental
            .as_deref()
            .map_or(JsValue::NULL, |b| Uint8Array::from(b).into()),
    );
    set(
        &o,
        "supplementalSource",
        opt_string(l.supplemental_source().map(|s| s.as_str())),
    );
    set(
        &o,
        "supplementalText",
        opt_string(l.supplemental_text().as_deref()),
    );
    set(
        &o,
        "supplementalEncoding",
        opt_string(l.supplemental_encoding()),
    );
    set(
        &o,
        "values1",
        d.values1
            .as_deref()
            .map_or(JsValue::NULL, |v| Int32Array::from(v).into()),
    );
    set(&o, "program", opt_string(d.program.as_deref()));
    set(&o, "lastWriteMs", opt_i64(d.last_write_ms, "last write")?);
    set(&o, "blockJulian", opt_i64(d.block_julian, "block date")?);
    Ok(o)
}

fn provenance_object(p: &EntryProvenance) -> Object {
    let o = Object::new();
    set(&o, "index", p.index as f64);
    set(&o, "dssVersion", p.version);
    set(&o, "pathname", p.pathname.as_str());
    set(&o, "recordType", p.record_type);
    set(&o, "status", p.status);
    let raw =
        (p.raw_status.unsigned_abs() <= MAX_SAFE_INTEGER as u64).then_some(p.raw_status as f64);
    set(&o, "rawStatus", opt_num(raw));
    set(&o, "recoverMode", p.recover_mode);
    set(&o, "recovered", p.recovered);
    set(&o, "duplicate", p.duplicate);
    set(&o, "duplicateOf", opt_num(p.duplicate_of.map(|i| i as f64)));
    set(&o, "hashChain", p.hash_chain);
    set(&o, "binEntryAddress", safe_u64(p.bin_entry_address));
    set(&o, "infoAddress", safe_u64(p.info_address));
    set(&o, "reachableByLookup", p.reachable_by_lookup);
    set(&o, "selectedByPathname", p.selected_by_pathname);
    set(&o, "infoProblem", opt_string(p.info_problem));
    set(&o, "infoDetail", opt_string(p.info_detail.as_deref()));
    set(&o, "tsSupported", p.ts_supported);
    set(&o, "passwordProtected", p.password_protected);
    set(&o, "outcome", p.outcome());
    o
}

/// `maxBlocks` as `readDataset` takes it.
fn max_blocks_arg(v: Option<f64>) -> Result<u64, JsValue> {
    match v {
        Some(x) => reader::count_arg(x, "maxBlocks", MAX_SAFE_INTEGER as u64).map_err(js_error),
        None => Ok(reader::DEFAULT_MAX_WINDOW_BLOCKS),
    }
}

/// `maxCells` of the profile reads.
fn max_cells_arg(v: Option<f64>) -> Result<u64, JsValue> {
    match v {
        Some(x) => reader::count_arg(x, "maxCells", MAX_CELLS_ARG).map_err(js_error),
        None => Ok(reader::DEFAULT_MAX_PROFILE_CELLS),
    }
}

/// `start`/`end` as `readDataset` takes them: both or neither.
fn window_arg(start: Option<f64>, end: Option<f64>) -> Result<Option<(i64, i64)>, JsValue> {
    match (start, end) {
        (None, None) => Ok(None),
        (Some(s), Some(e)) => Ok(Some((seconds_arg(s, "start")?, seconds_arg(e, "end")?))),
        _ => Err(invalid(
            "give both start and end, or neither (whole dataset)",
        )),
    }
}

// ---------------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------------

/// Return the DSS major version (6 or 7) of a buffer holding at least the file header.
#[wasm_bindgen(js_name = dssVersion)]
pub fn dss_version(head: &[u8]) -> Result<u8, JsValue> {
    let sig = reader::signature(head).map_err(js_error)?;
    Ok(reader::version_number(sig.version))
}

/// Header summary of a buffer holding at least the file header: `version`,
/// `versionString`, `byteOrder`, `numberRecords`, `fileSizeBytes`, `deadWords`,
/// `numberDeletes` (DSS7 only, else `null`).
#[wasm_bindgen(js_name = fileSignature)]
pub fn file_signature(head: &[u8]) -> Result<Object, JsValue> {
    let sig = reader::signature(head).map_err(js_error)?;
    signature_object(&sig)
}

/// HEC's missing-value test (`zisMissingFloat`/`zisMissingDouble`): -901, -902 and
/// -FLT_MAX. A stored NaN is a value.
#[wasm_bindgen(js_name = isHecMissing)]
pub fn is_hec_missing(value: f64) -> bool {
    dss_core::is_hec_missing(value)
}

/// A DSS6 or DSS7 file held in WASM memory, read-only. The constructor copies the
/// bytes; the JavaScript buffer may be reused afterwards. Call `free()` to release the
/// copy early.
#[wasm_bindgen]
pub struct DssFile {
    inner: DssBuffer,
}

#[wasm_bindgen]
impl DssFile {
    /// Open a whole DSS file given as a `Uint8Array`.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8]) -> Result<DssFile, JsValue> {
        let inner = DssBuffer::new(bytes.to_vec()).map_err(js_error)?;
        Ok(DssFile { inner })
    }

    /// Open a whole DSS file in explicit recovery mode. DSS7 catalogs also list
    /// entries in unlinked bins, which HEC's catalog omits, and pathname reads take the
    /// best-preserved copy of a duplicated pathname. Every integrity check still applies.
    /// `new DssFile` never does this. DSS6 has no recovery mode and reads as usual.
    #[wasm_bindgen(js_name = openRecovery)]
    pub fn open_recovery(bytes: &[u8]) -> Result<DssFile, JsValue> {
        let inner = DssBuffer::new_recovery(bytes.to_vec()).map_err(js_error)?;
        Ok(DssFile { inner })
    }

    /// Whether this object was created by `DssFile.openRecovery`.
    #[wasm_bindgen(getter, js_name = recoveryMode)]
    pub fn recovery_mode(&self) -> bool {
        self.inner.recovery_mode()
    }

    /// DSS major version, 6 or 7.
    #[wasm_bindgen(getter)]
    pub fn version(&self) -> u8 {
        self.inner.version()
    }

    /// Version string from the header (for example `7-IU` or `6-UF`).
    #[wasm_bindgen(getter, js_name = versionString)]
    pub fn version_string(&self) -> String {
        self.inner.signature().version_string.clone()
    }

    /// Header summary (same keys as `fileSignature`).
    pub fn signature(&self) -> Result<Object, JsValue> {
        signature_object(self.inner.signature())
    }

    /// Every catalog entry (all statuses, on-disk order) as parallel arrays:
    /// `pathname`, `storedPathname`, `recordType`, `status`, `rawStatus`,
    /// `lastWriteMs`, `duplicate`, `recovered`, plus `anomalies`.
    pub fn catalog(&mut self) -> Result<Object, JsValue> {
        let c = self.inner.catalog().map_err(js_error)?;
        catalog_object(c)
    }

    /// Read one time-series record (one block of regular data) by its full pathname.
    /// `times` are seconds since 1899-12-31 00:00 (no time zone applied); `values` are
    /// a `Float32Array` or `Float64Array` as stored, missing markers kept.
    #[wasm_bindgen(js_name = readRecord)]
    pub fn read_record(&mut self, pathname: &str) -> Result<Object, JsValue> {
        let d = self.inner.read_record(pathname).map_err(js_error)?;
        ts_object(d)
    }

    /// Read a time-series dataset (any D part) stitched from its blocks. `start` and
    /// `end` are seconds since 1899-12-31 00:00, both inclusive; give both or neither
    /// (whole dataset). `trimMissing` (default `true`) removes leading and trailing
    /// -901/-902/-FLT_MAX values of regular data, as heclib's `ztsTrim`. A window may
    /// span at most `maxBlocks` block periods of the dataset (default 100,000; a wider
    /// window throws `invalid_argument` before reading), because a windowed read probes
    /// every block period on the caller's thread. `maxRows` (default 16,777,216; a whole
    /// number from 1 to 2^30, so it cannot be switched off) limits the regular rows the read
    /// would assemble, with or without a window, because every period of the range gets a
    /// row, stored or not. A whole-dataset read also obeys `maxBlocks`. Both are upper
    /// estimates, checked before anything is assembled.
    #[wasm_bindgen(js_name = readDataset)]
    pub fn read_dataset(
        &mut self,
        pathname: &str,
        start: Option<f64>,
        end: Option<f64>,
        trim_missing: Option<bool>,
        max_blocks: Option<f64>,
        max_rows: Option<f64>,
    ) -> Result<Object, JsValue> {
        let max_blocks = max_blocks_arg(max_blocks)?;
        let max_rows = match max_rows {
            Some(v) => reader::count_arg(v, "maxRows", MAX_CELLS_ARG).map_err(js_error)?,
            None => reader::DEFAULT_MAX_ROWS,
        };
        let window = window_arg(start, end)?;
        let d = self
            .inner
            .read_dataset_limited(
                pathname,
                window,
                trim_missing.unwrap_or(true),
                max_blocks,
                max_rows,
            )
            .map_err(js_error)?;
        ts_object(d)
    }

    /// Read one paired-data record (types 200/205) by its full pathname. `values` is
    /// flat and curve-major (`numberCurves` runs of `numberOrdinates`).
    #[wasm_bindgen(js_name = readPairedData)]
    pub fn read_paired_data(&mut self, pathname: &str) -> Result<Object, JsValue> {
        let d = self.inner.read_paired_data(pathname).map_err(js_error)?;
        Ok(pd_object(d))
    }

    /// Read one grid record (types 400-431) by its full pathname. `data` is a
    /// `Float32Array` of `nx * ny` cells, x fastest, lowest row first. `maxCells`
    /// overrides the 16,777,216-cell limit with a whole number from 1 to
    /// [`MAX_CELLS_ARG`] (other values throw `invalid_argument`; nothing wraps);
    /// `preserveLegacyMissing` keeps raw legacy ZLIB bits instead of Java heclib's
    /// missing normalization.
    #[wasm_bindgen(js_name = readGrid)]
    pub fn read_grid(
        &mut self,
        pathname: &str,
        max_cells: Option<f64>,
        preserve_legacy_missing: Option<bool>,
    ) -> Result<Object, JsValue> {
        let mut opts = GridReadOptions {
            preserve_legacy_missing: preserve_legacy_missing.unwrap_or(false),
            ..GridReadOptions::default()
        };
        if let Some(v) = max_cells {
            let n = reader::count_arg(v, "maxCells", MAX_CELLS_ARG).map_err(js_error)?;
            opts.max_cells = usize::try_from(n).map_err(|_| invalid("maxCells is too large"))?;
        }
        let d = self.inner.read_grid(pathname, &opts).map_err(js_error)?;
        grid_object(d)
    }

    /// Read a regular profile dataset (types 102/107, any D part) stitched from its blocks.
    /// `values` is row-major: `values[t * numberDepths + d]`. `start` and `end` are as for
    /// `readDataset`. A read may span at most `maxBlocks` block periods (default 100,000)
    /// and `maxCells` times x depths (default 16,777,216; at most 2^30), estimated before
    /// the trim; wider reads throw `invalid_argument` before anything is assembled, with
    /// or without a window, because every period of the range gets a row.
    #[wasm_bindgen(js_name = readProfile)]
    pub fn read_profile(
        &mut self,
        pathname: &str,
        start: Option<f64>,
        end: Option<f64>,
        trim_missing: Option<bool>,
        max_blocks: Option<f64>,
        max_cells: Option<f64>,
    ) -> Result<Object, JsValue> {
        let max_blocks = max_blocks_arg(max_blocks)?;
        let max_cells = max_cells_arg(max_cells)?;
        let window = window_arg(start, end)?;
        let d = self
            .inner
            .read_profile(
                pathname,
                window,
                trim_missing.unwrap_or(true),
                max_blocks,
                max_cells,
            )
            .map_err(js_error)?;
        profile_object(d)
    }

    /// Read one stored profile record (one block) by its full pathname. `maxCells` is as
    /// for `readProfile`, checked against the block's size before decoding.
    #[wasm_bindgen(js_name = readProfileRecord)]
    pub fn read_profile_record(
        &mut self,
        pathname: &str,
        max_cells: Option<f64>,
    ) -> Result<Object, JsValue> {
        let max_cells = max_cells_arg(max_cells)?;
        let d = self
            .inner
            .read_profile_record(pathname, max_cells)
            .map_err(js_error)?;
        profile_object(d)
    }

    /// Read a DSS7 location record (type 20) by its pathname, `/A/B/Location Info////`.
    /// DSS6 files have none: throws `unsupported`.
    #[wasm_bindgen(js_name = readLocation)]
    pub fn read_location(&mut self, pathname: &str) -> Result<Object, JsValue> {
        let d = self.inner.read_location(pathname).map_err(js_error)?;
        location_object(d)
    }

    /// The location associated with a data pathname, or `null`. DSS7: the record
    /// `/A/B/Location Info////`. DSS6: the coordinates in the time-series header; a
    /// pathname naming no stored block reads the whole dataset and throws
    /// `DssLocationConflictError` when its blocks store different coordinates.
    #[wasm_bindgen(js_name = readAssociatedLocation)]
    pub fn read_associated_location(&mut self, pathname: &str) -> Result<JsValue, JsValue> {
        match self
            .inner
            .read_associated_location(pathname)
            .map_err(js_error)?
        {
            Some(d) => Ok(location_object(d)?.into()),
            None => Ok(JsValue::NULL),
        }
    }

    /// Forensic: where a `catalog()` row sits and what reading it would give (`outcome`),
    /// without decoding values.
    #[wasm_bindgen(js_name = entryProvenance)]
    pub fn entry_provenance(&mut self, index: f64) -> Result<Object, JsValue> {
        let i = reader::index_arg(index).map_err(js_error)?;
        let p = self.inner.entry_provenance(i).map_err(js_error)?;
        Ok(provenance_object(&p))
    }

    /// Forensic: `{provenance, record}` for the time-series record behind a `catalog()`
    /// row, reaching copies and orphans a pathname read cannot. A record whose info block
    /// fails verification throws `DssRecordUnavailableError` (`kind` `format`, with
    /// `problem`, `index`, `infoAddress`); no data is substituted. A row out of range or
    /// not live throws `DssIndexError` (`kind` `index`).
    #[wasm_bindgen(js_name = readRecordAt)]
    pub fn read_record_at(&mut self, index: f64) -> Result<Object, JsValue> {
        let i = reader::index_arg(index).map_err(js_error)?;
        let (p, ts) = self.inner.read_record_at(i).map_err(js_error)?;
        let o = Object::new();
        set(&o, "provenance", provenance_object(&p));
        set(&o, "record", ts_object(ts)?);
        Ok(o)
    }
}

#![forbid(unsafe_code)]

//! Target-independent read layer behind the JavaScript exports.
//!
//! Everything here is plain Rust over an owned byte buffer, so it builds and is tested
//! on the host as well as on `wasm32-unknown-unknown`. The `wasm-bindgen` layer in
//! `lib.rs` only converts these results to JavaScript values.
//!
//! The dispatch follows the Python binding (`crates/dss-py/src/lib.rs`, `Inner`): DSS7
//! reads go through [`Dss7File`] with one cached catalog, DSS6 reads through
//! [`Dss6File`] with one cached [`DatasetIndex`]. No format rule lives in this crate;
//! the readers in `dss-core` cite their `heclib` sources.

use dss_core::dss6::{self, DatasetIndex, DatasetOptions, Dss6File, TimeWindow, TsLayout};
use dss_core::dss7::{
    self, CatalogOptions, Dss7Catalog, Dss7File, RecordRef, TsReadOptions, TsWindow,
};
use dss_core::{
    AnomalyKind, CatalogAnomaly, DssVersion, FileSignature, Grid, GridError, GridReadOptions,
    PairedData, RecordStatus, TimeSeries,
};
use std::fmt;
use std::io::Cursor;
use std::rc::Rc;

/// Error category, the same split the Python binding raises as exception classes
/// (decision D32). JavaScript sees it as the `kind` property of the thrown `Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Damaged or non-DSS bytes (Python `DssFormatError`).
    Format,
    /// Pathname not in the file, or no data for it (`DssPathnameNotFoundError`).
    NotFound,
    /// Record type or encoding this reader does not decode (`DssUnsupportedRecordError`).
    Unsupported,
    /// Other DSS failure: limits, password protection (`DssError`).
    Dss,
    /// The caller passed an invalid argument (`ValueError`).
    InvalidArgument,
    /// P40: catalog index out of range, or the entry is not live (Python `IndexError`).
    Index,
    /// P40: the catalog entry's info block fails verification
    /// (`DssRecordUnavailableError`, a `DssFormatError`; no data is returned).
    Unavailable,
    /// P40: DSS6 blocks of a series store different coordinates
    /// (`DssLocationConflictError`, a `DssError`).
    LocationConflict,
}

impl ErrorKind {
    /// Value of the JavaScript `kind` property.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Format | ErrorKind::Unavailable => "format",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Dss | ErrorKind::LocationConflict => "dss",
            ErrorKind::InvalidArgument => "invalid_argument",
            ErrorKind::Index => "index",
        }
    }

    /// Value of the JavaScript `name` property; the Python exception class names.
    pub fn error_name(self) -> &'static str {
        match self {
            ErrorKind::Format => "DssFormatError",
            ErrorKind::NotFound => "DssPathnameNotFoundError",
            ErrorKind::Unsupported => "DssUnsupportedRecordError",
            ErrorKind::Dss => "DssError",
            ErrorKind::InvalidArgument => "DssValueError",
            ErrorKind::Index => "DssIndexError",
            ErrorKind::Unavailable => "DssRecordUnavailableError",
            ErrorKind::LocationConflict => "DssLocationConflictError",
        }
    }
}

/// A read failure: its kind and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub kind: ErrorKind,
    pub message: String,
    /// Set for [`ErrorKind::Unavailable`].
    pub unavailable: Option<UnavailableInfo>,
}

/// Provenance facts of an [`ErrorKind::Unavailable`] error (the attributes of Python's
/// `DssRecordUnavailableError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableInfo {
    /// `zeroed`, `bad_flag`, `hash_mismatch`, `status`, `pathname_mismatch`,
    /// `out_of_bounds` or `unreadable`.
    pub problem: &'static str,
    /// Catalog row.
    pub index: usize,
    /// Byte offset of the info block.
    pub info_address: u64,
}

impl ReadError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            unavailable: None,
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReadError {}

pub type ReadResult<T> = Result<T, ReadError>;

// In-memory sources have no OS errors; an I/O error here is a short buffer.
fn map_dss7(e: dss7::Dss7Error) -> ReadError {
    use dss7::Dss7Error as E;
    match e {
        E::BadPathname => ReadError::new(ErrorKind::InvalidArgument, e.to_string()),
        // `NotLiveEntry` (Python: IndexError) comes only from `record_at`, which the
        // pathname-based reads here never call; it falls under `Format` like the rest.
        other => ReadError::new(ErrorKind::Format, other.to_string()),
    }
}

/// A malformed pathname is "no data" for the record readers, as in the Python binding.
fn map_lookup7(e: dss7::Dss7Error) -> ReadError {
    match e {
        dss7::Dss7Error::BadPathname => ReadError::new(ErrorKind::NotFound, e.to_string()),
        other => map_dss7(other),
    }
}

/// Whether a record type is a DSS7 profile (`zdataTypeDescriptions.h`: 102, 107, 112,
/// 117), as in the Python binding's `record_is_profile`.
fn record_is_profile(t: i32) -> bool {
    matches!(
        t,
        dss7::ts::RTS_PROFILE
            | dss7::ts::RTD_PROFILE
            | dss7::ts::ITS_PROFILE
            | dss7::ts::ITD_PROFILE
    )
}

fn map_ts7(e: dss7::TsError) -> ReadError {
    use dss7::TsError as E;
    // The Python binding's `map_ts7_hint`: name profile records as such.
    if let E::UnsupportedRecordType { record_type, .. } = &e {
        if record_is_profile(*record_type) {
            return ReadError::new(
                ErrorKind::Unsupported,
                format!("{e}: it is a profile record; read it with readProfile()"),
            );
        }
    }
    let kind = match e {
        E::Dss7(inner) => return map_dss7(inner),
        E::NotFound { .. } | E::BadPathname { .. } => ErrorKind::NotFound,
        E::UnsupportedRecordType { .. } => ErrorKind::Unsupported,
        E::BadWindow => ErrorKind::InvalidArgument,
        E::Corrupt { .. } => ErrorKind::Format,
        E::TooManyValues { .. } => ErrorKind::Dss,
    };
    ReadError::new(kind, e.to_string())
}

fn map_dss6(e: dss6::Dss6Error) -> ReadError {
    ReadError::new(ErrorKind::Format, e.to_string())
}

fn map_ts6(e: dss6::TsError) -> ReadError {
    use dss6::TsError as E;
    let kind = match &e {
        E::Dss6(_) => ErrorKind::Format,
        // The default policy (`NoDataPolicy::Return`) never refuses; kept for safety.
        E::NotFound { .. } | E::BadPathname { .. } | E::NoData { .. } => ErrorKind::NotFound,
        E::UnsupportedRecordType { .. } | E::UnsupportedCompression { .. } => {
            ErrorKind::Unsupported
        }
        E::PasswordProtected | E::TooLarge { .. } => ErrorKind::Dss,
        E::Corrupt { .. } => ErrorKind::Format,
    };
    ReadError::new(kind, e.to_string())
}

fn map_pd7(e: dss7::PdError) -> ReadError {
    use dss7::PdError as E;
    let kind = match e {
        E::Dss7(inner) => return map_lookup7(inner),
        E::NotFound { .. } => ErrorKind::NotFound,
        E::UnsupportedRecordType { .. } => ErrorKind::Unsupported,
        E::Corrupt { .. } => ErrorKind::Format,
    };
    ReadError::new(kind, e.to_string())
}

fn map_pd6(pathname: &str, e: dss6::PdError) -> ReadError {
    use dss6::PdError as E;
    let kind = match &e {
        E::Dss6(_) => ErrorKind::Format,
        E::NotFound { .. } => return ReadError::new(ErrorKind::NotFound, e.to_string()),
        E::UnsupportedRecordType { .. } => ErrorKind::Unsupported,
        E::PasswordProtected => ErrorKind::Dss,
        E::Corrupt { .. } => ErrorKind::Format,
    };
    // The DSS6 messages carry no pathname; add it.
    ReadError::new(kind, format!("{pathname}: {e}"))
}

fn map_grid(pathname: &str, e: GridError) -> ReadError {
    use GridError as E;
    match e {
        E::Dss6(inner) => map_dss6(inner),
        E::Dss7(inner) => map_lookup7(inner),
        E::NotFound => ReadError::new(
            ErrorKind::NotFound,
            format!("grid record not found: {pathname}"),
        ),
        E::UnsupportedRecordType { .. }
        | E::UnsupportedStructVersion { .. }
        | E::UnsupportedCompression { .. }
        | E::UnsupportedStorageType { .. } => {
            ReadError::new(ErrorKind::Unsupported, format!("{pathname}: {e}"))
        }
        E::PasswordProtected | E::LimitExceeded { .. } => {
            ReadError::new(ErrorKind::Dss, format!("{pathname}: {e}"))
        }
        E::Corrupt { .. } => ReadError::new(ErrorKind::Format, format!("{pathname}: {e}")),
    }
}

fn not_found(pathname: &str) -> ReadError {
    ReadError::new(ErrorKind::NotFound, format!("record not found: {pathname}"))
}

/// Pathname bytes decoded for display: UTF-8 when valid, else Latin-1 (as the Python
/// binding's `lossy`).
pub fn lossy(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

/// Candidate byte encodings of a pathname given as text: UTF-8 first, then Latin-1
/// when it differs (the round trip of [`lossy`]).
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

/// Read the file signature from the start of a buffer (at least the header).
pub fn signature(head: &[u8]) -> ReadResult<FileSignature> {
    FileSignature::from_bytes(head).map_err(|e| ReadError::new(ErrorKind::Format, e.to_string()))
}

pub fn version_number(v: DssVersion) -> u8 {
    match v {
        DssVersion::V6 => 6,
        DssVersion::V7 => 7,
    }
}

pub fn status_name(s: RecordStatus) -> &'static str {
    match s {
        RecordStatus::Live => "live",
        RecordStatus::Alias => "alias",
        RecordStatus::Deleted => "deleted",
        RecordStatus::Renamed => "renamed",
        RecordStatus::RewrittenOld => "rewritten_old",
        RecordStatus::Other => "other",
    }
}

pub fn anomaly_name(k: AnomalyKind) -> &'static str {
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

/// Every catalog entry (all statuses) as parallel columns, the shape of the Python
/// binding's `NativeFile.catalog()`.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogData {
    /// Pathname for display (DSS6: with the D part HEC shows).
    pub pathname: Vec<String>,
    /// Pathname as stored.
    pub stored_pathname: Vec<String>,
    pub record_type: Vec<i32>,
    pub status: Vec<&'static str>,
    pub raw_status: Vec<i64>,
    /// Milliseconds since 1970-01-01 UTC; -1 when the version does not store it.
    pub last_write_ms: Vec<i64>,
    pub duplicate: Vec<bool>,
    pub recovered: Vec<bool>,
    pub anomalies: Vec<CatalogAnomaly>,
}

impl CatalogData {
    fn with_capacity(n: usize) -> Self {
        Self {
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

    fn from_dss7(cat: &Dss7Catalog) -> Self {
        let mut d = Self::with_capacity(cat.catalog.entries.len());
        for (e, m) in cat.catalog.entries.iter().zip(cat.meta.iter()) {
            let p = lossy(&e.pathname);
            d.stored_pathname.push(p.clone());
            d.pathname.push(p);
            d.record_type.push(e.record_type);
            d.status.push(status_name(e.status));
            d.raw_status.push(i64::from(e.raw_status));
            d.last_write_ms.push(e.last_write_ms.unwrap_or(-1));
            d.duplicate.push(m.duplicate_of.is_some());
            d.recovered.push(m.recovered);
        }
        d.anomalies = cat.catalog.anomalies.clone();
        d
    }

    fn from_dss6(walk: &dss6::BinWalk) -> Self {
        let mut d = Self::with_capacity(walk.entries.len());
        let mut seen = std::collections::HashSet::new();
        for e in &walk.entries {
            d.pathname.push(lossy(&e.display_pathname()));
            d.stored_pathname.push(lossy(&e.pathname));
            d.record_type.push(e.record_type);
            let st = e.record_status();
            d.status.push(status_name(st));
            d.raw_status.push(i64::from(e.status));
            d.last_write_ms.push(-1);
            d.duplicate
                .push(st == RecordStatus::Live && !seen.insert(e.pathname.clone()));
            d.recovered.push(false);
        }
        d.anomalies = walk.anomalies.clone();
        d
    }

    pub fn len(&self) -> usize {
        self.pathname.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pathname.is_empty()
    }
}

/// A decoded time series plus read metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct TsData {
    pub version: u8,
    pub ts: TimeSeries,
    pub time_offset_seconds: i64,
    pub pattern: bool,
    pub blocks_found: usize,
    pub blocks_missing: usize,
    pub warnings: Vec<String>,
    /// Stored precision (record reads; `None` for DSS6 dataset reads).
    pub precision: Option<i32>,
}

fn from_dss7_ts(r: dss7::Dss7Ts) -> TsData {
    TsData {
        version: 7,
        time_offset_seconds: r.meta.time_offset_seconds,
        pattern: r.meta.pattern,
        blocks_found: r.meta.blocks_found,
        blocks_missing: r.meta.blocks_in_window.saturating_sub(r.meta.blocks_found),
        warnings: Vec::new(),
        precision: Some(r.meta.precision),
        ts: r.ts,
    }
}

fn from_dss6_record(r: dss6::Dss6TsRecord) -> TsData {
    TsData {
        version: 6,
        time_offset_seconds: i64::from(r.time_offset_minutes) * 60,
        pattern: matches!(
            r.layout,
            TsLayout::RegularPattern | TsLayout::IrregularPattern
        ),
        blocks_found: 1,
        blocks_missing: 0,
        warnings: Vec::new(),
        precision: Some(r.precision),
        ts: r.to_timeseries(),
    }
}

fn from_dss6_dataset(d: dss6::Dss6Dataset) -> TsData {
    // heclib_f/src/zrrtsi.f:ZRRTSI6 identifies patterns by the D part only.
    let pattern = dss6::pathname::split_parts(&d.series.pathname)
        .map(|parts| dss6::ts::is_pattern_d_part(&d.series.pathname[parts[3].clone()]))
        .unwrap_or(false);
    TsData {
        version: 6,
        time_offset_seconds: i64::from(d.report.time_offset_minutes.unwrap_or(0)) * 60,
        pattern,
        blocks_found: d.report.blocks_found,
        blocks_missing: d.report.blocks_missing,
        warnings: d.report.warnings,
        precision: None,
        ts: d.series,
    }
}

/// A decoded paired-data record and its file version.
#[derive(Debug, Clone, PartialEq)]
pub struct PdData {
    pub version: u8,
    pub pd: PairedData,
}

/// A decoded grid record and its file version.
#[derive(Debug, Clone, PartialEq)]
pub struct GridData {
    pub version: u8,
    pub grid: Grid,
}

/// Time-series record types (100-199), for the typed "not a time series" error (D29).
fn record_is_ts(t: i32) -> bool {
    (100..200).contains(&t)
}

enum Inner {
    V6 {
        file: Box<Dss6File<Vec<u8>>>,
        index: Option<DatasetIndex>,
        /// Bin walk of the catalog-index route, built once.
        walk: Option<Box<dss6::BinWalk>>,
        /// Live time-series pathnames by upper-cased (A, B, C, E, F), built once for the
        /// associated-location dataset fallback.
        ts_blocks: Option<routes::Ts6Blocks>,
    },
    V7 {
        file: Box<Dss7File<Cursor<SharedBytes>>>,
        /// The bytes `file` reads, for the size check of the profile guard.
        bytes: SharedBytes,
        catalog: Option<Box<Dss7Catalog>>,
        recover_catalog: Option<Box<Dss7Catalog>>,
    },
}

/// The file bytes, shared without a copy between the reader and the profile guard.
/// Reference counted, not atomic: this crate has no threads (spec D11).
#[derive(Clone)]
struct SharedBytes(Rc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A DSS6 or DSS7 file held in memory, read-only.
pub struct DssBuffer {
    sig: FileSignature,
    inner: Inner,
    /// DSS7 recovery mode ([`DssBuffer::new_recovery`]); ignored for DSS6, as the Python
    /// binding's `recover` is.
    recover: bool,
}

impl DssBuffer {
    /// Open a whole DSS file given as bytes. The buffer is owned; nothing is written.
    pub fn new(bytes: Vec<u8>) -> ReadResult<Self> {
        Self::open(bytes, false)
    }

    /// Open in explicit recovery mode (P06, P16; the Python `recover=True`). DSS7 catalogs
    /// also list entries in unlinked bins (interrupted writes, which HEC's catalog omits)
    /// and pathname reads take the best-preserved copy of a duplicated pathname, through
    /// [`Dss7Catalog::find`] and [`Dss7File::record_at`]. Every integrity check still
    /// applies. DSS6 has no recovery mode: the file reads as usual.
    pub fn new_recovery(bytes: Vec<u8>) -> ReadResult<Self> {
        Self::open(bytes, true)
    }

    fn open(bytes: Vec<u8>, recover: bool) -> ReadResult<Self> {
        let n = bytes.len().min(dss_core::header::MIN_HEADER_BYTES);
        let sig = signature(&bytes[..n])?;
        let inner = match sig.version {
            DssVersion::V7 => {
                let bytes = SharedBytes(Rc::new(bytes));
                Inner::V7 {
                    file: Box::new(Dss7File::new(Cursor::new(bytes.clone())).map_err(map_dss7)?),
                    bytes,
                    catalog: None,
                    recover_catalog: None,
                }
            }
            DssVersion::V6 => Inner::V6 {
                file: Box::new(Dss6File::new(bytes).map_err(map_dss6)?),
                index: None,
                walk: None,
                ts_blocks: None,
            },
        };
        Ok(Self {
            sig,
            inner,
            recover,
        })
    }

    /// Whether the file was opened with [`DssBuffer::new_recovery`].
    pub fn recovery_mode(&self) -> bool {
        self.recover
    }

    pub fn signature(&self) -> &FileSignature {
        &self.sig
    }

    pub fn version(&self) -> u8 {
        version_number(self.sig.version)
    }

    /// Every catalog entry, all statuses, in on-disk bin order.
    pub fn catalog(&mut self) -> ReadResult<CatalogData> {
        let recover = self.recover;
        match &mut self.inner {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
                ..
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                Ok(CatalogData::from_dss7(v7_catalog(file, slot, recover)?))
            }
            Inner::V6 { file, .. } => {
                let walk = file
                    .walk_bins(&dss6::CatalogOptions::default())
                    .map_err(map_dss6)?;
                Ok(CatalogData::from_dss6(&walk))
            }
        }
    }

    /// Record type of `pathname` when the file holds it.
    fn record_type_of(&mut self, pathname: &str) -> Option<i32> {
        encodings(pathname)
            .into_iter()
            .find_map(|p| match &mut self.inner {
                Inner::V7 { file, .. } => file.lookup(&p).ok().flatten().map(|r| r.record_type),
                Inner::V6 { file, .. } => {
                    file.lookup(&p).ok().flatten().map(|r| r.entry.record_type)
                }
            })
    }

    /// Turn not-found into the typed unsupported-record error when the pathname names
    /// a record that is not a time series (decision D29).
    fn refine(&mut self, err: ReadError, pathname: &str) -> ReadError {
        if err.kind == ErrorKind::NotFound {
            if let Some(t) = self.record_type_of(pathname) {
                if !record_is_ts(t) {
                    return ReadError::new(
                        ErrorKind::Unsupported,
                        format!(
                            "record {pathname} has type {t}, which is not a time series (decision D29)"
                        ),
                    );
                }
            }
        }
        err
    }

    /// Read one time-series record (one block of regular data) by its full pathname.
    /// Values are as stored; HEC missing markers are not converted.
    pub fn read_record(&mut self, pathname: &str) -> ReadResult<TsData> {
        let r = self.read_record_inner(pathname);
        r.map_err(|e| self.refine(e, pathname))
    }

    fn read_record_inner(&mut self, pathname: &str) -> ReadResult<TsData> {
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            let r = match &mut self.inner {
                Inner::V7 {
                    file,
                    recover_catalog,
                    ..
                } => match v7_record(file, recover_catalog, recover, &p, map_dss7)? {
                    Some(rec) => file.read_ts_record(&rec).map(from_dss7_ts).map_err(map_ts7),
                    None => Err(not_found(pathname)),
                },
                Inner::V6 { file, .. } => file
                    .read_ts_record(&p)
                    .map(from_dss6_record)
                    .map_err(map_ts6),
            };
            match r {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }

    /// Read a time-series dataset (any D part) stitched from its blocks. `window` is
    /// `(start, end)` in seconds since 1899-12-31 00:00, both inclusive; `None` reads
    /// the whole dataset. `trim_missing` removes leading and trailing -901/-902/-FLT_MAX
    /// values of regular data, as heclib's `ztsTrim` does.
    /// Windows are limited to [`DEFAULT_MAX_WINDOW_BLOCKS`] block periods; see
    /// [`Self::read_dataset_bounded`].
    pub fn read_dataset(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim_missing: bool,
    ) -> ReadResult<TsData> {
        self.read_dataset_bounded(pathname, window, trim_missing, DEFAULT_MAX_WINDOW_BLOCKS)
    }

    /// [`Self::read_dataset`] with an explicit limit on the block periods a read may span
    /// ([`window_blocks`]). A wider read is refused as `InvalidArgument` before any block
    /// is probed.
    pub fn read_dataset_bounded(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim_missing: bool,
        max_blocks: u64,
    ) -> ReadResult<TsData> {
        self.read_dataset_limited(pathname, window, trim_missing, max_blocks, DEFAULT_MAX_ROWS)
    }

    /// [`Self::read_dataset_bounded`] with an explicit row limit. A regular dataset gets a
    /// row for every period of its range, stored or not, so a range wider than `max_rows`
    /// periods is refused as `InvalidArgument` before anything is assembled
    /// ([`Self::dataset_time_guard`]). Whole-dataset reads also obey `max_blocks`, because
    /// they walk every block period between the first and last stored blocks.
    pub fn read_dataset_limited(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim_missing: bool,
        max_blocks: u64,
        max_rows: u64,
    ) -> ReadResult<TsData> {
        if let Some((s, e)) = window {
            if e < s {
                return Err(ReadError::new(
                    ErrorKind::InvalidArgument,
                    "time window end is before its start",
                ));
            }
            let n = window_blocks(pathname, s, e);
            if n > max_blocks {
                return Err(ReadError::new(
                    ErrorKind::InvalidArgument,
                    format!(
                        "time window spans up to {n} block periods of {pathname} (limit \
                         {max_blocks}); narrow the window, raise maxBlocks, or read the \
                         whole dataset"
                    ),
                ));
            }
        }
        for p in encodings(pathname) {
            self.dataset_time_guard(&p, window, max_blocks, max_rows)?;
        }
        let r = self.read_dataset_inner(pathname, window, trim_missing);
        r.map_err(|e| self.refine(e, pathname))
    }

    fn read_dataset_inner(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim_missing: bool,
    ) -> ReadResult<TsData> {
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            let r = match &mut self.inner {
                Inner::V7 {
                    file,
                    catalog,
                    recover_catalog,
                    ..
                } => {
                    let opts = TsReadOptions {
                        window: window.map(|(start, end)| TsWindow { start, end }),
                        trim_missing,
                    };
                    if window.is_none() {
                        let slot = if recover { recover_catalog } else { catalog };
                        let cat = v7_catalog(file, slot, recover)?;
                        file.read_ts_with_catalog(cat, &p, &opts)
                    } else {
                        file.read_ts(&p, &opts)
                    }
                    .map(from_dss7_ts)
                    .map_err(map_ts7)
                }
                Inner::V6 { file, index, .. } => {
                    let opts = DatasetOptions {
                        window: window.map(|(s, e)| TimeWindow::new(s, e)),
                        trim_missing,
                        ..DatasetOptions::whole()
                    };
                    if index.is_none() {
                        *index = Some(DatasetIndex::build(file.as_ref()).map_err(map_ts6)?);
                    }
                    let idx = index.as_ref().expect("index just set");
                    file.read_ts_dataset_indexed(idx, &p, &opts)
                        .map(from_dss6_dataset)
                        .map_err(map_ts6)
                }
            };
            match r {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }

    /// Read one paired-data record (types 200/205) by its full pathname. Values keep
    /// their stored width; missing markers are not converted.
    pub fn read_paired_data(&mut self, pathname: &str) -> ReadResult<PdData> {
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            let r = match &mut self.inner {
                Inner::V7 {
                    file,
                    recover_catalog,
                    ..
                } => match v7_record(file, recover_catalog, recover, &p, map_lookup7)? {
                    Some(rec) => file
                        .read_pd_record(&rec)
                        .map(|r| PdData {
                            version: 7,
                            pd: r.pd,
                        })
                        .map_err(map_pd7),
                    None => Err(not_found(pathname)),
                },
                Inner::V6 { file, .. } => file
                    .read_pd(&p)
                    .map(|r| PdData {
                        version: 6,
                        pd: r.pd,
                    })
                    .map_err(|e| map_pd6(pathname, e)),
            };
            match r {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }

    /// Read one grid record (types 400-431) by its full pathname. Cells are float32 as
    /// decoded, x fastest, lowest row first (index `y * nx + x`).
    pub fn read_grid(&mut self, pathname: &str, opts: &GridReadOptions) -> ReadResult<GridData> {
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            let r = match &mut self.inner {
                Inner::V7 {
                    file,
                    recover_catalog,
                    ..
                } => match v7_record(file, recover_catalog, recover, &p, map_lookup7)? {
                    Some(rec) => file
                        .read_grid_record_with(&rec, opts)
                        .map(|grid| GridData { version: 7, grid })
                        .map_err(|e| map_grid(pathname, e)),
                    None => Err(map_grid(pathname, GridError::NotFound)),
                },
                Inner::V6 { file, .. } => file
                    .read_grid_with(&p, opts)
                    .map(|grid| GridData { version: 6, grid })
                    .map_err(|e| map_grid(pathname, e)),
            };
            match r {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }
}

/// Default limit on the block periods a dataset window may span: 100,000, which is
/// 273 years of daily blocks (the smallest block) and far more for every other block
/// size. A windowed read probes one block per period (DSS7 `ztsRetrieve` walks block
/// start dates; DSS6 `zrrtsi6`/`zritsi6` likewise), at about 30 us per empty block on
/// the reference workstation, so the default bounds a call to a few seconds on the caller's thread.
pub const DEFAULT_MAX_WINDOW_BLOCKS: u64 = 100_000;

const DAY_SECONDS: i64 = 86_400;

/// Upper estimate of the block periods a window `[start, end]` (seconds) spans for the
/// dataset named by `pathname`. The block size comes from the E part with the DSS6
/// rules (`zbegdt6.f`, `zirbeg6.f`, through `dss6::interval::parse_e_part`), taking
/// daily blocks for 15-59 minute data as pre-`6-I` files do, so the size is never
/// larger than the block heclib uses and the estimate never undercounts. An E part that
/// names no interval counts daily blocks.
pub fn window_blocks(pathname: &str, start: i64, end: i64) -> u64 {
    use dss6::interval::{parse_e_part, regular_block_for, BlockSpan, EPart};
    let e = pathname.split('/').nth(5).unwrap_or("");
    let span = match parse_e_part(e.as_bytes()) {
        Some(EPart::Regular { minutes, .. }) => regular_block_for(minutes, 0),
        Some(EPart::Pseudo { block, .. }) | Some(EPart::Irregular { block }) => block,
        None => BlockSpan::Day,
    };
    // Shortest length of one block period in days (a month is at least 28 days).
    let min_days: i128 = match span {
        BlockSpan::Day => 1,
        BlockSpan::Month => 28,
        BlockSpan::Year => 365,
        BlockSpan::Decade => 3_652,
        BlockSpan::Century => 36_524,
    };
    let days = (i128::from(end) - i128::from(start)).max(0) / i128::from(DAY_SECONDS);
    // +2: the window may start late in one block and end early in another.
    u64::try_from(days / min_days + 2).unwrap_or(u64::MAX)
}

/// A JavaScript count argument (`maxCells`, `maxBlocks`): a whole number in
/// `1..=max`. Negative, fractional, non-finite and out-of-range values are refused, so
/// nothing wraps (wasm-bindgen converts a number to `u32` modulo 2^32).
pub fn count_arg(v: f64, what: &str, max: u64) -> ReadResult<u64> {
    if !v.is_finite() || v.fract() != 0.0 || v < 1.0 || v > max as f64 {
        return Err(ReadError::new(
            ErrorKind::InvalidArgument,
            format!("{what} must be a whole number from 1 to {max}, got {v}"),
        ));
    }
    Ok(v as u64)
}

fn v7_catalog<'a>(
    file: &mut Dss7File<Cursor<SharedBytes>>,
    slot: &'a mut Option<Box<Dss7Catalog>>,
    recover: bool,
) -> ReadResult<&'a Dss7Catalog> {
    if slot.is_none() {
        let opts = CatalogOptions {
            recover_unlinked: recover,
            ..CatalogOptions::default()
        };
        let cat = file.catalog_with(&opts).map_err(map_dss7)?;
        *slot = Some(Box::new(cat));
    }
    Ok(slot.as_deref().expect("catalog just set"))
}

/// The record behind `pathname`: the hash lookup, or in recovery mode the copy the
/// recovery catalog selects (as the Python binding's `read_by_pathname`).
fn v7_record(
    file: &mut Dss7File<Cursor<SharedBytes>>,
    recover_slot: &mut Option<Box<Dss7Catalog>>,
    recover: bool,
    pathname: &[u8],
    map_lookup: fn(dss7::Dss7Error) -> ReadError,
) -> ReadResult<Option<RecordRef>> {
    if recover {
        let cat = v7_catalog(file, recover_slot, true)?;
        return match cat.find(pathname) {
            Some(i) => file.record_at(cat, i).map(Some).map_err(map_dss7),
            None => Ok(None),
        };
    }
    file.lookup(pathname).map_err(map_lookup)
}

// P40: profile, location and catalog-index routes.
mod routes;
pub use routes::*;

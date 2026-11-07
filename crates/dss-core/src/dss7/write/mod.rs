//! DSS7 writer, packet P20: create a file, add regular-interval time series (float RTS
//! 100, double RTD 105) and replace or merge values in existing blocks with HEC's
//! default store semantics. Specification: `packets/P20_dss7_ts_writer.md`.
//!
//! Packet P21 adds irregular-interval time series (float ITS 110, double ITD 115;
//! [`write_its`]) with HEC's "replace" store semantics (storage flag 1, what hecdss
//! uses): inside the new time window the new values replace what is stored, the rest of
//! each block is kept, and blocks wholly inside the window that receive no values are
//! deleted. Specification: `packets/P21_dss7_its_writer.md`.
//!
//! Every write:
//!
//! 1. validates the input (pathname rules of P20 §3.1; refusal, never truncation);
//! 2. tests HEC's lock words on its own handle and applies an explicit
//!    [`LockPolicy`] ([`crate::lock::ensure_unlocked_for_write`]);
//! 3. holds the write lock while writing (`lockhold`);
//! 4. writes block by block, each block as one transaction ordered "data first,
//!    pointers last" (P20 §4, [`WriteStep`]).
//!
//! Format rules cite `heclib/heclib_c/src/` of `hec-dss` tag `7-IU-15` in the
//! submodules: `path` (validation), `block` (aggregate, compression, internal header),
//! `engine` (store semantics, allocation, bins, header bookkeeping).
//!
//! ```no_run
//! use dss_core::dss7::write::{create_dss7, write_rts, RtsInput, RtsValues, WriteOptions};
//! create_dss7("new.dss".as_ref())?;
//! let v = [1.0f32, 2.0, 3.0];
//! let input = RtsInput::new(b"/BASIN/LOC/FLOW//1Hour/SIM/", 43_831 * 86_400 + 3_600,
//!                           RtsValues::F32(&v), b"cfs", b"INST-VAL");
//! let report = write_rts("new.dss".as_ref(), &input, &WriteOptions::default())?;
//! # Ok::<(), dss_core::dss7::write::WriteError>(())
//! ```

mod block;
mod engine;
// P24: grid records.
mod grid;
pub(crate) mod lockhold;
pub(crate) mod overlay;
mod path;
pub mod pd;
// P38: multi-word quality, notes and patterns.
mod variants;
// P39: profile and location records.
pub mod location;
pub mod profile;

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub use block::{BlockValues, MAX_HEADER_STRING_BYTES};
pub use location::LocationInput;
pub use path::pseudo_e_part;
pub use path::validate_pd_pathname;
pub use path::{
    irregular_e_part, regular_e_part, validate_its_pathname, validate_rts_pathname, ItsPathname,
    RtsPathname, MAX_PART_BYTES, MAX_WRITE_PATHNAME_BYTES,
};
pub use pd::{PdArrays, PdInput};
pub use profile::{ProfileArrays, ProfileInput};
pub use variants::{PatternInput, TsNotes, TsVariants, MAX_ELEMENT_WORDS};

use crate::dss7::{Dss7Error, TsError};
use crate::lock::{LockError, LockPolicy, LockRefusal};

/// Program name stored in `kinfoProgram` (16 bytes, NUL-padded) of every record this
/// writer stores (P20 §2.3). HEC stores `"not specified"` (`Internal/zinit.c` line 1030).
pub const PROGRAM_NAME: &str = "fast-dss";

/// Version string stamped into new files: copied, with every other default, from an
/// empty file created by hecdss 0.1.33 (heclib `7-JA`; P20 §2.2).
pub const NEW_FILE_VERSION: &[u8; 4] = b"7-JA";

/// Errors from the writer.
#[derive(Debug)]
pub enum WriteError {
    Io(io::Error),
    /// The file is not a usable DSS7 file (bad header).
    Dss7(Dss7Error),
    /// An existing block could not be read for merging.
    Ts(TsError),
    /// The lock state could not be determined.
    Lock(LockError),
    /// HEC software holds the file (Python `DssLockedError`).
    Locked(LockRefusal),
    /// Invalid pathname (Python `ValueError`).
    BadPathname(String),
    /// Invalid values, times, quality or strings (Python `ValueError`).
    BadInput(String),
    /// A valid request outside P20's scope, or a file the writer does not modify
    /// (Python `DssUnsupportedRecordError` / `NotImplementedError`).
    Unsupported(String),
    /// `create_dss7` refuses to replace an existing file.
    AlreadyExists(PathBuf),
    /// Structures the write would modify are damaged (Python `DssFormatError`).
    Corrupt(String),
    /// A store refused after planning passed: a defect in the writer, not in the file. Planning
    /// runs the same code first, so only an I/O error should reach this point.
    Internal(String),
    /// Test hook: the write stopped after `step` of block `block` (crash simulation).
    InjectedFault {
        block: usize,
        step: WriteStep,
    },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Io(e) => write!(f, "I/O error while writing: {e}"),
            WriteError::Dss7(e) => write!(f, "{e}"),
            WriteError::Ts(e) => write!(f, "cannot read the existing block: {e}"),
            WriteError::Lock(e) => write!(f, "{e}"),
            WriteError::Locked(r) => write!(f, "{r}"),
            WriteError::BadPathname(m) => write!(f, "invalid pathname: {m}"),
            WriteError::BadInput(m) => write!(f, "invalid input: {m}"),
            WriteError::Unsupported(m) => write!(f, "not supported by the writer: {m}"),
            WriteError::AlreadyExists(p) => write!(f, "file already exists: {}", p.display()),
            WriteError::Corrupt(m) => write!(f, "refusing to write to a damaged file: {m}"),
            WriteError::Internal(m) => write!(f, "internal writer error after planning: {m}"),
            WriteError::InjectedFault { block, step } => {
                write!(
                    f,
                    "write stopped by the test hook after {step:?} of block {block}"
                )
            }
        }
    }
}

impl std::error::Error for WriteError {}

impl WriteError {
    /// For a store of a write of `records` records. A single record is not planned: its store
    /// refuses before its first write, and the refusal stands. After planning, I/O errors and
    /// the test hook stay as they are; a data-dependent refusal means planning missed it.
    pub(crate) fn after_planning(self, records: usize) -> WriteError {
        if records < 2 {
            return self;
        }
        match self {
            e @ (WriteError::Io(_) | WriteError::InjectedFault { .. }) => e,
            e => {
                debug_assert!(false, "refused after planning passed: {e}");
                WriteError::Internal(e.to_string())
            }
        }
    }
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

impl From<Dss7Error> for WriteError {
    fn from(e: Dss7Error) -> Self {
        WriteError::Dss7(e)
    }
}

/// Values to write, in the stored width: `F32` makes RTS (100) records, `F64` RTD (105).
#[derive(Debug, Clone, Copy)]
pub enum RtsValues<'a> {
    F32(&'a [f32]),
    F64(&'a [f64]),
}

impl RtsValues<'_> {
    pub fn len(&self) -> usize {
        match self {
            RtsValues::F32(v) => v.len(),
            RtsValues::F64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One regular-interval series to store.
#[derive(Debug, Clone)]
pub struct RtsInput<'a> {
    /// `/A/B/C/D/E/F/`; D may be empty or a date (it is replaced per block).
    pub pathname: &'a [u8],
    /// Time of the first value: whole seconds since 1899-12-31 00:00 (HEC julian 0);
    /// the 24:00 stamp of a day is the next day's 00:00. A start inside an interval
    /// gives a time offset, as HEC stores it.
    pub start: i64,
    /// Values, stored bits unchanged (−901, −902 and −FLT_MAX are HEC missing; NaN is a
    /// value).
    pub values: RtsValues<'a>,
    /// One quality word per value (`qualityElementSize` 1), or none.
    pub quality: Option<&'a [i32]>,
    pub units: &'a [u8],
    /// HEC data type (`INST-VAL`, `PER-AVER`, `PER-CUM`, `INST-CUM`, ...).
    pub data_type: &'a [u8],
    pub time_zone: Option<&'a [u8]>,
    /// Decimal places (`INT_HEAD_precision`); −1 = unset.
    pub precision: i32,
}

impl<'a> RtsInput<'a> {
    pub fn new(
        pathname: &'a [u8],
        start: i64,
        values: RtsValues<'a>,
        units: &'a [u8],
        data_type: &'a [u8],
    ) -> Self {
        RtsInput {
            pathname,
            start,
            values,
            quality: None,
            units,
            data_type,
            time_zone: None,
            precision: -1,
        }
    }
}

/// One irregular-interval series to store (P21).
#[derive(Debug, Clone)]
pub struct ItsInput<'a> {
    /// `/A/B/C/D/E/F/` with E one of `IR-Day`, `IR-Month`, `IR-Year`, `IR-Decade`,
    /// `IR-Century`; D may be empty or a date (it is replaced per block).
    pub pathname: &'a [u8],
    /// One time per value, strictly ascending: whole seconds since 1899-12-31 00:00
    /// (HEC julian 0), as [`RtsInput::start`]. A 00:00 time is 24:00 of the previous
    /// day and is stored in that day's block, as heclib stores it.
    pub times: &'a [i64],
    /// Values, stored bits unchanged; `F32` makes ITS (110) records, `F64` ITD (115).
    pub values: RtsValues<'a>,
    /// One quality word per value (`qualityElementSize` 1), or none.
    pub quality: Option<&'a [i32]>,
    pub units: &'a [u8],
    pub data_type: &'a [u8],
    pub time_zone: Option<&'a [u8]>,
    /// Decimal places (`INT_HEAD_precision`); −1 = unset.
    pub precision: i32,
    /// Seconds per time unit stamped in internal-header word 0 (1, 60, 3600 or 86400).
    /// `None` chooses 60 when every time is a whole minute (always for IR-Century),
    /// else 1. Every time must be a whole multiple of an explicit granularity.
    pub time_granularity: Option<i64>,
}

impl<'a> ItsInput<'a> {
    pub fn new(
        pathname: &'a [u8],
        times: &'a [i64],
        values: RtsValues<'a>,
        units: &'a [u8],
        data_type: &'a [u8],
    ) -> Self {
        ItsInput {
            pathname,
            times,
            values,
            quality: None,
            units,
            data_type,
            time_zone: None,
            precision: -1,
            time_granularity: None,
        }
    }
}

/// The ordered steps of one block transaction (P20 §4). A fault injected after a step
/// leaves the file as a process crash at that point would. Steps that do not apply to a
/// write (Chain without a new block, Retire for a new record, Link without a new bin)
/// write nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WriteStep {
    /// Record area (info block written inert, status MOVED) and any new bin block
    /// written at EOF.
    Data,
    /// End-of-file flag written at the new `kfileSize`; flushed.
    EndOfFile,
    /// Header `kfileSize` and the in-progress marker (word 29); flushed.
    Allocation,
    /// Bin-block chain word (new block only), after the space is reserved.
    Chain,
    /// Rewrite only: the old info block is marked MOVED (readers do not test it).
    Retire,
    /// Entry written (hash word last) or bin info address switched (rewrite). For an
    /// entry in an existing bin, or a rewrite, this makes the record visible.
    Publish,
    /// Header counters, statistics and bin allocation state (marker still set).
    Counters,
    /// Table slot or overflow link to a new bin, which already holds the entry; this
    /// makes a record in a new bin visible.
    Link,
    /// The new info block's status set to 1 (primary).
    Activate,
    /// The in-progress marker cleared.
    Finish,
}

impl WriteStep {
    pub const ALL: [WriteStep; 10] = [
        WriteStep::Data,
        WriteStep::EndOfFile,
        WriteStep::Allocation,
        WriteStep::Chain,
        WriteStep::Retire,
        WriteStep::Publish,
        WriteStep::Counters,
        WriteStep::Link,
        WriteStep::Activate,
        WriteStep::Finish,
    ];
}

/// Test hook: stop after `step` of the `block`-th block (0-based) of a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultPoint {
    pub block: usize,
    pub step: WriteStep,
}

/// Writer options.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Lock policy; default [`LockPolicy::default`], the policy the maintainer decided on
    /// 2026-10-28 (P19 REPORT): hold the lock while writing, refuse when HEC readers or
    /// writers hold the file, and write old-scheme files (readers undetectable) while
    /// holding the lock, reporting [`WriteReport::readers_undetected`].
    pub lock_policy: LockPolicy,
    /// `fsync` before publishing and at the end of each block (default true).
    pub sync: bool,
    /// Crash-simulation hook (tests only).
    pub fault: Option<FaultPoint>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            lock_policy: LockPolicy::default(),
            sync: true,
            fault: None,
        }
    }
}

/// What happened to one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOutcome {
    /// New record.
    Added,
    /// Existing record rewritten (new copy at EOF, old copy MOVED).
    Replaced,
    /// The stored record already equals the new one; nothing was written.
    Unchanged,
    /// Irregular writes only (P21): an existing block wholly inside the time window
    /// received no values and was deleted, as heclib's storage flag 1 deletes it.
    Deleted,
}

impl BlockOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockOutcome::Added => "added",
            BlockOutcome::Replaced => "replaced",
            BlockOutcome::Unchanged => "unchanged",
            BlockOutcome::Deleted => "deleted",
        }
    }
}

/// Result for one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockReport {
    /// Block pathname as stored.
    pub pathname: Vec<u8>,
    pub outcome: BlockOutcome,
    /// Record type written (100 or 105), or 0 when nothing was written.
    pub record_type: i32,
    /// Info address of the record now live (0 when none).
    pub info_address: i64,
    /// `kinfoNumberData` / `kinfoLogicalNumber` of the record now live.
    pub number_data: i64,
    pub logical_number: i64,
}

/// Result of [`write_rts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReport {
    pub blocks: Vec<BlockReport>,
    /// The lock guard passed although readers cannot be detected (only when the policy
    /// allows undetectable readers): callers should warn.
    pub readers_undetected: bool,
    /// The file carried the marker of an interrupted native write, which was
    /// reconciled before this write (bin allocation state, pending info status,
    /// `knumberRecords`, `khashsUsed`).
    pub reconciled_interrupted_write: bool,
    /// How the write lock was held (`whole_file_lockfileex`, `fcntl_setlk`, or `none`
    /// when the policy does not hold it).
    pub lock_method: &'static str,
}

/// Words of a new, empty DSS7 file (P20 §2.2): the hecdss 0.1.33 empty-file defaults
/// with `kcreateDate` = `klastWriteTime` = `now_ms`.
pub fn new_file_words(now_ms: i64) -> Vec<i64> {
    engine::new_file_words(now_ms)
}

/// Create an empty DSS7 file at `path`. Refuses an existing path
/// ([`WriteError::AlreadyExists`]); the file is written in full and synced before
/// returning.
pub fn create_dss7(path: &Path) -> Result<(), WriteError> {
    let words = engine::new_file_words(engine::now_ms());
    let mut f = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(WriteError::AlreadyExists(path.to_path_buf()))
        }
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::with_capacity(words.len() * 8);
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Store a regular-interval series in the existing DSS7 file at `path` (P20 §3–§4).
pub fn write_rts(
    path: &Path,
    input: &RtsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_planned(&file, input, &plan, opts)
}

/// Same as [`write_rts`] on an already open read-write handle. The lock is tested and
/// held on this handle.
pub fn write_rts_to(
    file: &File,
    input: &RtsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan(input)?;
    engine::write_planned(file, input, &plan, opts)
}

/// Validate an input without touching any file (pathname, times, values, strings).
pub fn validate_rts_input(input: &RtsInput) -> Result<(), WriteError> {
    engine::plan(input).map(|_| ())
}

/// The time stamps (whole HEC seconds, see [`RtsInput::start`]) the writer assigns to
/// `n` values of `pathname` starting at `start`: `incrementTime` from the start, as
/// `ztsRegStoreBlock()` places values (lines 320-327). Validates like a write.
pub fn value_times(pathname: &[u8], start: i64, n: usize) -> Result<Vec<i64>, WriteError> {
    engine::value_times(pathname, start, n)
}

/// Store an irregular-interval series in the existing DSS7 file at `path` (P21 §3–§4).
pub fn write_its(
    path: &Path,
    input: &ItsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_its(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_its_planned(&file, input, &plan, opts)
}

/// Same as [`write_its`] on an already open read-write handle. The lock is tested and
/// held on this handle.
pub fn write_its_to(
    file: &File,
    input: &ItsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_its(input)?;
    engine::write_its_planned(file, input, &plan, opts)
}

/// Validate an irregular input without touching any file (pathname, times, values,
/// granularity, strings). Returns the granularity chosen for the input (a block whose
/// kept stored times are not whole multiples of an automatic choice uses seconds).
pub fn validate_its_input(input: &ItsInput) -> Result<i64, WriteError> {
    engine::plan_its(input).map(|p| p.granularity)
}

/// Store one paired-data record, PD (200) or PDD (205) as the arrays' width, in the
/// existing DSS7 file at `path` (P23): a new record, or a full replacement of an existing
/// paired-data record (`Public/zpdStore.c:zpdStore()`, full-record branch). An existing
/// record of another type is refused (heclib would delete it).
pub fn write_pd(
    path: &Path,
    input: &PdInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_pd(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_pd_planned(&file, &plan, opts)
}

/// Same as [`write_pd`] on an already open read-write handle.
pub fn write_pd_to(
    file: &File,
    input: &PdInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_pd(input)?;
    engine::write_pd_planned(file, &plan, opts)
}

/// Validate a paired-data input for DSS7 without touching any file.
pub fn validate_pd_input(input: &PdInput) -> Result<(), WriteError> {
    engine::plan_pd(input).map(|_| ())
}

/// P23 review M1: refuse a `|` in an elevation-side unit of an ELEV paired-data record
/// (heclib parses it as a unit specification with a vertical datum). Returns whether the C
/// part names an ELEV parameter (`pathnameIsElevPd()`). Shared with the DSS6 writer.
pub(crate) fn pd_elev_units(input: &PdInput) -> Result<bool, WriteError> {
    engine::check_elev_units(input)
}

// ---------------------------------------------------------------------------------
// P24: grids (create, add, replace). Specification: packets/P24_grid_writer.md.
// ---------------------------------------------------------------------------------

pub use crate::grid_write::{
    grid_statistics, AlbersParameters, GridCompression, GridInput, GridPlan, GridProjection,
    GridStatistics, GridWriteError, SpecifiedReference,
};

impl From<crate::grid_write::GridWriteError> for WriteError {
    fn from(e: crate::grid_write::GridWriteError) -> Self {
        use crate::grid_write::GridWriteError as G;
        match e {
            G::BadPathname(m) => WriteError::BadPathname(m),
            G::BadInput(m) => WriteError::BadInput(m),
            G::Unsupported(m) => WriteError::Unsupported(m),
        }
    }
}

/// Validate a grid for a DSS7 write without touching any file; returns the plan (stored
/// pathname, record type, times and the statistics that will be stored).
pub fn validate_grid_input(input: &GridInput) -> Result<GridPlan, WriteError> {
    Ok(crate::grid_write::plan_grid(input, 7)?)
}

/// Store one grid record in the existing DSS7 file at `path` (P24): a new record, or a
/// replacement of a grid record of the same type. Every check runs before the first
/// write; a refused write leaves the file unchanged.
pub fn write_grid(
    path: &Path,
    input: &GridInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = validate_grid_input(input)?;
    let areas = grid::areas(input, &plan);
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    write_grid_planned(&file, &plan, &areas, opts)
}

/// Same as [`write_grid`] on an already open read-write handle.
pub fn write_grid_to(
    file: &File,
    input: &GridInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = validate_grid_input(input)?;
    let areas = grid::areas(input, &plan);
    write_grid_planned(file, &plan, &areas, opts)
}

fn write_grid_planned(
    file: &File,
    plan: &GridPlan,
    areas: &engine::Areas,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let want = plan.record_type;
    // zwriteExisting(): a record keeps its data type (dss7.md §5.3); the writer replaces
    // only a grid of the same record type.
    let check = |found: Option<&crate::dss7::RecordRef>| -> Result<(), WriteError> {
        match found {
            Some(r) if r.info.record_type != want => Err(WriteError::Unsupported(format!(
                "the existing record is type {}, not grid type {want}; the writer does not \
                 change a record's type",
                r.info.record_type
            ))),
            _ => Ok(()),
        }
    };
    engine::write_areas(file, &plan.pathname, areas, &check, opts)
}

// ---------------------------------------------------------------------------------
// P38: time-series variants. Specification: packets/P38_ts_writer_variants.md.
// ---------------------------------------------------------------------------------

/// [`write_rts`] with multi-word quality or notes.
pub fn write_rts_variants(
    path: &Path,
    input: &RtsInput,
    variants: &TsVariants,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan(input)?;
    engine::check_rts_variants(input, variants)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_rts_v(&file, input, &plan, variants, opts)
}

/// [`write_its`] with multi-word quality or notes.
pub fn write_its_variants(
    path: &Path,
    input: &ItsInput,
    variants: &TsVariants,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_its(input)?;
    engine::check_its_variants(input, variants)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_its_v(&file, input, &plan, variants, opts)
}

/// Store one pattern record (`SemiPublic/ztsStorePattern.c:ztsStorePattern()`): a new
/// record, or a replacement of a pattern of the same type.
pub fn write_pattern(
    path: &Path,
    input: &PatternInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_pattern(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_pattern_planned(&file, &plan, opts)
}

/// Validate a pattern input without touching any file.
pub fn validate_pattern_input(input: &PatternInput) -> Result<(), WriteError> {
    engine::plan_pattern(input).map(|_| ())
}

// ---------------------------------------------------------------------------------
// P39: profiles and locations. Specification: packets/P39_profile_location_writers.md.
// ---------------------------------------------------------------------------------

/// Store a regular-interval profile, RTS-PROFILE (102) or RTD-PROFILE (107) as the arrays'
/// width, in the existing DSS7 file at `path` (`Public/ztsStore.c`, storage flag 0).
pub fn write_profile(
    path: &Path,
    input: &ProfileInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_profile(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_profile_planned(&file, input, &plan, opts)
}

/// Same as [`write_profile`] on an already open read-write handle.
pub fn write_profile_to(
    file: &File,
    input: &ProfileInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_profile(input)?;
    engine::write_profile_planned(file, input, &plan, opts)
}

/// Validate a profile input without touching any file.
pub fn validate_profile_input(input: &ProfileInput) -> Result<(), WriteError> {
    engine::plan_profile(input).map(|_| ())
}

/// Store the location record `/A/B/Location Info////` of `input.pathname` in the existing
/// DSS7 file at `path` (`Public/zlocationStore.c`, storage flag 1).
pub fn write_location(
    path: &Path,
    input: &LocationInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let lp = engine::plan_location(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_location_planned(&file, &lp, input, opts)
}

/// Same as [`write_location`] on an already open read-write handle.
pub fn write_location_to(
    file: &File,
    input: &LocationInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let lp = engine::plan_location(input)?;
    engine::write_location_planned(file, &lp, input, opts)
}

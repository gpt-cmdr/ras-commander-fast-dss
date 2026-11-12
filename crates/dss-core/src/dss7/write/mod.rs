//! DSS7 writer: create a file, add regular-interval time series (float RTS
//! 100, double RTD 105) and replace or merge values in existing blocks with HEC's
//! default store semantics. Format and write order: `docs/format/dss7.md` (§6.4).
//!
//! The writer also stores irregular-interval time series (float ITS 110, double ITD 115;
//! [`write_its`]) with HEC's "replace" store semantics (storage flag 1, what hecdss
//! uses): inside the new time window the new values replace what is stored, the rest of
//! each block is kept, and blocks wholly inside the window that receive no values are
//! deleted.
//!
//! Every write:
//!
//! 1. validates the input (pathname rules in `path.rs`; refusal, never truncation);
//! 2. tests HEC's lock words on its own handle and applies an explicit
//!    [`LockPolicy`] ([`crate::lock::ensure_unlocked_for_write`]);
//! 3. holds the write lock while writing (`lockhold`);
//! 4. writes block by block, each block as one transaction ordered "data first,
//!    pointers last" (`dss7.md` §6.4, [`WriteStep`]).
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
// Grid records.
mod grid;
pub(crate) mod lockhold;
pub(crate) mod overlay;
mod path;
pub mod pd;
// Multi-word quality, notes and patterns.
mod variants;
// Profile and location records.
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
/// writer stores. HEC stores `"not specified"` (`Internal/zinit.c` line 1030).
pub const PROGRAM_NAME: &str = "fast-dss";

/// Version string stamped into new files: copied, with every other default, from an
/// empty file created by hecdss 0.1.33 (heclib `7-JA`).
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
    /// A valid request outside the writer's scope, or a file the writer does not modify
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

/// One irregular-interval series to store.
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

/// The ordered steps of one block transaction (`dss7.md` §6.4). A fault injected after a step
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

/// Killed-process test hook, not a supported API.
///
/// When installed, both write engines (DSS7 and DSS6) call it at every ordering boundary
/// of a write that is not a planning pass: with `Some(step)` after each [`WriteStep`]
/// (where [`FaultPoint`] stops) and with `None` at each flush point inside a step (for
/// example between an entry body and its publication word). It is called in both sync
/// modes, before any flush, after all preceding writes have returned. A test child
/// process installs a hook that blocks at the n-th call, so the parent can kill it there
/// with the file open and the write lock held. Install it at most once per process.
#[doc(hidden)]
pub type BarrierHook = fn(block: usize, step: Option<WriteStep>);

static BARRIER_HOOK: std::sync::OnceLock<BarrierHook> = std::sync::OnceLock::new();

/// Install the [`BarrierHook`] for this process (tests only). Returns `false` if one is
/// already installed.
#[doc(hidden)]
pub fn set_barrier_hook(hook: BarrierHook) -> bool {
    BARRIER_HOOK.set(hook).is_ok()
}

#[inline]
pub(crate) fn barrier(block: usize, step: Option<WriteStep>) {
    if let Some(h) = BARRIER_HOOK.get() {
        h(block, step);
    }
}

static WRITE_BARRIERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Also call the [`BarrierHook`] (with `None`) immediately
/// before every separate file write of both engines, so a killed-process test can stop
/// between any two writes, not only at flush points and step boundaries. Off by default,
/// so the numbering of the coarse barriers is unchanged.
#[doc(hidden)]
pub fn set_write_barriers(on: bool) {
    WRITE_BARRIERS.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Called before each separate `write` of a write session (both engines).
#[inline]
pub(crate) fn write_barrier(block: usize) {
    if WRITE_BARRIERS.load(std::sync::atomic::Ordering::Relaxed) {
        barrier(block, None);
    }
}

/// Test hook: stop after `step` of the `block`-th block (0-based) of a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultPoint {
    pub block: usize,
    pub step: WriteStep,
}

/// When a write forces its data to storage (`fsync`). [`SyncMode::End`] is the default
/// since 0.1.2; up to 0.1.1 the default was what is now [`SyncMode::Step`].
///
/// The three modes issue **the same `write` calls in the same order** (data first,
/// pointers last, with the in-progress marker), through unbuffered writes that reach the
/// operating system as they are made. They differ only in when `fsync` is called. A write
/// interrupted by **process death** therefore leaves the same on-disk states in every
/// mode, and the next write reconciles it the same way (the crash model of
/// `docs/format/dss7.md` and `dss6.md`; killed-process matrix `tests/p64_kill.rs`, run in
/// all three modes). **No power-loss claim is made in any mode**: after a power loss or an operating-system crash, parts of an
/// interrupted write, and in [`SyncMode::None`] of earlier completed writes, may be
/// missing, torn or out of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// No flush at all. Nothing is forced to storage by the writer.
    None,
    /// One flush (`sync_all`) at the end of the write call, before the lock is released
    /// and the call returns; none between the steps of the transaction. The default.
    ///
    /// A caller that stores several records in one public call (the gridded importer,
    /// the grid tools, grid-to-point) flushes once after its last record; see
    /// [`SyncMode::batched`].
    #[default]
    End,
    /// A flush (`sync_data`) at every flush point of the transaction, then one
    /// `sync_all` at the end: the default up to 0.1.1. The flush points are: the record
    /// area together with the end-of-file flag (the [`WriteStep::Data`] step has no flush
    /// of its own), the header allocation and marker, the bin-block chain word, a bin
    /// entry's body before the word that publishes it and that word itself, the header
    /// counters, the link, the activation, and the cleared marker. A step with nothing to
    /// write flushes nothing. Reconciliation of an interrupted write flushes its own
    /// repairs. This is a description of the flush points, not a flush per [`WriteStep`].
    Step,
}

impl SyncMode {
    /// The public names: `"none"`, `"end"`, `"step"`.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncMode::None => "none",
            SyncMode::End => "end",
            SyncMode::Step => "step",
        }
    }

    /// Parse a public name (`"none"`, `"end"`, `"step"`).
    pub fn from_name(name: &str) -> Option<SyncMode> {
        match name {
            "none" => Some(SyncMode::None),
            "end" => Some(SyncMode::End),
            "step" => Some(SyncMode::Step),
            _ => None,
        }
    }

    /// True when the transaction flushes at each of its flush points.
    #[inline]
    pub fn each_step(self) -> bool {
        self == SyncMode::Step
    }

    /// True when the write call ends with a `sync_all`.
    #[inline]
    pub fn at_end(self) -> bool {
        self != SyncMode::None
    }

    /// For a public call that stores several records, each through its own write call:
    /// the mode to give each record, and whether the caller must call [`sync_path`] once
    /// after the last one. [`SyncMode::End`] becomes "no flush per record, one at the
    /// end", so the call as a whole keeps the meaning of `End`; the other two modes pass
    /// through.
    pub fn batched(self) -> (SyncMode, bool) {
        match self {
            SyncMode::End => (SyncMode::None, true),
            other => (other, false),
        }
    }
}

static FLUSHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called by both engines immediately before each `sync_data` / `sync_all`.
#[inline]
pub(crate) fn count_flush() {
    FLUSHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Test and measurement hook, not a supported API: the number of flushes
/// (`sync_data` and `sync_all` calls) the writers of this process have made so far, in
/// every thread. The tests of the flush policies read it.
#[doc(hidden)]
pub fn flush_count() -> u64 {
    FLUSHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// `i64::MIN`: no fixed clock.
static CLOCK_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);

/// Test hook, not a supported API: fix the time, in milliseconds since
/// 1970-01-01 UTC, that the writers of this process stamp on files and records (DSS7 last-write
/// times, DSS6 write date and time), or `None` for the system clock again. Two runs that
/// should write the same file, such as a gridded import with one worker and with all cores,
/// can then be compared byte for byte.
#[doc(hidden)]
pub fn set_write_clock(ms: Option<i64>) {
    CLOCK_MS.store(ms.unwrap_or(i64::MIN), std::sync::atomic::Ordering::Relaxed);
}

/// The fixed clock of [`set_write_clock`], if set.
pub(crate) fn write_clock() -> Option<i64> {
    match CLOCK_MS.load(std::sync::atomic::Ordering::Relaxed) {
        i64::MIN => None,
        ms => Some(ms),
    }
}

/// Force everything written to the file at `path` to storage (`sync_all`): the single
/// flush of a multi-record call in [`SyncMode::End`] (see [`SyncMode::batched`]). It
/// writes nothing and touches no lock word. A path that does not exist is not an error.
pub fn sync_path(path: &Path) -> io::Result<()> {
    // Windows `FlushFileBuffers` needs write access to the handle.
    match OpenOptions::new().write(true).open(path) {
        Ok(f) => {
            count_flush();
            f.sync_all()
        }
        // A call that stored nothing may not have created the file.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writer options.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Lock policy; default [`LockPolicy::default`], the policy the maintainer decided on
    /// 2026-10-28: hold the lock while writing, refuse when HEC readers or
    /// writers hold the file, and write old-scheme files (readers undetectable) while
    /// holding the lock, reporting [`WriteReport::readers_undetected`].
    pub lock_policy: LockPolicy,
    /// Flush policy; default [`SyncMode::End`]. See [`SyncMode`].
    pub sync: SyncMode,
    /// Crash-simulation hook (tests only).
    pub fault: Option<FaultPoint>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            lock_policy: LockPolicy::default(),
            sync: SyncMode::End,
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
    /// Irregular writes only: an existing block wholly inside the time window
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

/// Result of [`repair_dss7`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    /// Records copied to the new file and verified bit for bit.
    pub records_copied: usize,
    /// Of those, records the source's catalog did not list (reachable only through hash
    /// lookups or the recovery catalog, for example behind a hole left by finding F1, `dss7.md` §6.4).
    pub hidden_records_copied: usize,
    /// Records found but not copied, with the reason (unreadable, implausible pathname).
    pub not_copied: Vec<(Vec<u8>, String)>,
    /// Alias entries (status 2) whose primary record was copied (under the primary's
    /// pathname). An alias whose primary could not be copied is in `not_copied`.
    pub aliases_skipped: usize,
    /// Further live copies of a pathname already copied.
    pub duplicates_skipped: usize,
    /// Records copied from a copy other than the preferred one:
    /// pathname, word address of the source info block actually copied, and why the
    /// preferred copy (the hash lookup's, or the primary entry) was not used.
    pub fallback_copies: Vec<(Vec<u8>, i64, String)>,
    /// Records written under a pathname other than the catalog spelling that found them:
    /// `(found as, written as, "alias" | "case variant")`. A record is
    /// written under its own identity, the pathname its info block stores (an alias's
    /// primary).
    pub variants_copied: Vec<(Vec<u8>, Vec<u8>, String)>,
    /// One entry for every source candidate found. That is each
    /// distinct info block that a pathname's lookup or catalog entries reach, each alias
    /// entry, and an entry without an address for a pathname refused as a whole. Each
    /// entry has exactly one outcome. `not_copied` is derived from it, and
    /// [`RepairReport::complete`] is true exactly when no entry is
    /// [`CandidateOutcome::NotCopied`].
    pub ledger: Vec<Candidate>,
    /// Live records the source's ordinary catalog lists.
    pub src_listed_records: usize,
    /// The source carried the marker of an interrupted native write (not reconciled:
    /// the source is only read; records are copied as a reader sees them).
    pub src_interrupted_write: bool,
}

impl RepairReport {
    /// Every readable record that the discovery procedure reaches
    /// ([`crate::dss7::Dss7File::discover`]) was copied and verified, or equals one that
    /// was, or is listed. Formally: no [`Candidate`] of [`RepairReport::ledger`] is
    /// [`CandidateOutcome::NotCopied`]. Pointer targets that
    /// discovery cannot classify are `NotCopied` too. Bytes that no pointer reaches are
    /// out of scope, and so are bins of a bin block whose declared span does not fit the
    /// file (that block itself is `NotCopied` as unclassified). A copy that is not complete
    /// is still published: check this and [`RepairReport::not_copied`].
    pub fn complete(&self) -> bool {
        !self
            .ledger
            .iter()
            .any(|c| matches!(c.outcome, CandidateOutcome::NotCopied(_)))
    }
}

/// One source candidate of [`repair_dss7`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The pathname that found it: the catalog spelling of its pathname group, or the
    /// alias entry's own pathname.
    pub pathname: Vec<u8>,
    /// Word address of its info block; `None` for a pathname refused as a whole
    /// (implausible pathname, pre-7-HA time series, no copy found at all).
    pub info_address: Option<i64>,
    /// Found through an alias entry (status 2).
    pub alias: bool,
    pub outcome: CandidateOutcome,
}

impl Candidate {
    pub(crate) fn new(
        pathname: &[u8],
        info_address: Option<i64>,
        alias: bool,
        outcome: CandidateOutcome,
    ) -> Self {
        Candidate {
            pathname: pathname.to_vec(),
            info_address,
            alias,
            outcome,
        }
    }
}

/// The one outcome of a [`Candidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOutcome {
    /// Written to the new file and verified.
    Copied,
    /// Not written: its payload (all six areas, type, counts, dates) equals the copy
    /// taken from the info block at `copied_info`.
    EqualToCopied { copied_info: i64 },
    /// Not readable; its pathname was copied from the candidate at `copied_info`.
    UnreadableCoveredBy { copied_info: i64 },
    /// Not copied, with the reason. Listed in [`RepairReport::not_copied`].
    NotCopied(String),
}

thread_local! {
    static COPY_OUT_FAULT: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Steps of [`repair_dss7`] at which [`set_copy_out_fault`] can inject an error (tests
/// only). `hard_link` makes publication's hard link fail as on a
/// filesystem without hard links; `hard_link_race` also creates `dest` first.
#[doc(hidden)]
pub const COPY_OUT_FAULT_STEPS: &[&str] = &[
    "create",
    "write_header",
    "open",
    "commit",
    "finish",
    "sync",
    "verify",
    "catalog",
    "hard_link",
    "hard_link_race",
];

/// Inject an error at `step` of [`repair_dss7`] calls on this thread (tests only).
#[doc(hidden)]
pub fn set_copy_out_fault(step: Option<&'static str>) {
    COPY_OUT_FAULT.with(|c| c.set(step));
}

pub(crate) fn copy_out_fault(step: &'static str) -> Result<(), WriteError> {
    if COPY_OUT_FAULT.with(|c| c.get()) == Some(step) {
        return Err(WriteError::Io(io::Error::other(format!(
            "injected fault at {step}"
        ))));
    }
    Ok(())
}

pub(crate) fn copy_out_fault_is(steps: &[&'static str]) -> Option<&'static str> {
    COPY_OUT_FAULT
        .with(|c| c.get())
        .filter(|s| steps.contains(s))
}

/// Words of a new, empty DSS7 file: the hecdss 0.1.33 empty-file defaults
/// with `kcreateDate` = `klastWriteTime` = `now_ms`.
pub fn new_file_words(now_ms: i64) -> Vec<i64> {
    engine::new_file_words(now_ms)
}

/// Create an empty DSS7 file at `path`. Refuses an existing path
/// ([`WriteError::AlreadyExists`]); the file is written in full and synced before
/// returning.
pub fn create_dss7(path: &Path) -> Result<(), WriteError> {
    create_dss7_with(path, SyncMode::End)
}

/// [`create_dss7`] with a flush policy: [`SyncMode::None`] skips the `sync_all` (the file
/// is still written in full by one `write`); the other two modes flush once, as
/// `create_dss7` does.
pub fn create_dss7_with(path: &Path, sync: SyncMode) -> Result<(), WriteError> {
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
    if sync.at_end() {
        count_flush();
        f.sync_all()?;
    }
    Ok(())
}

/// Copy the readable records that discovery reaches in the DSS7 file `src` into a new
/// DSS7 file `dest`.
///
/// This is the safe path for a damaged file. In place, reconciliation of an interrupted
/// write guarantees exact recovery only for a file that was consistent before the write
/// (ordinary process death); extra, independent damage is refused when cheap bounded
/// checks detect it, without a promise to detect every case (`dss7.md`, threat model;
/// documented limitations R7 and R12).
///
/// `src` is opened read-only and never written. The records are found by one
/// procedure, [`crate::dss7::Dss7File::discover`]. It starts from
/// every bin of the bin-block chain and every hash-table slot, follows the overflow
/// word of every bin reached (empty bins included), and is bounded. The records are
/// then read with the normal reader. Pointer targets it cannot classify are listed as
/// not copied; bytes that no pointer reaches are out of scope. A bin block whose
/// declared span does not fit the file is reported as unclassified and its bins are not
/// searched, unlike the catalog walk, which salvages whole bins from a partial block;
/// such records can be readable through the catalog and still not be copied. For each pathname the
/// lookup's record is tried first, then every other copy that discovery found; the
/// first that reads in full is copied ([`RepairReport::fallback_copies`] names a copy
/// other than the lookup's). An alias counts as covered only when its primary record is
/// copied; otherwise the primary is copied through it or the alias is reported.
///
/// Each record's six data areas, record type, dates, value and logical counts are
/// written, unchanged, with the normal writer's record transaction into a new file; the
/// info block's write time, creation time, program name and version count are the new
/// write's. The new file is then read back: every copied record must be bit-for-bit
/// equal and decode as the copied source record does (raw comparison only for kinds
/// without a decoder or records neither reader decodes), and the new file must catalog
/// without anomalies, or nothing is published.
///
/// The new file is built under a temporary name next to `dest`
/// (`.<name>.<pid>.<nanos>.partial`) and published at `dest` once every record selected
/// for copying has been written and verified, never replacing anything: by a hard link, else by an atomic no-replace
/// rename where one exists (Linux), else the call fails naming the kept, verified copy.
/// `dest` must not exist ([`WriteError::AlreadyExists`]). Every other returned error
/// removes the temporary file. A process killed before publication leaves `src`
/// unchanged, no `dest`, and the temporary file, which can be deleted; one killed just
/// after the hard link leaves the verified `dest` and the temporary name.
///
/// Each record is written and verified under its own identity, the pathname its info
/// block stores (an alias's primary); [`RepairReport::variants_copied`] records an alias
/// or case variant of the catalog spelling. Any other change of identity is refused: a
/// primary whose info block names another record is not copied under that name, and a
/// duplicate is skipped only when its payload is equal; otherwise the
/// pathname is in [`RepairReport::not_copied`].
///
/// **A partial recovery is published**. `Ok` means the selected
/// records were copied and verified and `dest` was published. It does **not** mean that
/// everything found was copied: records that could not be copied, or that could not be
/// kept under one pathname (a readable duplicate or identity conflict with a different
/// payload), are listed in [`RepairReport::not_copied`]. In that case
/// [`RepairReport::complete`] is `false` and `dest` is still published. Callers must
/// check `complete()` and `not_copied`. Every source candidate's outcome is in
/// [`RepairReport::ledger`].
///
/// Whether hidden records come back is reported, not guaranteed
/// ([`RepairReport::hidden_records_copied`]). A complete copy says only that what the
/// readers could find was copied and verified; it is not an integrity certificate for
/// `src`.
///
/// Flushes as [`SyncMode::End`]; see [`repair_dss7_with`].
pub fn repair_dss7(src: &Path, dest: &Path) -> Result<RepairReport, WriteError> {
    repair_dss7_with(src, dest, SyncMode::End)
}

/// [`repair_dss7`] with a flush policy. The new file is
/// built under a temporary name that no reader can see until it is published, so a flush
/// between its steps orders nothing a reader can observe: [`SyncMode::End`] (the default)
/// and [`SyncMode::Step`] both make **one** `sync_all` of the new file after its last
/// record and before it is verified and published; [`SyncMode::None`] makes none. In
/// every mode the file is published only after every selected record was written and
/// verified, so a process killed at any point leaves `src` unchanged and either no `dest`
/// or the complete verified `dest`. No power-loss claim; with `None` a
/// published `dest` may also be incomplete after a power loss.
pub fn repair_dss7_with(
    src: &Path,
    dest: &Path,
    sync: SyncMode,
) -> Result<RepairReport, WriteError> {
    engine::copy_out(src, dest, sync)
}

/// Store a regular-interval series in the existing DSS7 file at `path`.
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

/// Store an irregular-interval series in the existing DSS7 file at `path`.
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
/// existing DSS7 file at `path`: a new record, or a full replacement of an existing
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

/// Refuse a `|` in an elevation-side unit of an ELEV paired-data record
/// (heclib parses it as a unit specification with a vertical datum). Returns whether the C
/// part names an ELEV parameter (`pathnameIsElevPd()`). Shared with the DSS6 writer.
pub(crate) fn pd_elev_units(input: &PdInput) -> Result<bool, WriteError> {
    engine::check_elev_units(input)
}

// ---------------------------------------------------------------------------------
// Grids (create, add, replace).
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

/// Store one grid record in the existing DSS7 file at `path`: a new record, or a
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
// Time-series variants (notes, multi-word quality, patterns).
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

/// Same as [`write_rts_variants`] on an already open read-write handle (a `DssFile` session's held handle).
pub fn write_rts_variants_to(
    file: &File,
    input: &RtsInput,
    variants: &TsVariants,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan(input)?;
    engine::check_rts_variants(input, variants)?;
    engine::write_rts_v(file, input, &plan, variants, opts)
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

/// Same as [`write_its_variants`] on an already open read-write handle (a `DssFile` session's held handle).
pub fn write_its_variants_to(
    file: &File,
    input: &ItsInput,
    variants: &TsVariants,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_its(input)?;
    engine::check_its_variants(input, variants)?;
    engine::write_its_v(file, input, &plan, variants, opts)
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

/// Same as [`write_pattern`] on an already open read-write handle (a `DssFile` session's held handle).
pub fn write_pattern_to(
    file: &File,
    input: &PatternInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = engine::plan_pattern(input)?;
    engine::write_pattern_planned(file, &plan, opts)
}

/// Validate a pattern input without touching any file.
pub fn validate_pattern_input(input: &PatternInput) -> Result<(), WriteError> {
    engine::plan_pattern(input).map(|_| ())
}

// ---------------------------------------------------------------------------------
// Profiles and locations.
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

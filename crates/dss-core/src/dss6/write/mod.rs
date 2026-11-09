//! DSS6 writer: create a DSS6 file and add, replace or merge regular (RTS 100,
//! RTD 105) and irregular (ITS 110, ITD 115) time series with HEC's store semantics.
//! Format: `docs/format/dss6.md`
//! (§9.3 for the native write ordering).
//!
//! Built on the DSS7 writer ([`crate::dss7::write`]): the same input, option, report
//! and error types, the same lock guard and held write lock (`LockPolicy::default()`), the
//! same fault-injection hook ([`FaultPoint`], [`WriteStep`]) and the same rule of refusing
//! rather than truncating. What differs is the DSS6 layout (127-word records with a pad,
//! a hash table of bin addresses, 127-word bins in blocks, info block flag -9753) and the
//! DSS6 write ordering ([`DSS6_STEP_ORDER`]).
//!
//! Every write:
//!
//! 1. validates the input (pathname, times, values, strings; refusal, never truncation);
//! 2. tests HEC's DSS6 lock words on its own handle and applies the [`crate::lock::LockPolicy`];
//! 3. holds HEC's write lock while writing (`lockhold`, `KLOCKB`);
//! 4. reconciles a file left with the in-progress marker by an interrupted native write;
//! 5. writes block by block, each block one transaction ordered "data first, pointers
//!    last" ([`DSS6_STEP_ORDER`]).
//!
//! Format rules cite `heclib/heclib_f/src/` of `hec-dss` tag `7-IU-15` (the DSS6 Fortran
//! that HEC Monolith 3.3.27's `javaHeclib` carries): `zopen6Int.f` (file creation),
//! `zsrtsi6.f`/`zmovbk6.f` (regular store), `zsitsi6.f`/`zircpy6.f`/`zirdow6.f`/`zirbeg6.f`
//! (irregular store), `zwritex6.f`/`znwrit6.f`/`zowrit6.f`/`znwbin6.f`/`zcheck6.f`
//! (records, bins, hash table), `zgetag6.f` (tags).
//!
//! ```no_run
//! use dss_core::dss6::write::{create_dss6, write_rts6};
//! use dss_core::dss7::write::{RtsInput, RtsValues, WriteOptions};
//! create_dss6("new6.dss".as_ref())?;
//! let v = [1.0f32, 2.0, 3.0];
//! let input = RtsInput::new(b"/BASIN/LOC/FLOW//1HOUR/SIM/", 43_831 * 86_400 + 3_600,
//!                           RtsValues::F32(&v), b"CFS", b"INST-VAL");
//! let report = write_rts6("new6.dss".as_ref(), &input, &WriteOptions::default())?;
//! # Ok::<(), dss_core::dss7::write::WriteError>(())
//! ```

mod check;
mod engine;
// Grid records.
mod grid;
mod io;
mod its;
// Location data of time-series records.
mod location;
mod path;
// Pattern records.
mod pattern;
mod pd;
mod rts;

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;

pub use crate::dss7::write::{
    BlockOutcome, BlockReport, FaultPoint, RtsInput, RtsValues, WriteError, WriteOptions,
    WriteReport, WriteStep,
};
pub use crate::dss7::write::{PatternInput, PdArrays, PdInput};
pub use check::{check_file6, CheckReport};
pub use location::{write_location6, write_location6_to, MAX_SUPPLEMENTAL_BYTES6};
pub use path::{irregular_e_part6, regular_e_part6, MAX_UNITS_TYPE_CHARS};
pub use pd::{MAX_CURVES6, MAX_LABEL_CHARS6};

/// Program name stored in `KIPROG` (6 characters, `zinit6.f` line 286 `NPROGC = 6`) of
/// every record this writer stores. HEC's Fortran default is `Undefi` (`zinit6.f` line
/// 906, `DATA CPROG / 'Undefi'/`).
pub const PROGRAM_NAME6: &[u8; 6] = b"FASTDS";

/// Version string of new files: the one HEC Monolith 3.3.27 (`javaHeclib` 7-IU) writes
/// after `Hec_zset("DSSV", "", 6)` (`zinit6.f:ZBKDAT` line 855).
pub const NEW_FILE_VERSION6: &[u8; 4] = b"6-YO";

/// The order in which a DSS6 block transaction applies the [`WriteStep`]s (dss6.md §9.3).
/// [`WriteStep::Activate`] does not apply to DSS6 (HEC never tests an info block's status
/// on read, `zrdinf6.f`; the info block is written live in the Data step) and writes
/// nothing.
///
/// - **Data**: the record (info block, internal header, data) and any new zeroed bin block,
///   all at or after `KFSIZE` (so nothing reaches them yet).
/// - **EndOfFile**: `-9630` at the new `KFSIZE`, rest of that record zero.
/// - **Allocation**: the header with the new `KFSIZE` and the in-progress marker (header
///   words 67/68, `KRES`).
/// - **Chain**: (new bin block only) word 127 of the last bin of the previous block.
/// - **Counters**: header counters and bin allocation state (`KNRECS`, `KSEQNO`, `KBNREM`,
///   `KANBIN`, `KBINS`, `KBOVER`, `KHUSED`, `KDEAD`, ...), marker still set. `KNRECS`
///   goes up before the entry exists: HEC catalogs stop after `KNRECS` live entries
///   (`zcatfi6.f` line 196), so one high still lists every record and one low would hide
///   one. HEC's file checks (`zckpnb6.f` line 321, `zcklnk6.f` line 167, `zckpat6.f`
///   line 173) report a count one high as an inconsistency until the next native write
///   reconciles it.
/// - **Link**: the hash-table slot (`NPERM + IHASH`) or, for an overflow bin, the full
///   bin's status word `-1` and word 126, in one write. A new bin is empty when linked;
///   an empty bin is valid in DSS6 (a lookup ends at status 0, HEC fills it next).
/// - **Publish**: the bin entry (status word included, one write inside one 512-byte
///   record) for a new record; the entry's info address and sizes (one write) for a
///   rewrite.
/// - **Retire**: (rewrite only) the old info block status set to -1, as `zowrit6.f`
///   line 77 marks a relocated block.
/// - **Finish**: the marker cleared.
pub const DSS6_STEP_ORDER: [WriteStep; 9] = [
    WriteStep::Data,
    WriteStep::EndOfFile,
    WriteStep::Allocation,
    WriteStep::Chain,
    WriteStep::Counters,
    WriteStep::Link,
    WriteStep::Publish,
    WriteStep::Retire,
    WriteStep::Finish,
];

/// How an irregular write combines with existing data (`zsitsi6.f` `INFLAG`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItsMode {
    /// `INFLAG = 1`: values inside the window from the first to the last new time replace
    /// the stored ones; stored values outside the window are kept. HEC's default
    /// (`ztsStoreIrreg6.c` passes storage flag 0 of `ztsStore` through; Monolith writes
    /// with replace).
    #[default]
    Replace,
    /// `INFLAG = 0`: new values are inserted; a stored value at the same time is
    /// replaced; all other stored values are kept.
    Merge,
}

/// One irregular-interval series to store.
#[derive(Debug, Clone)]
pub struct ItsInput<'a> {
    /// `/A/B/C/D/E/F/`; E is `IR-DAY`, `IR-MONTH`, `IR-YEAR`, `IR-DECADE` or `IR-CENTURY`;
    /// D may be empty or a date (it is replaced per block).
    pub pathname: &'a [u8],
    /// One stamp per value, whole seconds since 1899-12-31 00:00 (the 24:00 stamp of a
    /// day is the next day's 00:00). DSS6 stores minutes: every time must be a whole
    /// minute. Times must not decrease (equal times are allowed, as in `zsitsi6.f`
    /// lines 158-163).
    pub times: &'a [i64],
    pub values: RtsValues<'a>,
    /// One quality word per value, or none.
    pub quality: Option<&'a [i32]>,
    pub units: &'a [u8],
    pub data_type: &'a [u8],
    pub mode: ItsMode,
}

/// Words of a new, empty DSS6 file (1-based word address `a` is element `a - 1`): the
/// header and table layout of an empty file created by HEC Monolith 3.3.27 with
/// `Hec_zset("DSSV", "", 6)` (`zopen6Int.f:ZOPEN6INT` lines 371-523, `zfsize6.f` default
/// size code 6). `date` is the 7-character `DDMMMYY` creation and last-write date, `time`
/// the 8-character `HH:MM:SS` last-write time.
pub fn new_file_words6(date: &[u8; 7], time: &[u8; 8]) -> Vec<i32> {
    engine::new_file_words(date, time)
}

/// Create an empty DSS6 file at `path`. Refuses an existing path
/// ([`WriteError::AlreadyExists`]); the file is written in full and synced before
/// returning.
pub fn create_dss6(path: &Path) -> Result<(), WriteError> {
    let (date, time) = engine::when();
    let words = engine::new_file_words(&date, &time);
    let bytes = io::words_to_file_bytes(&words);
    if path.exists() {
        return Err(WriteError::AlreadyExists(path.to_path_buf()));
    }
    // Written in full to a temporary file beside the target, then linked to the target
    // name (which fails if the name exists): an interrupted create leaves no partial file
    // at `path`.
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.dss6-create", std::process::id()));
    let result = (|| -> Result<(), WriteError> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        match std::fs::hard_link(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(WriteError::AlreadyExists(path.to_path_buf()))
            }
            Err(e) => Err(e.into()),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Store a regular-interval series in the existing DSS6 file at `path`.
///
/// `input.start` must be a whole minute. `input.time_zone` must be `None` (DSS6 writes
/// of a time-zone name are not supported). `input.precision` −1 stores `KIPREC` 0, HEC's
/// "undefined" (`zinit6.f` line 897); 0..=9 is stored as given.
pub fn write_rts6(
    path: &Path,
    input: &RtsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = rts::plan(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_with(&file, opts, |eng| rts::store(eng, input, &plan))
}

/// Same as [`write_rts6`] on an already open read-write handle.
pub fn write_rts6_to(
    file: &File,
    input: &RtsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = rts::plan(input)?;
    engine::write_with(file, opts, |eng| rts::store(eng, input, &plan))
}

/// Validate a regular input without touching any file.
pub fn validate_rts6_input(input: &RtsInput) -> Result<(), WriteError> {
    rts::plan(input).map(|_| ())
}

/// The time stamps (whole HEC seconds) the DSS6 writer assigns to `n` values of
/// `pathname` starting at `start`: `INCTIM` from the start, as `zsrtsi6.f` places values
/// (lines 447-459, `zmovbk6.f`). Validates like a write.
pub fn value_times6(pathname: &[u8], start: i64, n: usize) -> Result<Vec<i64>, WriteError> {
    rts::value_times(pathname, start, n)
}

/// Store an irregular-interval series in the existing DSS6 file at `path`.
pub fn write_its6(
    path: &Path,
    input: &ItsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = its::plan(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_with(&file, opts, |eng| its::store(eng, input, &plan))
}

/// Same as [`write_its6`] on an already open read-write handle.
pub fn write_its6_to(
    file: &File,
    input: &ItsInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = its::plan(input)?;
    engine::write_with(file, opts, |eng| its::store(eng, input, &plan))
}

/// Validate an irregular input without touching any file.
pub fn validate_its6_input(input: &ItsInput) -> Result<(), WriteError> {
    its::plan(input).map(|_| ())
}

/// Store one paired-data record, PD (200) or PDD (205) as the arrays' width, in the
/// existing DSS6 file at `path` (`zpdStore6.c` → `zspdi6.f` `IPLAN` 0): a new record
/// or a full replacement of an existing paired-data record. The pathname is stored in
/// upper case. `input.time_zone` must be `None`; at most 100 curves; units and types at
/// most 8 characters; labels at most 100 characters without trailing blanks.
pub fn write_pd6(
    path: &Path,
    input: &PdInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = pd::plan(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_with(&file, opts, |eng| pd::store(eng, &plan))
}

/// Same as [`write_pd6`] on an already open read-write handle.
pub fn write_pd6_to(
    file: &File,
    input: &PdInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = pd::plan(input)?;
    engine::write_with(file, opts, |eng| pd::store(eng, &plan))
}

/// Validate a paired-data input for DSS6 without touching any file.
pub fn validate_pd6_input(input: &PdInput) -> Result<(), WriteError> {
    pd::plan(input).map(|_| ())
}

// ---------------------------------------------------------------------------------
// Grids (add, replace).
// ---------------------------------------------------------------------------------

pub use crate::dss7::write::{
    AlbersParameters, GridCompression, GridInput, GridPlan, GridProjection, SpecifiedReference,
};

/// Validate a grid for a DSS6 write without touching any file (pathname, geometry,
/// strings of at most 12 bytes for units, HRAP source and Albers units, compression);
/// builds the record, so PRECIP_2_BYTE range refusals are reported here too.
pub fn validate_grid6_input(input: &GridInput) -> Result<GridPlan, WriteError> {
    let plan = crate::grid_write::plan_grid(input, 6)?;
    grid::record(input, &plan)?;
    Ok(plan)
}

/// Store one grid record in the existing DSS6 file at `path`: a new record, or a
/// replacement of a grid record of the same type. Every check runs before the first
/// write; a refused write leaves the file unchanged.
pub fn write_grid6(
    path: &Path,
    input: &GridInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    write_grid6_to(&file, input, opts)
}

/// Same as [`write_grid6`] on an already open read-write handle.
pub fn write_grid6_to(
    file: &File,
    input: &GridInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let plan = crate::grid_write::plan_grid(input, 6)?;
    let rec = grid::record(input, &plan)?;
    engine::write_with(file, opts, |eng| {
        if let engine::Found::Yes(ex) = &eng.lookup(&rec.pathname)?.found {
            let stored = ex.f(engine::I_TYPE);
            if stored != rec.record_type || ex.bin_type != rec.record_type {
                return Err(WriteError::Unsupported(format!(
                    "the existing record is type {stored}, not grid type {}; the writer does \
                     not change a record's type",
                    rec.record_type
                )));
            }
        }
        eng.store_all(std::slice::from_ref(&rec))
    })
}

// ---------------------------------------------------------------------------------
// Pattern records.
// ---------------------------------------------------------------------------------

/// Store one pattern record in the existing DSS6 file at `path` (`zsrtpa6.f`, `zsitsi6.f`
/// lines 213-266): a new record, or a replacement of a pattern of the same type. Floats
/// only; no time zone, precision, quality or notes.
pub fn write_pattern6(
    path: &Path,
    input: &PatternInput,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let rec = pattern::record(input)?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    engine::write_with(&file, opts, |eng| pattern::store(eng, &rec))
}

/// Validate a pattern input for DSS6 without touching any file.
pub fn validate_pattern6_input(input: &PatternInput) -> Result<(), WriteError> {
    pattern::record(input).map(|_| ())
}

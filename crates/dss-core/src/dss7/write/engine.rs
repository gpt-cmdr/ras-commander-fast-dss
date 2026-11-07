//! The write engine: series planning, HEC store semantics per block, record allocation,
//! pathname bins and header bookkeeping, with the transactional order of P20 §4.
//!
//! Ports (paths relative to `heclib/heclib_c/src/`, `hec-dss` tag `7-IU-15`; the oracle
//! build `7-JA` differs only where noted):
//! - planning: `Internal/ztsProcessTimes.c:ztsProcessTimes()` (store branch) and the main
//!   loop of `Internal/ztsStoreReg7.c:ztsStoreReg7()`;
//! - one block: `Internal/ztsRegStoreBlock.c:ztsRegStoreBlock()` for storage flag 0 and
//!   `Internal/ztsWriteBlock.c:ztsWriteBlock()`;
//! - records: `Internal/zwriteInternal.c`, `Internal/zwriteNew.c:zwriteNew()`,
//!   `Internal/zwriteExisting.c:zwriteExisting()`, `Internal/zdeleteInternal.c`;
//! - bins: `Internal/zbinNew.c:zbinNew()`, `Internal/zbinUpdate.c:zbinUpdate()`,
//!   `Internal/zcheckInternal.c:zcheckInternal()` (chain walk statistics);
//! - new file: `Internal/zpermCreate.c:zpermCreate()`, `Internal/znewFileSize.c`;
//! - irregular series (P21): `Internal/ztsStoreIrreg7.c:ztsStoreIrreg7()`,
//!   `Internal/ztsIrregStoreBlock.c:ztsIrregStoreBlock()`,
//!   `Internal/ztsIrregMergeBlocks.c:ztsIrregMergeBlocks()` (storage flag 1),
//!   `Internal/zdeleteInternal.c:zdeleteInternal()` for blocks the window empties.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

// P23: paired-data store (`Public/zpdStore.c`), built on the records and transactions
// below.
mod pd;
pub(super) use pd::{check_elev_units, plan_pd, write_pd_planned};
// P38: multi-word quality, notes and patterns.
mod variants;
pub(super) use variants::{
    check_its_variants, check_rts_variants, plan_pattern, write_its_v, write_pattern_planned,
    write_rts_v,
};
// P39: profiles (`Public/ztsStore.c`) and locations (`Public/zlocationStore.c`).
mod location;
mod profile;
pub(super) use location::{plan_location, write_location_planned};
pub(super) use profile::{plan_profile, write_profile_planned};

use super::block::{self, ih, BlockValues};
use super::path::{validate_its_pathname, validate_rts_pathname, ItsPathname, RtsPathname};
use super::{
    lockhold, overlay, BlockOutcome, BlockReport, ItsInput, RtsInput, RtsValues, WriteError,
    WriteOptions, WriteReport, WriteStep, NEW_FILE_VERSION, PROGRAM_NAME,
};
use crate::dss7::compress::Disaggregate;
use crate::dss7::header::{self, Dss7Header};
use crate::dss7::info::{self, split_i4, InfoBlock};
use crate::dss7::interval::{self as iv};
use crate::dss7::ts::{path_parts, ITD, ITS};
use crate::dss7::{pathname_hash, status, table_hash, Dss7Error, Dss7File, RecordRef};
use crate::header::{ByteOrder, FileSignature};
use crate::lock::{self, LockReason, LockRefusal};
use crate::timeseries::TsValues;

// ---------------------------------------------------------------------------------
// Header words (`zdssFileKeys`, `Internal/zinit.c:zinit()`; dss7.md §2.2)
// ---------------------------------------------------------------------------------

mod k {
    pub const NUMBER_RECORDS: usize = 3;
    pub const NUMBER_DELETES: usize = 10;
    pub const FILE_SIZE: usize = 5;
    pub const DEAD: usize = 6;
    pub const NUMBER_EXPANSIONS: usize = 7;
    pub const CREATE_DATE: usize = 12;
    pub const LAST_WRITE_TIME: usize = 13;
    pub const LOCK_ADDRESS_WORD: usize = 14;
    pub const MAX_HASH: usize = 15;
    pub const HASHS_USED: usize = 16;
    pub const MAX_PATHS_ONE_HASH: usize = 17;
    pub const MAX_PATHS_HASH_CODE: usize = 18;
    pub const HASH_TABLE_START: usize = 19;
    pub const HASH_COLLISIONS: usize = 20;
    pub const BINS_PER_BLOCK: usize = 21;
    pub const BINS_REMAIN_IN_BLOCK: usize = 22;
    pub const BIN_SIZE: usize = 23;
    pub const FIRST_BIN: usize = 24;
    pub const NEXT_EMPTY_BIN: usize = 25;
    pub const TOTAL_BINS: usize = 26;
    pub const BINS_OVERFLOW: usize = 27;
    pub const CAT_SEQUENCE_NUMBER: usize = 32;
    pub const CAT_SORT_STATUS: usize = 33;
    pub const CAT_SORT_NEW_WRITES: usize = 34;
    pub const CAT_SORT_DELETES: usize = 35;
    pub const RECLAIM_MIN: usize = 39;
    pub const RECLAIM_TABLE_ADDRESS: usize = 42;
    pub const RECLAIM_SEG_AVAILABLE_ADD: usize = 43;
    pub const RECLAIM_MAX_SEGMENT: usize = 45;
    pub const RECLAIM_NUMBER: usize = 47;
    pub const RECLAIM_SIZE: usize = 48;
    pub const MAX_PATH: usize = 51;
    pub const MAX_A: usize = 52;
    pub const MAX_INTERNAL_HEADER: usize = 58;
    pub const MAX_HEADER2: usize = 59;
    pub const MAX_USER_HEADER: usize = 60;
    pub const MAX_VALUES1: usize = 61;
    pub const MAX_VALUES2: usize = 62;
    pub const MAX_VALUES3: usize = 63;
    pub const NUMBER_INTERNAL_HEADER: usize = 64;
    pub const NUMBER_HEADER2: usize = 65;
    pub const NUMBER_DATA_AREA3: usize = 66;
    pub const NUMBER_USER_HEADER: usize = 67;
    pub const NUMBER_DATA_AREA1: usize = 68;
    pub const NUMBER_DATA_AREA2: usize = 69;
    pub const MAX_RECORD_SIZE: usize = 70;
    pub const MAX_RTS_SIZE: usize = 71;
    pub const MAX_RTD_SIZE: usize = 72;
    pub const MAX_ITS_SIZE: usize = 73;
    pub const MAX_ITD_SIZE: usize = 74;
    pub const MAX_PD_SIZE: usize = 75;
    pub const MAX_OTHER_SIZE: usize = 77;
    pub const LOCK_ARRAY_SIZES: usize = 90;
    pub const LOCK_WRITE_ARRAY: usize = 91;
    pub const LOCK_READ_ARRAY: usize = 92;
    pub const PID_ARRAY: usize = 93;
}

/// Header word 29: the gap between `kfilePassword` (28) and `kfileError` (30)
/// (`zinit.c` lines 535-545: `kfileError = kfilePassword + 2`), never read or written by
/// heclib and 0 in every file it creates. The writer stores the info address of the
/// record it is writing there from the Allocation step until the counters are written,
/// so the next write can reconcile an interrupted one (`Engine::reconcile`).
const PENDING_WORD: usize = 29;

const RTS: i32 = 100;
const RTD: i32 = 105;
const BIN_ENTRY: usize = 7;
const FVER_7_HA: i32 = 70_801;
/// `DSS_LOCK_ARRAY_SIZE` (`headers/zdssVals.h` line 43).
const LOCK_ARRAY_SIZE: i64 = 25;
/// `MINS_IN_1_DAY` (`headers/hecdssInternal.h`).
const MINS_IN_1_DAY: i64 = 1_440;

// ---------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------

/// `i4toi8` on little-endian: `a` low, `b` high (`Utilities/i4toi8.c`).
fn i4toi8(a: i32, b: i32) -> i64 {
    (u64::from(a as u32) | (u64::from(b as u32) << 32)) as i64
}

/// `numberLongsInInts` (`Utilities/numberLongsInInts.c`).
fn longs_in_ints(n: i64) -> i64 {
    if n > 0 {
        (n - 1) / 2 + 1
    } else {
        0
    }
}

/// `numberLongsInBytes` (`Utilities/numberLongsInBytes.c`).
fn longs_in_bytes(n: usize) -> usize {
    if n > 0 {
        (n - 1) / 8 + 1
    } else {
        0
    }
}

fn ints_to_words(v: &[i32]) -> Vec<i64> {
    v.chunks(2)
        .map(|c| i4toi8(c[0], c.get(1).copied().unwrap_or(0)))
        .collect()
}

fn bytes_to_words(b: &[u8], words: usize) -> Vec<i64> {
    let mut buf = b.to_vec();
    buf.resize(words * 8, 0);
    buf.chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
        .collect()
}

/// Milliseconds since 1970 (`getCurrentTimeMillis`).
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn bad_input(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

fn corrupt(m: impl Into<String>) -> WriteError {
    WriteError::Corrupt(m.into())
}

// ---------------------------------------------------------------------------------
// New file
// ---------------------------------------------------------------------------------

/// Words of a new, empty DSS7 file. `zpermCreate()` lines 225-357 with the
/// `znewFileSize()` defaults (8192 hash slots, 200-word bins, 32 bins per block), as an
/// empty file from hecdss 0.1.33 stores them (P20 §2.2).
pub(super) fn new_file_words(now: i64) -> Vec<i64> {
    let max_hash = 8192i64;
    let bin_size = 200i64;
    let bins_per_block = 32i64;
    let reclaim_size = 1002i64;
    // Layout: header 0..100, lock words 100..103, write/read/PID arrays (25 each),
    // hash table, reclaim table, first bin block, EOF flag.
    let lock_word = 100i64;
    let write_array = lock_word + 3;
    let read_array = write_array + LOCK_ARRAY_SIZE;
    let pid_array = read_array + LOCK_ARRAY_SIZE;
    let hash_table = pid_array + LOCK_ARRAY_SIZE;
    let reclaim = hash_table + max_hash;
    let first_bin = reclaim + reclaim_size;
    let eof = first_bin + bin_size * bins_per_block + 1;
    let mut w = vec![0i64; eof as usize + 1];
    w[header::key::DSS] = i64::from_le_bytes(*b"ZDSS\0\0\0\0");
    w[header::key::FILE_HEADER_SIZE] = header::MIN_HEADER_SIZE;
    let mut v = [0u8; 8];
    v[..4].copy_from_slice(NEW_FILE_VERSION);
    w[header::key::VERSION] = i64::from_le_bytes(v);
    w[k::FILE_SIZE] = eof;
    w[k::CREATE_DATE] = now;
    w[k::LAST_WRITE_TIME] = now;
    w[k::LOCK_ADDRESS_WORD] = lock_word;
    w[k::MAX_HASH] = max_hash;
    w[k::HASH_TABLE_START] = hash_table;
    w[k::BINS_PER_BLOCK] = bins_per_block;
    w[k::BINS_REMAIN_IN_BLOCK] = bins_per_block;
    w[k::BIN_SIZE] = bin_size;
    w[k::FIRST_BIN] = first_bin;
    w[k::NEXT_EMPTY_BIN] = first_bin;
    w[k::RECLAIM_MIN] = 100;
    w[k::RECLAIM_TABLE_ADDRESS] = reclaim;
    w[k::RECLAIM_SEG_AVAILABLE_ADD] = reclaim;
    w[k::RECLAIM_MAX_SEGMENT] = 20;
    w[k::RECLAIM_NUMBER] = (reclaim_size - 2) / 2;
    w[k::RECLAIM_SIZE] = reclaim_size;
    w[k::LOCK_ARRAY_SIZES] = LOCK_ARRAY_SIZE;
    w[k::LOCK_WRITE_ARRAY] = write_array;
    w[k::LOCK_READ_ARRAY] = read_array;
    w[k::PID_ARRAY] = pid_array;
    w[header::key::END_FILE_HEADER] = header::END_HEADER_FLAG;
    w[eof as usize] = header::END_FILE_FLAG;
    w
}

// ---------------------------------------------------------------------------------
// Planning (ztsProcessTimes + ztsStoreReg7 main loop)
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(super) struct BlockPlan {
    /// Block start julian (D part).
    pub julian: i64,
    /// `numberInBlock`.
    pub number_in_block: i64,
    /// `blockStartPosition` before trimming.
    pub block_start_position: i64,
    /// Index of the first input value stored in this block (`currentPosition`).
    pub position_in_values: i64,
    /// `numberToStore` before trimming.
    pub number_to_store: i64,
    /// `boolReadBlock` as `ztsStoreReg7()` sets it.
    pub read_block: bool,
}

#[derive(Debug, Clone)]
pub(super) struct Plan {
    pub path: RtsPathname,
    pub interval: i64,
    /// Standard (end-of-interval) time of the first value.
    pub start_julian: i64,
    pub start_seconds: i64,
    pub time_offset: i64,
    pub granularity: i64,
    pub strings: Vec<u8>,
    pub blocks: Vec<BlockPlan>,
}

fn check_text(what: &str, b: &[u8]) -> Result<(), WriteError> {
    if let Some(&c) = b.iter().find(|&&c| !(32..=126).contains(&c)) {
        return Err(bad_input(format!(
            "{what} byte 0x{c:02x} is outside printable ASCII"
        )));
    }
    Ok(())
}

fn year_ok(julian: i64) -> bool {
    (1..=9999).contains(&iv::julian_to_ymd(julian).0)
}

pub(super) fn plan(input: &RtsInput) -> Result<Plan, WriteError> {
    let path = validate_rts_pathname(input.pathname)?;
    let n = input.values.len() as i64;
    if n == 0 {
        return Err(bad_input("no values"));
    }
    if n > i64::from(i32::MAX) {
        return Err(bad_input("more than 2^31 - 1 values"));
    }
    if let Some(q) = input.quality {
        if q.len() as i64 != n {
            return Err(bad_input(format!(
                "{} quality values for {n} values",
                q.len()
            )));
        }
    }
    check_text("units", input.units)?;
    check_text("data type", input.data_type)?;
    if let Some(tz) = input.time_zone {
        check_text("time zone", tz)?;
    }
    let strings = block::header_strings(input.units, input.data_type, input.time_zone);
    if strings.len() > block::MAX_HEADER_STRING_BYTES {
        return Err(bad_input(format!(
            "units, type and time zone take {} header bytes; HEC reads back at most {}",
            strings.len(),
            block::MAX_HEADER_STRING_BYTES
        )));
    }
    if !(-1..=127).contains(&input.precision) {
        return Err(bad_input("precision must be -1 (unset) or 0..=127"));
    }
    let interval = path.interval_seconds;
    // ztsProcessTimes() lines 128, 358-384: clean the start, end = start + (n-1)
    // intervals, then both moved to standard boundaries; the start's offset is kept.
    if input.start == 0 {
        return Err(bad_input(
            "start 1899-12-31 00:00 is HEC's undefined time (julian 0, second 0)",
        ));
    }
    let mut sj = input.start.div_euclid(iv::SECS_IN_1_DAY);
    let mut ss = input.start.rem_euclid(iv::SECS_IN_1_DAY);
    if !year_ok(sj) {
        return Err(bad_input("start year must be 1..=9999"));
    }
    iv::clean_time(&mut sj, &mut ss, iv::SECOND_GRANULARITY);
    let (mut ej, mut es) = iv::increment_time(interval, n - 1, sj, ss);
    let time_offset = iv::offset_adjust_to_standard(interval, &mut sj, &mut ss)
        .ok_or_else(|| bad_input("interval rejected by ztsOffsetAdjustToStandard"))?;
    iv::offset_adjust_to_standard(interval, &mut ej, &mut es);
    if !year_ok(sj) || !year_ok(ej) {
        return Err(bad_input("the series must lie within years 1..=9999"));
    }
    // ztsProcessTimes() lines 453-471.
    let days = ej - sj;
    let granularity = if interval < iv::SECS_IN_1_MINUTE {
        iv::SECOND_GRANULARITY
    } else if days < 1_000_000 {
        iv::MINUTE_GRANULARITY
    } else if days < 30_000_000 {
        iv::HOUR_GRANULARITY
    } else {
        iv::DAY_GRANULARITY
    };
    let (start_block, block_size) = iv::reg_block_start(sj, interval);
    let (end_block, _) = iv::reg_block_start(ej, interval);
    let mut blocks = Vec::new();
    let mut current = 0i64;
    let mut jb = start_block;
    while jb <= end_block {
        let next = iv::increment_block(jb, block_size);
        if next <= jb {
            return Err(bad_input("block arithmetic did not advance"));
        }
        let nib = iv::number_periods(
            interval,
            jb - 1,
            iv::SECS_IN_1_DAY,
            next - 1,
            iv::SECS_IN_1_DAY,
        );
        // Storage flag 0: no read when the series covers the whole block.
        let read_block = !(sj < jb && ej > next);
        let rel = iv::number_periods(interval, jb - 1, iv::SECS_IN_1_DAY, sj, ss) - 1;
        let mut bsp = rel + current;
        if interval == iv::SECS_IN_1_WEEK && bsp == -1 {
            bsp = 0;
        }
        if bsp < 0 {
            return Err(bad_input("invalid block position (HEC INVALID_DATE_TIME)"));
        }
        let to_store = (nib - bsp).min(n - current);
        if to_store < 0 || nib <= 0 {
            return Err(bad_input("invalid block layout for this start time"));
        }
        blocks.push(BlockPlan {
            julian: jb,
            number_in_block: nib,
            block_start_position: bsp,
            position_in_values: current,
            number_to_store: to_store,
            read_block,
        });
        current += to_store;
        jb = next;
    }
    if current != n {
        return Err(bad_input(format!(
            "the block split covers {current} of {n} values"
        )));
    }
    Ok(Plan {
        path,
        interval,
        start_julian: sj,
        start_seconds: ss,
        time_offset,
        granularity,
        strings,
        blocks,
    })
}

// ---------------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------------

/// The areas of one record to store (a `zStructTransfer`, `headers/zStructTransfer.h`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Areas {
    pub data_type: i32,
    pub ih: Vec<i32>,
    pub h2: Vec<i32>,
    pub uh: Vec<i32>,
    pub v1: Vec<i32>,
    /// values2 and values3 (P24: grids store their cells in values3,
    /// `Public/zspatialGridStore.c`); empty for time series.
    pub v2: Vec<i32>,
    pub v3: Vec<i32>,
    pub number_data: i64,
    pub logical: i64,
    pub first: (i32, i32),
    pub last: (i32, i32),
    /// `totalExpandedSize` (`ztsWriteBlock()` line 199), ints.
    pub total_expanded: i64,
}

impl Areas {
    /// `totalNumberInts` of `zwriteInternal()` lines 257-262: the three data arrays, each
    /// rounded to whole words, in ints.
    fn data_ints(&self) -> i64 {
        2 * (longs_in_ints(self.v1.len() as i64)
            + longs_in_ints(self.v2.len() as i64)
            + longs_in_ints(self.v3.len() as i64))
    }
}

/// One block prepared for commit (P21 review M1).
struct Prepared {
    path: Vec<u8>,
    found: Option<RecordRef>,
    areas: Areas,
}

/// A new bin block: block address, block length in words, chain-link word to set.
type NewBlock = (i64, usize, i64);

/// Where a new bin entry goes (`zbinUpdate()`).
enum Target {
    /// No bin for this table hash yet: new bin, table slot.
    FreshSlot,
    /// Entry offset `icount` in the last bin of the chain; `reuse` when it overwrites a
    /// same-pathname non-live entry.
    InBin {
        bin: i64,
        icount: usize,
        reuse: bool,
    },
    /// The last bin is full: new overflow bin linked from `last_bin`.
    Overflow { last_bin: i64 },
}

struct Engine<'a> {
    file: &'a File,
    /// Working header.
    hdr: [i64; 100],
    /// Header as last written to disk.
    disk: [i64; 100],
    opts: &'a WriteOptions,
    now: i64,
    block_index: usize,
    /// While planning (P38 re-check): writes are held here and reads see them.
    plan: std::cell::RefCell<Option<overlay::Overlay<i64>>>,
}

impl<'a> Engine<'a> {
    fn new(file: &'a File, opts: &'a WriteOptions) -> Result<Self, WriteError> {
        let len = file.metadata()?.len();
        let mut head = vec![0u8; header::HEADER_BYTES];
        {
            let mut f = file;
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut head)?;
        }
        let sig = FileSignature::from_bytes(&head).map_err(|e| corrupt(e.to_string()))?;
        if sig.byte_order != ByteOrder::Little {
            return Err(WriteError::Unsupported(
                "big-endian DSS7 files are not written".into(),
            ));
        }
        let h = Dss7Header::parse(&head, len)?;
        if h.fver < FVER_7_HA {
            return Err(WriteError::Unsupported(format!(
                "file revision {} is older than 7-HA; its deprecated aggregate layout is not \
                 written (P20 §1)",
                h.version_string
            )));
        }
        let mut hdr = h.words;
        let disk = hdr;
        // Never overwrite bytes past the EOF flag: allocate after everything on disk.
        let len_words = (len as i64 + 7) / 8;
        if len_words > hdr[k::FILE_SIZE] + 1 {
            let gap = len_words - hdr[k::FILE_SIZE];
            hdr[k::FILE_SIZE] = len_words;
            hdr[k::DEAD] += gap;
        }
        Ok(Engine {
            file,
            hdr,
            disk,
            opts,
            now: now_ms(),
            block_index: 0,
            plan: std::cell::RefCell::new(None),
        })
    }

    // -- raw I/O ---------------------------------------------------------------------

    fn read_words(&self, addr: i64, n: usize) -> Result<Vec<i64>, WriteError> {
        if let Some(o) = &*self.plan.borrow() {
            return o.read(addr, n, |a, m| self.read_file_words(a, m));
        }
        self.read_file_words(addr, n)
    }

    fn read_file_words(&self, addr: i64, n: usize) -> Result<Vec<i64>, WriteError> {
        if addr < header::MIN_HEADER_SIZE {
            return Err(corrupt(format!("read inside the header at word {addr}")));
        }
        let mut buf = vec![0u8; n * 8];
        let mut f = self.file;
        f.seek(SeekFrom::Start(addr as u64 * 8))?;
        f.read_exact(&mut buf)
            .map_err(|_| corrupt(format!("{n} words at {addr} lie past the end of the file")))?;
        Ok(buf
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
            .collect())
    }

    fn read_word(&self, addr: i64) -> Result<i64, WriteError> {
        Ok(self.read_words(addr, 1)?[0])
    }

    fn write_words(&self, addr: i64, w: &[i64]) -> Result<(), WriteError> {
        if let Some(o) = &mut *self.plan.borrow_mut() {
            o.put(addr, w);
            return Ok(());
        }
        let mut buf = Vec::with_capacity(w.len() * 8);
        for x in w {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        let mut f = self.file;
        f.seek(SeekFrom::Start(addr as u64 * 8))?;
        f.write_all(&buf)?;
        Ok(())
    }

    fn write_zeros(&self, addr: i64, n: usize) -> Result<(), WriteError> {
        self.write_words(addr, &vec![0i64; n])
    }

    fn write_header(&mut self, words: [i64; 100]) -> Result<(), WriteError> {
        if self.plan.borrow().is_some() {
            self.disk = words;
            return Ok(());
        }
        let mut buf = Vec::with_capacity(800);
        for x in &words {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        let mut f = self.file;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&buf)?;
        self.disk = words;
        Ok(())
    }

    fn sync(&self) -> Result<(), WriteError> {
        if self.opts.sync && self.plan.borrow().is_none() {
            self.file.sync_data()?;
        }
        Ok(())
    }

    fn step(&self, s: WriteStep) -> Result<(), WriteError> {
        if self.plan.borrow().is_some() {
            return Ok(());
        }
        if let Some(f) = self.opts.fault {
            if f.block == self.block_index && f.step == s {
                return Err(WriteError::InjectedFault {
                    block: self.block_index,
                    step: s,
                });
            }
        }
        Ok(())
    }

    fn alloc(&mut self, words: i64) -> i64 {
        // zgetFileSpace() at EOF (no reclaimed space is used, P20 §5).
        let a = self.hdr[k::FILE_SIZE];
        self.hdr[k::FILE_SIZE] += words;
        a
    }

    fn reader(&self) -> Result<Dss7File<&'a File>, WriteError> {
        Ok(Dss7File::new(self.file)?)
    }

    // -- one block ----------------------------------------------------------------------

    /// Everything one block store reads from the file and every refusal it can raise,
    /// computed before any block of the write is committed (P21 review M1): a refused
    /// multi-block write leaves the file unchanged.
    fn prepare_block(
        &self,
        input: &RtsInput,
        plan: &Plan,
        bp: &BlockPlan,
    ) -> Result<Prepared, WriteError> {
        let path = plan.path.block_pathname(bp.julian);
        let interval = plan.interval;
        let from = bp.position_in_values as usize;
        let count = bp.number_to_store as usize;
        let new_values = match input.values {
            RtsValues::F32(v) => BlockValues::F32(v[from..from + count].to_vec()),
            RtsValues::F64(v) => BlockValues::F64(v[from..from + count].to_vec()),
        };
        let new_quality: Option<Vec<i32>> = input.quality.map(|q| q[from..from + count].to_vec());

        // Lines 313-327: first/last valid of the values being stored, and their times.
        // All missing gives positions (0, 0) (ztsFirstLastValidValues.c lines 189-193).
        let (_, mut f, mut l) = block::first_last_valid(&new_values, new_quality.as_deref());
        let stamp =
            |pos: i64| iv::increment_time(interval, pos, plan.start_julian, plan.start_seconds);
        let mut dates = (
            stamp(bp.position_in_values + f as i64),
            stamp(bp.position_in_values + l as i64),
        );

        // Existing record (zreadInfo), and the checks HEC applies or P20 adds.
        let mut rdr = self.reader()?;
        let found: Option<RecordRef> = match rdr.lookup(&path) {
            Ok(r) => r,
            Err(Dss7Error::BadInfoBlock {
                info_address,
                problem,
            }) => {
                return Err(corrupt(format!(
                    "the existing record's info block at word {info_address} is damaged \
                     ({problem})"
                )))
            }
            Err(e) => return Err(e.into()),
        };
        let mut old_ih: Vec<i32> = Vec::new();
        if let Some(r) = &found {
            if r.status == status::ALIAS {
                return Err(WriteError::Unsupported(
                    "the pathname is an alias; aliases are not written".into(),
                ));
            }
            if r.info.aliases_bin_address != 0 {
                return Err(WriteError::Unsupported(
                    "the record has aliases; records with aliases are not rewritten".into(),
                ));
            }
            if r.record_type != RTS && r.record_type != RTD || r.info.record_type != r.record_type {
                return Err(WriteError::Unsupported(format!(
                    "existing record type {} (info {}) is not a regular float/double time \
                     series (HEC DIFFERENT_RECORD_TYPE)",
                    r.record_type, r.info.record_type
                )));
            }
            old_ih = self.read_area(&r.info.internal_header)?;
            let g = |i: usize| old_ih.get(i).copied().unwrap_or(0);
            if g(ih::PROFILE_DEPTHS) != 0
                || g(ih::INOTES_ELEMENT_SIZE) != 0
                || g(ih::CNOTES_LENGTH) != 0
                || g(ih::INOTES_NUMBER) != 0
            {
                return Err(WriteError::Unsupported(
                    "the existing block stores notes or profile depths".into(),
                ));
            }
            if new_quality.is_some() && g(ih::QUALITY_ELEMENT_SIZE) > 1 {
                return Err(WriteError::Unsupported(
                    "the existing block stores multi-word quality".into(),
                ));
            }
        }
        let gih = |i: usize| i64::from(old_ih.get(i).copied().unwrap_or(0));

        // Stored width: 7-JA writes floats into a double record as doubles (lines
        // 843-875); doubles into a float record make it double (zwriteExisting.c 208-230).
        let to_double = matches!(new_values, BlockValues::F64(_))
            || found.as_ref().is_some_and(|r| r.record_type == RTD);
        // HEC compares the lengths of the data as given, before 7-JA widens floats.
        let given_elem = new_values.element_size() as i64;
        let new_values = widen(new_values, to_double);
        let elem = new_values.element_size() as i64;
        let qes = i64::from(new_quality.is_some());

        // Lines 345-416: decide whether to merge with the block on disk.
        let mut read_block = bp.read_block;
        if let Some(r) = &found {
            if read_block && r.info.logical_number < 1 {
                read_block = false;
            }
            if read_block
                && given_elem == gih(ih::VALUE_SIZE)
                && qes == gih(ih::QUALITY_ELEMENT_SIZE)
                && gih(ih::INOTES_ELEMENT_SIZE) == 0
                && gih(ih::CNOTES_LENGTH) == 0
            {
                let (s, e) = if new_quality.is_none() {
                    let s = bp.block_start_position + f as i64;
                    (s, s + l as i64 - f as i64)
                } else {
                    (
                        bp.block_start_position,
                        bp.block_start_position + bp.number_to_store - 1,
                    )
                };
                if s == gih(ih::BLOCK_START) && e == gih(ih::BLOCK_END) {
                    read_block = false;
                }
            }
        }
        if !read_block && new_quality.is_some() {
            read_block = true;
        }

        let nib = bp.number_in_block as usize;
        let (values, quality, mut bsp) = if read_block {
            // ztsRegReadBlock(): the whole block, missing where nothing is stored.
            let (mut full, mut fq) = match &found {
                Some(r) => {
                    let t = rdr.read_ts_record(r).map_err(WriteError::Ts)?;
                    if t.ts.values.len() != nib {
                        return Err(corrupt(format!(
                            "existing block holds {} positions, expected {nib}",
                            t.ts.values.len()
                        )));
                    }
                    let v = match t.ts.values {
                        TsValues::F32(v) => BlockValues::F32(v),
                        TsValues::F64(v) => BlockValues::F64(v),
                    };
                    let q = t.ts.quality.unwrap_or_else(|| vec![0; nib]);
                    (widen(v, to_double), q)
                }
                None => (missing_block(nib, to_double), vec![0; nib]),
            };
            let at = bp.block_start_position as usize;
            overwrite(&mut full, at, &new_values);
            if let Some(q) = &new_quality {
                fq[at..at + q.len()].copy_from_slice(q);
            }
            let fq = new_quality.as_ref().map(|_| fq);
            (_, f, l) = block::first_last_valid(&full, fq.as_deref());
            // Lines 783-788: dates from the block start, without the time offset.
            let stamp_block =
                |pos: usize| iv::increment_time(interval, pos as i64, bp.julian, interval);
            dates = (stamp_block(f), stamp_block(l));
            (full, fq, 0i64)
        } else {
            (new_values, new_quality, bp.block_start_position)
        };

        let data_type = if to_double { RTD } else { RTS };
        // Lines 794-803: an all-missing block is still written (as one missing value at
        // position 0), because heclib's default `copyEmptyRecords` is 1
        // (`Internal/zinit.c` line 120, `zquery("empty")`); storage flag 0 never deletes.
        // Lines 816-836.
        let (first_index, to_store) = if quality.is_some() {
            bsp = 0;
            (0usize, nib)
        } else {
            bsp += f as i64;
            (f, l - f + 1)
        };
        let ((jf, sf), (jl, sl)) = dates;
        let mut fixed = [0i32; ih::FIXED];
        fixed[ih::TIME_GRANULARITY] = plan.granularity as i32;
        fixed[ih::PRECISION] = input.precision;
        fixed[ih::TIME_OFFSET] = plan.time_offset as i32;
        fixed[ih::BLOCK_START] = bsp as i32;
        fixed[ih::BLOCK_END] = (bsp + to_store as i64 - 1) as i32;
        fixed[ih::VALUE_ELEMENT_SIZE] = elem as i32;
        let words = values.words(first_index, to_store);
        let q_slice = quality
            .as_ref()
            .map(|q| &q[first_index..first_index + to_store]);
        let agg = block::aggregate(to_store, &words, elem as usize, q_slice, &mut fixed);
        let ihw = block::pack_internal_header(&fixed, &plan.strings);
        let areas = Areas {
            data_type,
            ih: ihw,
            h2: agg.header2,
            // ztsRegStoreBlock() passes a NULL user-header buffer to ztsRegReadBlock()
            // (lines 236-238, 498-508; ztsRegReadBlock.c line 273), so HEC never keeps
            // an existing user header; the writer stores none either.
            uh: Vec::new(),
            v1: agg.values1,
            v2: Vec::new(),
            v3: Vec::new(),
            number_data: to_store as i64,
            logical: bp.number_in_block,
            first: (jf as i32, sf as i32),
            last: (jl as i32, sl as i32),
            total_expanded: bp.number_in_block * (elem + qes),
        };
        Ok(Prepared { path, found, areas })
    }

    fn read_area(&self, a: &info::Area) -> Result<Vec<i32>, WriteError> {
        if a.number_ints <= 0 {
            return Ok(Vec::new());
        }
        if a.number_ints > 1 << 30 {
            return Err(corrupt("record area length out of range"));
        }
        let w = self.read_words(a.address, longs_in_ints(a.number_ints) as usize)?;
        let mut v = Vec::with_capacity(a.number_ints as usize);
        for x in w {
            let (lo, hi) = split_i4(x);
            v.push(lo);
            v.push(hi);
        }
        v.truncate(a.number_ints as usize);
        Ok(v)
    }

    fn same_as_stored(&self, r: &RecordRef, a: &Areas) -> Result<bool, WriteError> {
        let i = &r.info;
        if i.record_type != a.data_type
            || i.number_data != a.number_data
            || i.logical_number != a.logical
            || i.first_date != a.first
            || i.last_date != a.last
            || i.values2.number_ints != a.v2.len() as i64
            || i.values3.number_ints != a.v3.len() as i64
        {
            return Ok(false);
        }
        Ok(self.read_area(&i.internal_header)? == a.ih
            && self.read_area(&i.header2)? == a.h2
            && self.read_area(&i.user_header)? == a.uh
            && self.read_area(&i.values1)? == a.v1
            && self.read_area(&i.values2)? == a.v2
            && self.read_area(&i.values3)? == a.v3)
    }

    // -- record words ------------------------------------------------------------------

    /// Record words (info and areas) at `address` (`zwriteNew()` lines 219-316).
    #[allow(clippy::too_many_arguments)]
    fn record_words(
        &self,
        path: &[u8],
        a: &Areas,
        address: i64,
        allocated: i64,
        version: i32,
        expansion: (i32, i32),
        creation: i64,
        reserved1: i64,
    ) -> Vec<i64> {
        let pw = longs_in_bytes(path.len());
        let number_info = info::INFO_FIXED_WORDS + pw;
        let ihn = a.ih.len() as i64;
        let h2n = a.h2.len() as i64;
        let uhn = a.uh.len() as i64;
        let v1n = a.v1.len() as i64;
        let v2n = a.v2.len() as i64;
        let total = number_info as i64
            + longs_in_ints(ihn)
            + longs_in_ints(h2n)
            + longs_in_ints(uhn)
            + longs_in_ints(allocated);
        let mut w = vec![0i64; total as usize];
        let ih_addr = address + number_info as i64;
        let h2_addr = ih_addr + longs_in_ints(ihn);
        let uh_addr = h2_addr + longs_in_ints(h2n);
        let v1_addr = uh_addr + longs_in_ints(uhn);
        let v2_addr = v1_addr + longs_in_ints(v1n);
        let v3_addr = v2_addr + longs_in_ints(v2n);
        use info::key as ik;
        w[ik::FLAG] = info::INFO_FLAG;
        // Written inert (MOVED) and activated after the bin points at it (P20 §4): until
        // then heclib's zcheckLinks() brute-force scan, which checks every info block of
        // status 0/1 for a bin that points at it, sees dead space, not an orphan record.
        w[ik::STATUS] = status::MOVED;
        w[ik::PATHNAME_LENGTH] = path.len() as i64;
        w[ik::HASH] = pathname_hash(path);
        w[ik::TYPE_VERSION] = i4toi8(a.data_type, version);
        w[ik::EXPANSION] = i4toi8(expansion.0, expansion.1);
        w[ik::LAST_WRITE_TIME] = self.now;
        let prog = bytes_to_words(PROGRAM_NAME.as_bytes(), 2);
        w[ik::PROGRAM] = prog[0];
        w[ik::PROGRAM + 1] = prog[1];
        w[ik::FIRST_DATE] = i4toi8(a.first.0, a.first.1);
        w[ik::LAST_DATE] = i4toi8(a.last.0, a.last.1);
        w[ik::CREATION_TIME] = creation;
        w[ik::CREATION_TIME + 1] = reserved1;
        w[ik::INTERNAL_HEAD_ADDRESS] = ih_addr;
        w[ik::INTERNAL_HEAD_NUMBER] = ihn;
        w[ik::HEADER2_ADDRESS] = h2_addr;
        w[ik::HEADER2_NUMBER] = h2n;
        w[ik::USER_HEAD_ADDRESS] = uh_addr;
        w[ik::USER_HEAD_NUMBER] = uhn;
        w[ik::VALUES1_ADDRESS] = v1_addr;
        w[ik::VALUES1_NUMBER] = v1n;
        w[ik::VALUES2_ADDRESS] = v2_addr;
        w[ik::VALUES2_NUMBER] = v2n;
        w[ik::VALUES3_ADDRESS] = v3_addr;
        w[ik::VALUES3_NUMBER] = a.v3.len() as i64;
        w[ik::ALLOCATED_SIZE] = allocated;
        w[ik::NUMBER_DATA] = a.number_data;
        w[ik::LOGICAL_NUMBER] = if a.logical > 0 {
            a.logical
        } else {
            a.number_data
        };
        for (i, x) in bytes_to_words(path, pw).into_iter().enumerate() {
            w[ik::PATHNAME + i] = x;
        }
        let mut put = |at: i64, ints: &[i32]| {
            let off = (at - address) as usize;
            for (i, x) in ints_to_words(ints).into_iter().enumerate() {
                w[off + i] = x;
            }
        };
        put(ih_addr, &a.ih);
        put(h2_addr, &a.h2);
        put(uh_addr, &a.uh);
        put(v1_addr, &a.v1);
        put(v2_addr, &a.v2);
        put(v3_addr, &a.v3);
        w
    }

    /// Area maxima and per-type sizes (`zwriteInternal()` lines 265-317).
    fn update_maxima(&mut self, a: &Areas) {
        let h = &mut self.hdr;
        let max = |h: &mut [i64; 100], i: usize, v: i64| {
            if v > h[i] {
                h[i] = v;
            }
        };
        max(h, k::MAX_INTERNAL_HEADER, a.ih.len() as i64);
        max(h, k::MAX_HEADER2, a.h2.len() as i64);
        max(h, k::MAX_USER_HEADER, a.uh.len() as i64);
        max(h, k::MAX_VALUES1, a.v1.len() as i64);
        max(h, k::MAX_VALUES2, a.v2.len() as i64);
        max(h, k::MAX_VALUES3, a.v3.len() as i64);
        let total = longs_in_ints(a.ih.len() as i64)
            + longs_in_ints(a.h2.len() as i64)
            + longs_in_ints(a.uh.len() as i64)
            + longs_in_ints(a.v1.len() as i64)
            + longs_in_ints(a.v2.len() as i64)
            + longs_in_ints(a.v3.len() as i64);
        max(h, k::MAX_RECORD_SIZE, total);
        match a.data_type {
            RTS => max(h, k::MAX_RTS_SIZE, total),
            RTD => max(h, k::MAX_RTD_SIZE, total),
            ITS => max(h, k::MAX_ITS_SIZE, total),
            ITD => max(h, k::MAX_ITD_SIZE, total),
            // zwriteInternal() lines 309-316: PD and PDD both keep kmaxPdSize (PDD does not
            // use kmaxPddSize); grids and other types kmaxOtherSize.
            crate::paireddata::PD | crate::paireddata::PDD => max(h, k::MAX_PD_SIZE, total),
            _ => max(h, k::MAX_OTHER_SIZE, total),
        }
    }

    /// Counters step: every header counter, statistic and the bin allocation state, with
    /// the in-progress marker still set (P20 review F2: the bin state reaches the disk
    /// before any slot points at the new bin, so heclib's zbinNew() never hands it out
    /// again).
    fn write_counters(&mut self) -> Result<(), WriteError> {
        self.hdr[k::LAST_WRITE_TIME] = self.now;
        let mut h = self.hdr;
        h[PENDING_WORD] = self.disk[PENDING_WORD];
        self.write_header(h)?;
        self.sync()?;
        self.step(WriteStep::Counters)
    }

    /// Finish step: the marker cleared.
    fn finish(&mut self) -> Result<(), WriteError> {
        self.hdr[PENDING_WORD] = 0;
        let h = self.hdr;
        self.write_header(h)?;
        self.sync()?;
        self.step(WriteStep::Finish)
    }

    /// Steps Data, EndOfFile and Allocation for a record (and an optional new bin block).
    fn write_data(
        &mut self,
        rec_addr: i64,
        rec: &[i64],
        block: Option<NewBlock>,
    ) -> Result<(), WriteError> {
        self.write_words(rec_addr, rec)?;
        if let Some((addr, n, _)) = block {
            self.write_zeros(addr, n)?;
        }
        self.step(WriteStep::Data)?;
        self.write_words(self.hdr[k::FILE_SIZE], &[header::END_FILE_FLAG])?;
        self.sync()?;
        self.step(WriteStep::EndOfFile)?;
        // `kfileSize` and the in-progress marker: the new space (record and any new bin
        // block) is reserved before anything points into it, so no later HEC write
        // allocates over it (zgetFileSpace() takes kfileSize). P20 review F1: this comes
        // before the chain word, so a block is never chained past kfileSize.
        let mut d = self.disk;
        d[k::FILE_SIZE] = self.hdr[k::FILE_SIZE];
        d[PENDING_WORD] = rec_addr;
        self.write_header(d)?;
        self.sync()?;
        self.step(WriteStep::Allocation)?;
        if let Some((addr, _, chain)) = block {
            // zbinNew() line 109: the previous block's last word links the new, all-zero
            // block. HEC's catalog walk reads its first (empty) bin and stops, as at the
            // end of any partly used block.
            self.write_words(chain, &[addr])?;
            self.sync()?;
        }
        self.step(WriteStep::Chain)
    }

    // -- bins ----------------------------------------------------------------------------

    /// Reconcile a write that was interrupted after its Allocation step (P20 reviews M1,
    /// M2, F2, F3). Header word 29 names the info block of the record that was being
    /// written. Everything is derived from the file, so the repair is idempotent, and it
    /// stays correct when heclib wrote in between (heclib writes its cached 100-word
    /// header back whole, `Internal/zpermWrite.c`, so the marker survives):
    ///
    /// 1. roll forward: an entry that names the pending info block but that no chain
    ///    reaches (a crash between Publish and Link) is linked as the Link step would:
    ///    table slot when the slot is empty (`zwriteNew()` line 195), else the overflow
    ///    word of the chain's last bin (`zbinNew()` lines 88-99);
    /// 2. bin cursor: a block already linked from the chain word is adopted, and
    ///    non-empty "next empty" bins are skipped as in use. No bin is ever cleared;
    /// 3. pending info block activated when the bin names it; a retired old copy that
    ///    the bin still names is restored to primary;
    /// 4. header statistics rebuilt from the file ([`Engine::rebuild_statistics`]).
    ///
    /// Returns true when a marker was found.
    fn reconcile(&mut self) -> Result<bool, WriteError> {
        let pending = self.hdr[PENDING_WORD];
        if pending == 0 {
            return Ok(false);
        }
        if pending < 0 {
            return self.reconcile_delete(-pending).map(|()| true);
        }
        let bs = self.hdr[k::BIN_SIZE];
        let bpb = self.hdr[k::BINS_PER_BLOCK];
        let path = self.pending_pathname(pending)?;
        // 1. Roll forward an unlinked entry of the pending record.
        if let Some(path) = &path {
            self.roll_forward(pending, path)?;
        }
        // 2. Bin allocation cursor.
        let max_steps = self.hdr[k::FILE_SIZE] / bs.max(1) + 2;
        for _ in 0..max_steps {
            if self.hdr[k::BINS_REMAIN_IN_BLOCK] <= 0 {
                let linked = self.read_word(self.hdr[k::NEXT_EMPTY_BIN])?;
                if linked >= header::MIN_HEADER_SIZE && linked + bs * bpb < self.hdr[k::FILE_SIZE] {
                    self.hdr[k::NEXT_EMPTY_BIN] = linked;
                    self.hdr[k::BINS_REMAIN_IN_BLOCK] = bpb;
                    continue;
                }
                break;
            }
            let x = self.hdr[k::NEXT_EMPTY_BIN];
            let bin = self.read_words(x, bs as usize)?;
            if bin.iter().all(|&w| w == 0) {
                break;
            }
            self.hdr[k::BINS_REMAIN_IN_BLOCK] -= 1;
            self.hdr[k::NEXT_EMPTY_BIN] += bs;
        }
        // 3. Pending info status.
        if let Some(path) = &path {
            let mut rdr = self.reader()?;
            if let Ok(Some(r)) = rdr.lookup(path) {
                let st = self.read_word(r.info_address + info::key::STATUS as i64)?;
                if st == status::MOVED {
                    self.write_words(
                        r.info_address + info::key::STATUS as i64,
                        &[status::PRIMARY],
                    )?;
                    self.sync()?;
                }
            }
        }
        // 4. Statistics.
        self.rebuild_statistics()?;
        self.hdr[PENDING_WORD] = 0;
        let h = self.hdr;
        self.write_header(h)?;
        self.sync()?;
        Ok(true)
    }

    /// Reconcile an interrupted delete (P21; marker = −info address). The Allocation
    /// step already stored the delete's `knumberDeletes`, `kcatSortDeletes` and `kdead`
    /// in the header, so:
    /// - bin entry still live (crash before Publish): the delete is rolled back, the
    ///   info status restored to primary and those three counters taken back;
    /// - bin entry DELETED (crash after Publish): the delete stands; the counters are
    ///   already right and `knumberRecords` is rebuilt with the other statistics.
    ///
    /// The counter changes reach the disk together with the cleared marker, so a crash
    /// inside this repair repeats it exactly once. `kcatSortStatus` stays 3 (conservative:
    /// the stored sort list is treated as unusable).
    fn reconcile_delete(&mut self, addr: i64) -> Result<(), WriteError> {
        if let Some(path) = self.pending_pathname(addr)? {
            let mut rdr = self.reader()?;
            if let Ok(Some(r)) = rdr.lookup(&path) {
                if r.info_address == addr {
                    self.write_words(addr + info::key::STATUS as i64, &[status::PRIMARY])?;
                    self.sync()?;
                    let released = released_words(&r);
                    let h = &mut self.hdr;
                    h[k::NUMBER_DELETES] = (h[k::NUMBER_DELETES] - 1).max(0);
                    h[k::CAT_SORT_DELETES] = (h[k::CAT_SORT_DELETES] - 1).max(0);
                    h[k::DEAD] = (h[k::DEAD] - released).max(0);
                }
            }
        }
        self.rebuild_statistics()?;
        self.hdr[PENDING_WORD] = 0;
        let h = self.hdr;
        self.write_header(h)?;
        self.sync()
    }

    /// Pathname of the info block at `pending`, when it is a valid info block.
    fn pending_pathname(&self, pending: i64) -> Result<Option<Vec<u8>>, WriteError> {
        if pending < header::MIN_HEADER_SIZE
            || pending + info::INFO_FIXED_WORDS as i64 > self.hdr[k::FILE_SIZE]
        {
            return Ok(None);
        }
        let w = self.read_words(pending, info::INFO_FIXED_WORDS)?;
        let n = w[info::key::PATHNAME_LENGTH];
        if w[info::key::FLAG] != info::INFO_FLAG || !(1..=392).contains(&n) {
            return Ok(None);
        }
        let pw = longs_in_bytes(n as usize);
        if pending + (info::INFO_FIXED_WORDS + pw) as i64 > self.hdr[k::FILE_SIZE] {
            return Ok(None);
        }
        let pb = self.read_words(pending + info::INFO_FIXED_WORDS as i64, pw)?;
        Ok(Some(
            pb.iter()
                .flat_map(|x| x.to_le_bytes())
                .take(n as usize)
                .collect(),
        ))
    }

    /// Every bin of every block of the bin-block chain, in physical order, with its
    /// address (`zcheckPathnames()` / `zcatalogInternal()` walk order).
    fn all_bins(&self) -> Result<Vec<(i64, Vec<i64>)>, WriteError> {
        let bs = self.hdr[k::BIN_SIZE] as usize;
        let bpb = self.hdr[k::BINS_PER_BLOCK] as usize;
        let mut block = self.hdr[k::FIRST_BIN];
        let mut out = Vec::new();
        let max_blocks = self.hdr[k::FILE_SIZE] / (bs * bpb) as i64 + 1;
        let mut n = 0i64;
        while block != 0 && n < max_blocks {
            n += 1;
            if block < header::MIN_HEADER_SIZE
                || block + (bs * bpb) as i64 >= self.hdr[k::FILE_SIZE]
            {
                return Err(corrupt(format!(
                    "bin block at word {block} is outside the file"
                )));
            }
            let w = self.read_words(block, bs * bpb + 1)?;
            for b in 0..bpb {
                out.push((block + (b * bs) as i64, w[b * bs..(b + 1) * bs].to_vec()));
            }
            block = w[bs * bpb];
        }
        Ok(out)
    }

    /// Entries of one bin: `(offset, hash, status, chars, pathname bytes, info address)`.
    fn bin_entries(bin: &[i64]) -> Vec<(usize, i64, i64, i32, Vec<u8>, i64)> {
        let bs = bin.len();
        let mut out = Vec::new();
        let mut ic = 0usize;
        while ic + BIN_ENTRY < bs && bin[ic] != 0 {
            let (chars, words) = split_i4(bin[ic + 2]);
            if chars <= 0 || words <= 0 || ic + BIN_ENTRY + words as usize > bs {
                break;
            }
            let path: Vec<u8> = bin[ic + BIN_ENTRY..ic + BIN_ENTRY + words as usize]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .take(chars as usize)
                .collect();
            out.push((ic, bin[ic], bin[ic + 1], chars, path, bin[ic + 3]));
            ic += BIN_ENTRY + words as usize;
            if ic >= bs - 2 {
                break;
            }
        }
        out
    }

    /// The bins of the chain of table slot `th`, in order (bounded).
    fn chain(&self, th: i64) -> Result<Vec<i64>, WriteError> {
        let bs = self.hdr[k::BIN_SIZE];
        let mut a = self.read_word(self.hdr[k::HASH_TABLE_START] + th)?;
        let mut out = Vec::new();
        let max_hops = self.hdr[k::FILE_SIZE] / bs.max(1) + 1;
        while a != 0 && (out.len() as i64) < max_hops {
            if a < header::MIN_HEADER_SIZE || a + bs > self.hdr[k::FILE_SIZE] || out.contains(&a) {
                break;
            }
            out.push(a);
            a = self.read_word(a + bs - 1)?;
        }
        Ok(out)
    }

    /// Step 1 of [`Engine::reconcile`].
    fn roll_forward(&mut self, pending: i64, path: &[u8]) -> Result<(), WriteError> {
        let th = table_hash(path, self.hdr[k::MAX_HASH]);
        let chain = self.chain(th)?;
        let bs = self.hdr[k::BIN_SIZE];
        for &b in &chain {
            let bin = self.read_words(b, bs as usize)?;
            if Self::bin_entries(&bin).iter().any(|e| e.5 == pending) {
                return Ok(()); // already reachable
            }
        }
        let Some((x, _)) = self.all_bins()?.into_iter().find(|(_, bin)| {
            Self::bin_entries(bin)
                .iter()
                .any(|e| e.5 == pending && e.2 <= 1)
        }) else {
            return Ok(()); // not written, or overwritten by heclib's zbinNew()
        };
        match chain.last() {
            None => self.write_words(self.hdr[k::HASH_TABLE_START] + th, &[x])?,
            Some(&last) => self.write_words(last + bs - 1, &[x])?,
        }
        self.sync()
    }

    /// Header statistics rebuilt from the file (P20 review F3), each as heclib keeps it:
    ///
    /// - `knumberRecords`: live (status 0/1) entries of a physical walk of every bin
    ///   (`SemiPublic/zcheckPathnames.c` lines 86-185; heclib changes it only in
    ///   `zwriteNew()` line 328 and `zdeleteInternal()`);
    /// - `khashsUsed`: non-zero table slots (`zwriteNew()` line 199);
    /// - `kbinsOverflow`: bins reached through an overflow word (`zbinUpdate()` line 243);
    /// - `khashCollisions`: entries whose chain holds an earlier entry with the same bin
    ///   hash and length but another pathname (`zwriteNew()` lines 203-206, from
    ///   `zcheckInternal()` line 349). Functional: above 0 it makes heclib compare full
    ///   pathnames (`zcheckInternal()` lines 146 and 341, `zbinUpdate()` line 198);
    /// - `kmaxPathsOneHash` / `kmaxPathsHashCode`: the longest chain in entries and its
    ///   slot (`zupdatePathStats()`);
    /// - `ktotalBins`: bins handed out up to the cursor (`zbinNew()` line 68);
    /// - `kmaxPath`, `kmaxA`..`kmaxF`: raised to the longest live pathname and parts
    ///   (`zupdatePathStats()`; never lowered, as heclib never lowers them);
    /// - `kcatSortStatus`: 1 becomes 2 (records were added, `zwriteNew()` line 333).
    ///
    /// Not rebuilt (approximate after an interrupted write; not used by heclib to find
    /// records): `kdead` (needs a full space accounting; the interrupted record's space is
    /// not booked), the area maxima and `knumber*` area counts, `kcatSortNewWrites`.
    fn rebuild_statistics(&mut self) -> Result<(), WriteError> {
        let bs = self.hdr[k::BIN_SIZE];
        let bpb = self.hdr[k::BINS_PER_BLOCK];
        let bins = self.all_bins()?;
        let mut live = 0i64;
        let mut maxes = [0i64; 7];
        for (_, bin) in &bins {
            for e in Self::bin_entries(bin) {
                if (0..=1).contains(&e.2) {
                    live += 1;
                    maxes[0] = maxes[0].max(e.4.len() as i64);
                    if e.4.len() > 2 {
                        for (i, part) in e.4[1..e.4.len() - 1]
                            .split(|&b| b == b'/')
                            .enumerate()
                            .take(6)
                        {
                            maxes[i + 1] = maxes[i + 1].max(part.len() as i64);
                        }
                    }
                }
            }
        }
        let mh = self.hdr[k::MAX_HASH];
        let slots = self.read_words(self.hdr[k::HASH_TABLE_START], mh as usize)?;
        let (mut used, mut overflow, mut collisions, mut longest, mut longest_slot) =
            (0i64, 0i64, 0i64, 0i64, 0i64);
        for (t, &slot) in slots.iter().enumerate() {
            if slot == 0 {
                continue;
            }
            used += 1;
            let chain = self.chain(t as i64)?;
            overflow += chain.len().saturating_sub(1) as i64;
            let mut seen: Vec<(i64, i32, Vec<u8>)> = Vec::new();
            for b in chain {
                let bin = self.read_words(b, bs as usize)?;
                for e in Self::bin_entries(&bin) {
                    if seen
                        .iter()
                        .any(|(h, n, p)| *h == e.1 && *n == e.3 && !p.eq_ignore_ascii_case(&e.4))
                    {
                        collisions += 1;
                    }
                    seen.push((e.1, e.3, e.4));
                }
            }
            if seen.len() as i64 > longest {
                longest = seen.len() as i64;
                longest_slot = t as i64;
            }
        }
        // Bins handed out: every bin of the blocks before the cursor's block, plus the
        // bins of that block before the cursor.
        let mut handed = 0i64;
        let next = self.hdr[k::NEXT_EMPTY_BIN];
        for (i, chunk) in bins.chunks(bpb as usize).enumerate() {
            let start = chunk[0].0;
            if (start..=start + bs * bpb).contains(&next) {
                handed = i as i64 * bpb + (next - start) / bs;
                break;
            }
        }
        let h = &mut self.hdr;
        h[k::NUMBER_RECORDS] = live;
        h[k::HASHS_USED] = used;
        h[k::BINS_OVERFLOW] = overflow;
        h[k::HASH_COLLISIONS] = collisions;
        h[k::MAX_PATHS_ONE_HASH] = longest;
        h[k::MAX_PATHS_HASH_CODE] = longest_slot;
        if handed > 0 {
            h[k::TOTAL_BINS] = handed;
        }
        h[k::MAX_PATH] = h[k::MAX_PATH].max(maxes[0]);
        for i in 0..6 {
            h[k::MAX_A + i] = h[k::MAX_A + i].max(maxes[i + 1]);
        }
        if h[k::CAT_SORT_STATUS] == 1 {
            h[k::CAT_SORT_STATUS] = 2;
        }
        Ok(())
    }

    /// `zbinNew()`: the next empty bin, or a new block at EOF. Returns the bin address and,
    /// for a new block, `(block address, block words, chain-link word)`.
    ///
    /// Bins left in use by an interrupted write whose counters did not reach the header
    /// (P20 §4: the bin allocation state is written with the counters) are skipped: a
    /// non-empty "next empty" bin counts as used, and a block already linked from the
    /// chain word is adopted instead of allocating another one.
    fn take_bin(&mut self) -> Result<(i64, Option<NewBlock>), WriteError> {
        let bs = self.hdr[k::BIN_SIZE];
        let bpb = self.hdr[k::BINS_PER_BLOCK];
        let max_steps = self.hdr[k::FILE_SIZE] / bs.max(1) + 2;
        for _ in 0..max_steps {
            if self.hdr[k::BINS_REMAIN_IN_BLOCK] <= 0 {
                let link = self.hdr[k::NEXT_EMPTY_BIN];
                let linked = self.read_word(link)?;
                if linked != 0 {
                    if linked < header::MIN_HEADER_SIZE
                        || linked + bs * bpb + 1 > self.hdr[k::FILE_SIZE]
                    {
                        return Err(corrupt(format!(
                            "bin-block chain word {link} points outside the file ({linked})"
                        )));
                    }
                    self.hdr[k::NEXT_EMPTY_BIN] = linked;
                    self.hdr[k::BINS_REMAIN_IN_BLOCK] = bpb;
                    continue;
                }
                let words = bs * bpb + 1;
                let addr = self.alloc(words);
                self.hdr[k::TOTAL_BINS] += 1;
                self.hdr[k::BINS_REMAIN_IN_BLOCK] = bpb - 1;
                self.hdr[k::NEXT_EMPTY_BIN] = addr + bs;
                return Ok((addr, Some((addr, words as usize, link))));
            }
            let addr = self.hdr[k::NEXT_EMPTY_BIN];
            let bin = self.read_words(addr, bs as usize)?;
            self.hdr[k::TOTAL_BINS] += 1;
            self.hdr[k::BINS_REMAIN_IN_BLOCK] -= 1;
            self.hdr[k::NEXT_EMPTY_BIN] += bs;
            if bin.iter().all(|&x| x == 0) {
                return Ok((addr, None));
            }
        }
        Err(corrupt("no empty pathname bin found"))
    }

    /// Walk the table-hash chain (`zcheckInternal()` statistics) and pick the insertion
    /// point (`zbinUpdate()`): returns the target, entries walked, and whether a
    /// different pathname with the same bin hash and length was seen.
    fn insertion_target(
        &self,
        path: &[u8],
        bh: i64,
        slot: i64,
    ) -> Result<(Target, i64, bool), WriteError> {
        if slot == 0 {
            return Ok((Target::FreshSlot, 0, false));
        }
        let bs = self.hdr[k::BIN_SIZE] as usize;
        let pw = longs_in_bytes(path.len());
        let max_hops = self.hdr[k::FILE_SIZE] / bs as i64 + 1;
        let (mut bin_addr, mut hops, mut walked, mut collision) = (slot, 0i64, 0i64, false);
        loop {
            hops += 1;
            if hops > max_hops || bin_addr < header::MIN_HEADER_SIZE {
                return Err(corrupt("pathname-bin chain loops or leaves the file"));
            }
            let bin = self.read_words(bin_addr, bs)?;
            let next = bin[bs - 1];
            let mut ic = 0usize;
            let mut candidate: Option<(usize, bool)> = None;
            while ic + BIN_ENTRY < bs {
                let h = bin[ic];
                if h == 0 {
                    candidate.get_or_insert((ic, false));
                    break;
                }
                let (chars, words) = split_i4(bin[ic + 2]);
                if chars <= 0
                    || words as i64 != (i64::from(chars) + 7) / 8
                    || ic + BIN_ENTRY + words as usize > bs - 1
                {
                    return Err(corrupt(format!(
                        "pathname bin at word {bin_addr} has an invalid entry at offset {ic}"
                    )));
                }
                let po = ic + BIN_ENTRY;
                let stored: Vec<u8> = bin[po..po + words as usize]
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .take(chars as usize)
                    .collect();
                let same_path = chars as usize == path.len() && stored.eq_ignore_ascii_case(path);
                if h == bh && chars as usize == path.len() && !same_path {
                    collision = true;
                }
                let st = bin[ic + 1];
                if h == bh && same_path {
                    if (status::VALID..=status::ALIAS).contains(&st) {
                        // lookup() found no live record, yet a live entry with this
                        // pathname exists: its info block was reused (damaged file).
                        return Err(corrupt(format!(
                            "a live bin entry for this pathname points at a foreign info \
                             block (bin {bin_addr}, offset {ic})"
                        )));
                    }
                    if next == 0 {
                        candidate.get_or_insert((ic, true));
                    }
                }
                walked += 1;
                ic += BIN_ENTRY + words as usize;
                if ic >= bs - 2 {
                    break;
                }
            }
            if next == 0 {
                // zbinUpdate() only places an entry in the last bin of the chain.
                let target = match candidate {
                    Some((ic, reuse)) if ic + BIN_ENTRY + pw + 1 < bs => Target::InBin {
                        bin: bin_addr,
                        icount: ic,
                        reuse,
                    },
                    _ => Target::Overflow { last_bin: bin_addr },
                };
                return Ok((target, walked, collision));
            }
            bin_addr = next;
        }
    }

    fn entry_words(&self, path: &[u8], info_address: i64, a: &Areas) -> Vec<i64> {
        let pw = longs_in_bytes(path.len());
        let mut e = vec![0i64; BIN_ENTRY + pw];
        e[0] = pathname_hash(path);
        e[1] = status::PRIMARY;
        e[2] = i4toi8(path.len() as i32, pw as i32);
        e[3] = info_address;
        e[4] = i4toi8(a.data_type, self.hdr[k::CAT_SEQUENCE_NUMBER] as i32);
        e[5] = self.now;
        e[6] = i4toi8(a.first.0, a.last.0);
        for (i, x) in bytes_to_words(path, pw).into_iter().enumerate() {
            e[BIN_ENTRY + i] = x;
        }
        e
    }

    // -- add / replace / delete ------------------------------------------------------------

    /// New record (`zwriteNew()`, `zbinNew()`, `zbinUpdate()`), in P20 §4 order.
    fn add_new(&mut self, path: &[u8], a: &Areas) -> Result<i64, WriteError> {
        let bh = pathname_hash(path);
        let th = table_hash(path, self.hdr[k::MAX_HASH]);
        let slot_addr = self.hdr[k::HASH_TABLE_START] + th;
        let slot = self.read_word(slot_addr)?;
        let (target, walked, collision) = self.insertion_target(path, bh, slot)?;
        let allocated = a.data_ints();
        let pw = longs_in_bytes(path.len());
        let rec_words = (info::INFO_FIXED_WORDS + pw) as i64
            + longs_in_ints(a.ih.len() as i64)
            + longs_in_ints(a.h2.len() as i64)
            + longs_in_ints(a.uh.len() as i64)
            + longs_in_ints(allocated);
        // Allocation order as HEC: a fresh table hash takes its bin before the record
        // (zwriteNew() line 189); an overflow bin is taken after it (zbinUpdate() line 242).
        let (bin, icount, reuse, block, link): (i64, usize, bool, Option<NewBlock>, Option<i64>);
        let rec_addr;
        match target {
            Target::FreshSlot => {
                let (b, blk) = self.take_bin()?;
                rec_addr = self.alloc(rec_words);
                (bin, icount, reuse, block, link) = (b, 0, false, blk, Some(slot_addr));
            }
            Target::InBin {
                bin: b,
                icount: ic,
                reuse: r,
            } => {
                rec_addr = self.alloc(rec_words);
                (bin, icount, reuse, block, link) = (b, ic, r, None, None);
            }
            Target::Overflow { last_bin } => {
                rec_addr = self.alloc(rec_words);
                let (b, blk) = self.take_bin()?;
                let bs = self.hdr[k::BIN_SIZE];
                (bin, icount, reuse, block, link) = (b, 0, false, blk, Some(last_bin + bs - 1));
                self.hdr[k::BINS_OVERFLOW] += 1;
            }
        }
        let rec = self.record_words(path, a, rec_addr, allocated, 1, (0, 0), self.now, 0);
        self.write_data(rec_addr, &rec, block)?;
        // No old copy to retire for a new record.
        self.step(WriteStep::Retire)?;
        // Publish: entry words, then the one word that makes the entry exist. In a new
        // bin the entry is not reachable yet: no table slot or overflow word points at
        // the bin until the Link step, so a bin a crashed write left behind is never
        // referenced while empty (P20 review M1).
        let e = self.entry_words(path, rec_addr, a);
        let at = bin + icount as i64;
        if reuse {
            self.write_words(at + 2, &[e[2]])?;
            self.write_words(at + 4, &e[4..7])?;
            self.write_words(at + BIN_ENTRY as i64, &e[BIN_ENTRY..])?;
            self.write_words(at + 3, &[e[3]])?;
            self.sync()?;
            self.write_words(at + 1, &[e[1]])?;
        } else {
            self.write_words(at + 1, &e[1..])?;
            self.sync()?;
            self.write_words(at, &[e[0]])?;
        }
        self.sync()?;
        self.step(WriteStep::Publish)?;
        // Counters (zwriteNew() lines 199-341, zupdatePathStats()).
        let h = &mut self.hdr;
        if slot == 0 {
            h[k::HASHS_USED] += 1;
        } else if collision {
            h[k::HASH_COLLISIONS] += 1;
        }
        if path.len() as i64 > h[k::MAX_PATH] {
            h[k::MAX_PATH] = path.len() as i64;
        }
        let inner = &path[1..path.len() - 1];
        for (i, p) in inner.split(|&b| b == b'/').enumerate().take(6) {
            if p.len() as i64 > h[k::MAX_A + i] {
                h[k::MAX_A + i] = p.len() as i64;
            }
        }
        if walked + 1 > h[k::MAX_PATHS_ONE_HASH] {
            h[k::MAX_PATHS_ONE_HASH] = walked + 1;
            h[k::MAX_PATHS_HASH_CODE] = th;
        }
        h[k::NUMBER_RECORDS] += 1;
        h[k::CAT_SORT_NEW_WRITES] += 1;
        if h[k::CAT_SORT_STATUS] > 0 && h[k::CAT_SORT_STATUS] < 2 {
            h[k::CAT_SORT_STATUS] = 2;
        }
        if !a.ih.is_empty() {
            h[k::NUMBER_INTERNAL_HEADER] += 1;
        }
        if !a.h2.is_empty() {
            h[k::NUMBER_HEADER2] += 1;
        }
        if !a.uh.is_empty() {
            h[k::NUMBER_USER_HEADER] += 1;
        }
        if !a.v1.is_empty() {
            h[k::NUMBER_DATA_AREA1] += 1;
        }
        if !a.v2.is_empty() {
            h[k::NUMBER_DATA_AREA2] += 1;
        }
        if !a.v3.is_empty() {
            h[k::NUMBER_DATA_AREA3] += 1;
        }
        self.update_maxima(a);
        self.write_counters()?;
        // Link: table slot (new table hash) or overflow word -> the new bin, which now
        // holds the entry. For a new bin this is the word that makes the record visible.
        if let Some(l) = link {
            self.write_words(l, &[bin])?;
            self.sync()?;
        }
        self.step(WriteStep::Link)?;
        // Activate: the new info block becomes primary (see record_words()).
        self.write_words(rec_addr + info::key::STATUS as i64, &[status::PRIMARY])?;
        self.sync()?;
        self.step(WriteStep::Activate)?;
        self.finish()?;
        Ok(rec_addr)
    }

    /// Rewrite an existing record (`zwriteExisting()` sizes and versions; always a new
    /// copy at EOF, P20 §5).
    fn replace(&mut self, r: &RecordRef, a: &Areas) -> Result<i64, WriteError> {
        let old: &InfoBlock = &r.info;
        let pw = longs_in_bytes(r.pathname.len());
        let number_info = (info::INFO_FIXED_WORDS + pw) as i64;
        let total_number_ints = a.data_ints();
        let mut allocated = total_number_ints;
        let original = old.internal_header.number_ints
            + old.header2.number_ints
            + old.user_header.number_ints
            + old.allocated_size;
        let new_size = a.ih.len() as i64 + a.h2.len() as i64 + a.uh.len() as i64 + allocated;
        let version = old.version.saturating_add(1);
        let (mut exp_n, exp_flag) = (old.number_expansions, old.expansion_flag);
        if new_size > original {
            // Lines 298-353: grow by 1/4, 1/2, full, then +10% of totalExpandedSize.
            exp_n = exp_n.saturating_add(1);
            let tes = a.total_expanded;
            let mut expansion = exp_n;
            if expansion == 1 {
                let es = tes / 4;
                if es > allocated {
                    allocated = es;
                } else {
                    expansion = 2;
                }
            }
            if expansion == 2 {
                let es = tes / 2;
                if es > allocated {
                    allocated = es;
                } else {
                    expansion = 3;
                }
            }
            if expansion == 3 {
                if tes > allocated {
                    allocated = tes;
                } else {
                    expansion = 4;
                }
            }
            if expansion >= 4 {
                let es = (f64::from(tes as f32) * 1.10) as i64;
                if es > allocated {
                    allocated = es;
                }
                if total_number_ints > allocated {
                    allocated = (f64::from(total_number_ints as f32) * 1.10) as i64;
                }
            }
            if total_number_ints > allocated {
                allocated = total_number_ints;
            }
        } else if new_size < original {
            // Lines 433-440: never shrink; keep the slack in the allocated size.
            allocated += original - new_size;
        }
        let rec_words = number_info
            + longs_in_ints(a.ih.len() as i64)
            + longs_in_ints(a.h2.len() as i64)
            + longs_in_ints(a.uh.len() as i64)
            + longs_in_ints(allocated);
        let rec_addr = self.alloc(rec_words);
        let reserved1 = self.read_word(r.info_address + info::key::CREATION_TIME as i64 + 1)?;
        let rec = self.record_words(
            &r.pathname,
            a,
            rec_addr,
            allocated,
            version,
            (exp_n, exp_flag),
            old.creation_ms,
            reserved1,
        );
        self.write_data(rec_addr, &rec, None)?;
        // Retire: the old info block becomes MOVED (zwriteExisting() line 281). Readers
        // do not test the info status (zreadInfo() checks the hash only), so the bin still
        // reads the old copy; zcheckLinks() never sees two status-1 copies.
        self.write_words(r.info_address + info::key::STATUS as i64, &[status::MOVED])?;
        self.sync()?;
        self.step(WriteStep::Retire)?;
        // Publish: bin +3 (info address), then +4..+6 (type, last write, dates).
        let e = self.entry_words(&r.pathname, rec_addr, a);
        let at = r.bin_entry_address;
        let old_tc = self.read_word(at + 4)?;
        let cat = split_i4(old_tc).1;
        self.write_words(at + 3, &[rec_addr])?;
        self.write_words(at + 4, &[i4toi8(a.data_type, cat), e[5], e[6]])?;
        self.sync()?;
        self.step(WriteStep::Publish)?;
        // Counters (zwriteExisting() lines 241-251; dead space of the old copy).
        let old_words = number_info
            + longs_in_ints(old.internal_header.number_ints)
            + longs_in_ints(old.header2.number_ints)
            + longs_in_ints(old.user_header.number_ints)
            + longs_in_ints(old.allocated_size);
        let h = &mut self.hdr;
        if old.internal_header.number_ints == 0 && !a.ih.is_empty() {
            h[k::NUMBER_INTERNAL_HEADER] += 1;
        }
        if old.header2.number_ints == 0 && !a.h2.is_empty() {
            h[k::NUMBER_HEADER2] += 1;
        }
        if old.user_header.number_ints == 0 && !a.uh.is_empty() {
            h[k::NUMBER_USER_HEADER] += 1;
        }
        if old.values1.number_ints == 0 && !a.v1.is_empty() {
            h[k::NUMBER_DATA_AREA1] += 1;
        }
        if old.values2.number_ints == 0 && !a.v2.is_empty() {
            h[k::NUMBER_DATA_AREA2] += 1;
        }
        if old.values3.number_ints == 0 && !a.v3.is_empty() {
            h[k::NUMBER_DATA_AREA3] += 1;
        }
        h[k::DEAD] += old_words;
        h[k::NUMBER_EXPANSIONS] += 1;
        self.update_maxima(a);
        self.write_counters()?;
        self.step(WriteStep::Link)?;
        self.write_words(rec_addr + info::key::STATUS as i64, &[status::PRIMARY])?;
        self.sync()?;
        self.step(WriteStep::Activate)?;
        self.finish()?;
        Ok(rec_addr)
    }
}

/// Words a delete releases (`zdeleteInternal()` lines 177-184), in words.
fn released_words(r: &RecordRef) -> i64 {
    let i = &r.info;
    info::INFO_FIXED_WORDS as i64
        + longs_in_bytes(r.pathname.len()) as i64
        + longs_in_ints(i.internal_header.number_ints)
        + longs_in_ints(i.header2.number_ints)
        + longs_in_ints(i.values3.number_ints)
        + longs_in_ints(i.user_header.number_ints)
        + longs_in_ints(i.values1.number_ints)
        + longs_in_ints(i.values2.number_ints)
}

fn widen(v: BlockValues, to_double: bool) -> BlockValues {
    match v {
        BlockValues::F32(x) if to_double => {
            BlockValues::F64(x.into_iter().map(f64::from).collect())
        }
        other => other,
    }
}

/// A block of HEC missing values (`zsetMissing`: −FLT_MAX in either width).
fn missing_block(n: usize, double: bool) -> BlockValues {
    if double {
        BlockValues::F64(vec![-f64::from(f32::MAX); n])
    } else {
        BlockValues::F32(vec![-f32::MAX; n])
    }
}

fn overwrite(full: &mut BlockValues, at: usize, new: &BlockValues) {
    match (full, new) {
        (BlockValues::F32(f), BlockValues::F32(n)) => f[at..at + n.len()].copy_from_slice(n),
        (BlockValues::F64(f), BlockValues::F64(n)) => f[at..at + n.len()].copy_from_slice(n),
        (BlockValues::F64(f), BlockValues::F32(n)) => {
            for (d, s) in f[at..at + n.len()].iter_mut().zip(n) {
                *d = f64::from(*s);
            }
        }
        // `widen` makes the new values double whenever the block is double.
        (BlockValues::F32(_), BlockValues::F64(_)) => unreachable!("widened before merge"),
    }
}

// ---------------------------------------------------------------------------------
// Irregular-interval series (P21): ztsProcessTimes (store branch, irregular) and the
// main loop of ztsStoreIrreg7, then ztsIrregStoreBlock / ztsIrregMergeBlocks for
// storage flag 1 ("replace": the values inside the new time window replace what is
// stored, the rest of each block is kept). Paths relative to `heclib/heclib_c/src/` of
// `hec-dss` tag `7-IU-15`.
// ---------------------------------------------------------------------------------

/// Granularities a caller may choose (`headers/hecdssInternal.h` lines 427-430).
pub(super) const GRANULARITIES: [i64; 4] = [
    iv::SECOND_GRANULARITY,
    iv::MINUTE_GRANULARITY,
    iv::HOUR_GRANULARITY,
    iv::DAY_GRANULARITY,
];

/// More empty blocks than this between two stored blocks are checked with one bin scan
/// instead of one lookup per block.
const GAP_LOOKUP_LIMIT: usize = 2048;

#[derive(Debug, Clone)]
pub(super) struct ItsBlockPlan {
    /// Block start julian (D part) and the next block's start.
    pub julian: i64,
    pub next: i64,
    /// Input values stored in this block: `from..from + count`.
    pub from: usize,
    pub count: usize,
    /// `boolReadBlock` (`ztsStoreIrreg7()` lines 357-366, storage flag 1).
    pub read_block: bool,
    /// `boolFromStartOfBlock` (0 for the first block, lines 323 and 509).
    pub from_start: bool,
    /// `boolToEndOfBlock` (lines 438-443).
    pub to_end: bool,
}

#[derive(Debug, Clone)]
pub(super) enum ItsStep {
    Store(ItsBlockPlan),
    /// Blocks `[first, stop)` that lie wholly inside the time window and receive no
    /// values: storage flag 1 deletes them (`ztsStoreIrreg7()` lines 411-415).
    DeleteGap {
        first: i64,
        stop: i64,
    },
}

#[derive(Debug, Clone)]
pub(super) struct ItsPlan {
    pub path: ItsPathname,
    /// Granularity stamped in internal-header word 0.
    pub granularity: i64,
    /// The caller chose the granularity (merged times that are not whole multiples of it
    /// are refused); otherwise a block falls back to seconds for such times.
    pub granularity_explicit: bool,
    pub strings: Vec<u8>,
    pub steps: Vec<ItsStep>,
}

impl ItsPlan {
    pub fn century(&self) -> bool {
        self.path.block_size == iv::BLOCK_1_CENTURY
    }

    /// Seconds per stored time unit: seconds, minutes for IR-Century
    /// (`ztsStoreIrreg7()` lines 429-435).
    pub fn unit(&self) -> i64 {
        if self.century() {
            iv::SECS_IN_1_MINUTE
        } else {
            1
        }
    }
}

/// `cleanTime()` of a time given in whole seconds since julian 0.
fn clean_seconds(t: i64) -> (i64, i64) {
    let mut j = t.div_euclid(iv::SECS_IN_1_DAY);
    let mut s = t.rem_euclid(iv::SECS_IN_1_DAY);
    iv::clean_time(&mut j, &mut s, iv::SECOND_GRANULARITY);
    (j, s)
}

pub(super) fn plan_its(input: &ItsInput) -> Result<ItsPlan, WriteError> {
    let path = validate_its_pathname(input.pathname)?;
    let n = input.values.len();
    if n == 0 {
        return Err(bad_input("no values"));
    }
    if n as i64 > i64::from(i32::MAX) {
        return Err(bad_input("more than 2^31 - 1 values"));
    }
    if input.times.len() != n {
        return Err(bad_input(format!(
            "{} times for {n} values",
            input.times.len()
        )));
    }
    if let Some(q) = input.quality {
        if q.len() != n {
            return Err(bad_input(format!(
                "{} quality values for {n} values",
                q.len()
            )));
        }
    }
    check_text("units", input.units)?;
    check_text("data type", input.data_type)?;
    if let Some(tz) = input.time_zone {
        check_text("time zone", tz)?;
    }
    let strings = block::header_strings(input.units, input.data_type, input.time_zone);
    if strings.len() > block::MAX_HEADER_STRING_BYTES {
        return Err(bad_input(format!(
            "units, type and time zone take {} header bytes; HEC reads back at most {}",
            strings.len(),
            block::MAX_HEADER_STRING_BYTES
        )));
    }
    if !(-1..=127).contains(&input.precision) {
        return Err(bad_input("precision must be -1 (unset) or 0..=127"));
    }
    let century = path.block_size == iv::BLOCK_1_CENTURY;
    // Times: defined, within years 1..=9999, strictly ascending. heclib refuses only a
    // descending pair (TIMES_NOT_ASCENDING, `ztsStoreIrreg7()` lines 257-278) and stores
    // equal times as two values; the writer refuses duplicates (P21 §2.4).
    let mut prev: Option<i64> = None;
    for (i, &t) in input.times.iter().enumerate() {
        if t == 0 {
            return Err(bad_input(format!(
                "time {i} is 1899-12-31 00:00, HEC's undefined time (julian 0, second 0)"
            )));
        }
        let (j, _) = clean_seconds(t);
        if !year_ok(j) {
            return Err(bad_input(format!("time {i} lies outside years 1..=9999")));
        }
        if let Some(p) = prev {
            if t == p {
                return Err(bad_input(format!(
                    "times {} and {i} are equal (duplicate times are not written)",
                    i - 1
                )));
            }
            if t < p {
                return Err(bad_input(format!(
                    "times are not ascending at {i} (HEC TIMES_NOT_ASCENDING)"
                )));
            }
        }
        prev = Some(t);
        // IR-Century stores minutes (`ztsStoreIrreg7()` lines 429-431 divide by 60 with
        // truncation): refuse rather than truncate.
        if century && t % iv::SECS_IN_1_MINUTE != 0 {
            return Err(bad_input(format!(
                "time {i} is not a whole minute; IR-Century blocks store minutes"
            )));
        }
    }
    let whole_minutes = input.times.iter().all(|&t| t % iv::SECS_IN_1_MINUTE == 0);
    let (granularity, explicit) = match input.time_granularity {
        Some(g) => {
            if !GRANULARITIES.contains(&g) {
                return Err(bad_input(format!(
                    "time granularity {g} is not 1, 60, 3600 or 86400 seconds"
                )));
            }
            if century && g < iv::MINUTE_GRANULARITY {
                return Err(bad_input(
                    "IR-Century blocks need a granularity of 60 seconds or more (a century \
                     of seconds overflows HEC's int32 time arrays)",
                ));
            }
            if let Some((i, _)) = input.times.iter().enumerate().find(|(_, &t)| t % g != 0) {
                return Err(bad_input(format!(
                    "time {i} is not a whole multiple of the {g}-second granularity"
                )));
            }
            (g, true)
        }
        None => (
            if century || whole_minutes {
                iv::MINUTE_GRANULARITY
            } else {
                iv::SECOND_GRANULARITY
            },
            false,
        ),
    };
    // ztsProcessTimes() lines 323-356 and 412-420: the window runs from the first to
    // the last time (cleaned, so a 00:00 time is 24:00 of the previous day), and the
    // blocks from the block of the first to the block of the last.
    let bs = path.block_size;
    let (sj, _) = clean_seconds(input.times[0]);
    let (ej, _) = clean_seconds(input.times[n - 1]);
    let start_block = iv::irreg_block_start(sj, bs).ok_or_else(|| bad_input("block size"))?;
    let end_block = iv::irreg_block_start(ej, bs).ok_or_else(|| bad_input("block size"))?;
    let mut steps = Vec::new();
    let mut current = 0usize;
    let mut jb = start_block;
    let mut first_block = true;
    let mut gap_start: Option<i64> = None;
    while jb <= end_block {
        let next = iv::increment_block(jb, bs);
        if next <= jb {
            return Err(bad_input("block arithmetic did not advance"));
        }
        // Lines 374-406: a value belongs to the block while its time from the block
        // start is at most the block length (24:00 of the last day is in the block).
        let block_end = next * iv::SECS_IN_1_DAY;
        let mut ipos = current;
        while ipos < n && input.times[ipos] <= block_end {
            ipos += 1;
        }
        let count = ipos - current;
        let read_block = !(sj < jb && ej >= next);
        if count == 0 {
            if !read_block {
                gap_start.get_or_insert(jb);
            }
        } else {
            if let Some(g0) = gap_start.take() {
                steps.push(ItsStep::DeleteGap {
                    first: g0,
                    stop: jb,
                });
            }
            steps.push(ItsStep::Store(ItsBlockPlan {
                julian: jb,
                next,
                from: current,
                count,
                read_block,
                from_start: !first_block,
                to_end: ej >= next,
            }));
        }
        current = ipos;
        first_block = false;
        if jb == end_block {
            break;
        }
        // Skip runs of empty blocks quickly (IR-Day over centuries): jump to the block
        // of the next value, recording the skipped blocks as a gap.
        let mut nb = next;
        if current < n {
            let (vj, _) = clean_seconds(input.times[current]);
            let target = iv::irreg_block_start(vj, bs).ok_or_else(|| bad_input("block size"))?;
            if target > next {
                gap_start.get_or_insert(next);
                nb = target;
            }
        }
        jb = nb;
    }
    if current != n {
        return Err(bad_input(format!(
            "the block split covers {current} of {n} values"
        )));
    }
    Ok(ItsPlan {
        path,
        granularity,
        granularity_explicit: explicit,
        strings,
        steps,
    })
}

/// One stored irregular block, expanded: times in stored units (seconds or minutes from
/// the block start), values in the requested width, quality when requested.
struct ItsBlockData {
    times: Vec<i32>,
    values: BlockValues,
    quality: Option<Vec<i32>>,
}

/// The intended `ztsIrregMergeBlocks()` result for storage flag 1 (lines 418-616):
/// stored values before the window (first block only, `!boolFromStartOfBlock`), the new
/// values, then stored values after the window (last block only, `!boolToEndOfBlock`).
///
/// heclib 7-IU-15 (and the 7-JA oracle) bound the copy of the stored values after the
/// window by `combinedTotal`, which was reduced by the values kept before the window
/// (line 488), so that copy stops early when more stored values precede the window
/// than it replaces (line 552). The writer keeps every stored value after the window
/// (P21 §5, REPORT §3).
fn merge_replace(
    old: ItsBlockData,
    new: ItsBlockData,
    from_start: bool,
    to_end: bool,
) -> ItsBlockData {
    let first = new.times[0];
    let last = new.times[new.times.len() - 1];
    let n_old = old.times.len();
    let keep: Vec<usize> = (0..n_old)
        .take_while(|&i| !from_start && old.times[i] < first)
        .chain((0..n_old).filter(|&i| !to_end && old.times[i] > last))
        .collect();
    let pre = keep.iter().take_while(|&&i| old.times[i] < first).count();
    let mut times = Vec::with_capacity(keep.len() + new.times.len());
    let mut idx: Vec<(bool, usize)> = Vec::with_capacity(times.capacity());
    for &i in &keep[..pre] {
        times.push(old.times[i]);
        idx.push((false, i));
    }
    for (i, &t) in new.times.iter().enumerate() {
        times.push(t);
        idx.push((true, i));
    }
    for &i in &keep[pre..] {
        times.push(old.times[i]);
        idx.push((false, i));
    }
    let values = match (&old.values, &new.values) {
        (BlockValues::F32(o), BlockValues::F32(v)) => BlockValues::F32(
            idx.iter()
                .map(|&(isnew, i)| if isnew { v[i] } else { o[i] })
                .collect(),
        ),
        (BlockValues::F64(o), BlockValues::F64(v)) => BlockValues::F64(
            idx.iter()
                .map(|&(isnew, i)| if isnew { v[i] } else { o[i] })
                .collect(),
        ),
        // Both sides are read or widened to the same width before merging.
        _ => unreachable!("merge widths differ"),
    };
    let quality = new.quality.as_ref().map(|q| {
        idx.iter()
            .map(|&(isnew, i)| {
                if isnew {
                    q[i]
                } else {
                    old.quality.as_ref().map_or(0, |oq| oq[i])
                }
            })
            .collect()
    });
    ItsBlockData {
        times,
        values,
        quality,
    }
}

/// `(julian, seconds)` of a stored time (`ztsIrregStoreBlock()` lines 682-707).
fn its_stamp(julian_block: i64, t: i32, century: bool) -> (i64, i64) {
    let t = i64::from(t);
    let (mut j, mut s) = if century {
        let d = t / MINS_IN_1_DAY;
        (
            julian_block + d,
            (t - d * MINS_IN_1_DAY) * iv::SECS_IN_1_MINUTE,
        )
    } else {
        let d = t / iv::SECS_IN_1_DAY;
        (julian_block + d, t - d * iv::SECS_IN_1_DAY)
    };
    if s < 1 {
        j -= 1;
        s += iv::SECS_IN_1_DAY;
    }
    (j, s)
}

/// `logcialNumberData` (`ztsIrregStoreBlock()` lines 717-743; `julianEndBlockDate` is
/// the next block's start, `ztsStoreIrreg7()` line 490). C int arithmetic.
fn its_logical(julian: i64, next: i64, first: (i64, i64), last: (i64, i64), total: i64) -> i64 {
    let days = next - julian + 1;
    let ((jf, sf), (jl, sl)) = (first, last);
    let (span, per_day) = if days < 370 {
        (
            (jl - jf + 1) * iv::SECS_IN_1_DAY + (sl - sf),
            iv::SECS_IN_1_DAY,
        )
    } else {
        (
            (jl - jf + 1) * MINS_IN_1_DAY + (sl - sf) / iv::SECS_IN_1_MINUTE,
            MINS_IN_1_DAY,
        )
    };
    let average = span / total.max(1);
    if average > 0 {
        days * per_day / average
    } else {
        1
    }
}

impl Engine<'_> {
    /// Read a stored irregular block in full (`ztsIrregReadBlock()` over the block's
    /// whole window) with `ztsDisaggregate()`: times are values1's leading
    /// `kinfoNumberData` ints; values in the requested width, quality when `qes` is 1.
    fn read_its_block(
        &self,
        r: &RecordRef,
        to_double: bool,
        qes: usize,
    ) -> Result<ItsBlockData, WriteError> {
        let ih = self.read_area(&r.info.internal_header)?;
        let h2 = self.read_area(&r.info.header2)?;
        let v1 = self.read_area(&r.info.values1)?;
        let n = r.info.number_data;
        if n < 0 || n as usize > v1.len() {
            return Err(corrupt(
                "the existing block's value count exceeds its values area",
            ));
        }
        let elem = if to_double { 2 } else { 1 };
        let d = Disaggregate {
            data_in: &v1,
            data_in_position: n as usize,
            header2: &h2,
            ih: &ih,
            number_to_read: n,
            number_stored: n,
            position_relative_first_valid: 0,
            position_relative_last_valid: 0,
            deprecated: false,
            values_requested: elem,
            quality_requested: qes,
            inotes_requested: 0,
            want_cnotes: false,
        };
        let x = d
            .run()
            .map_err(|e| corrupt(format!("the existing block does not decode: {}", e.0)))?;
        let n = n as usize;
        if x.values.len() < n * elem || x.quality.len() < n * qes {
            return Err(corrupt(
                "the existing block decodes to fewer values than stored",
            ));
        }
        let values = if to_double {
            BlockValues::F64(
                (0..n)
                    .map(|i| f64::from_bits(i4toi8(x.values[2 * i], x.values[2 * i + 1]) as u64))
                    .collect(),
            )
        } else {
            BlockValues::F32(
                x.values[..n]
                    .iter()
                    .map(|&w| f32::from_bits(w as u32))
                    .collect(),
            )
        };
        Ok(ItsBlockData {
            times: v1[..n].to_vec(),
            values,
            quality: (qes == 1).then(|| x.quality[..n].to_vec()),
        })
    }

    /// One irregular block: `ztsIrregStoreBlock()` with storage flag 1, then
    /// `ztsWriteBlock()` (P21 §3).
    fn prepare_its_block(
        &self,
        input: &ItsInput,
        plan: &ItsPlan,
        bp: &ItsBlockPlan,
    ) -> Result<Prepared, WriteError> {
        let path = plan.path.block_pathname(bp.julian);
        let century = plan.century();
        let unit = plan.unit();
        let base = bp.julian * iv::SECS_IN_1_DAY;
        let (from, count) = (bp.from, bp.count);
        // ztsStoreIrreg7() lines 424-436: seconds (minutes for IR-Century) from the
        // block start. The plan bounds them to the block, so they fit an int.
        let new_times: Vec<i32> = input.times[from..from + count]
            .iter()
            .map(|&t| ((t - base) / unit) as i32)
            .collect();
        let new_values = match input.values {
            RtsValues::F32(v) => BlockValues::F32(v[from..from + count].to_vec()),
            RtsValues::F64(v) => BlockValues::F64(v[from..from + count].to_vec()),
        };
        let new_quality: Option<Vec<i32>> = input.quality.map(|q| q[from..from + count].to_vec());

        let found = self.lookup_for_write(&path)?;
        if let Some(r) = &found {
            self.check_existing_its(r)?;
        }
        // Width: the 7-JA oracle build (`hec-dss` `f14554d`) widens floats written into a
        // double record to doubles inside the block loop of `ztsStoreIrreg7()`, with the
        // write's time window unchanged; this port follows it. 7-IU-15 instead calls
        // `ztsStore()` recursively on a clone holding only this block's values
        // (`ztsIrregStoreBlock()` lines 304-352), whose window is recomputed from those
        // values, so in a multi-block write it would keep stored values that 7-JA and
        // this writer replace. The reference build is 7-JA. Doubles into a float
        // record make it double (`zwriteExisting.c` lines 208-230).
        let to_double = matches!(new_values, BlockValues::F64(_))
            || found.as_ref().is_some_and(|r| r.record_type == ITD);
        let new_values = widen(new_values, to_double);
        let elem = new_values.element_size() as i64;
        let qes = usize::from(new_quality.is_some());
        let new = ItsBlockData {
            times: new_times,
            values: new_values,
            quality: new_quality,
        };
        // Lines 409-441: merge with the stored block unless the window covers it.
        let read = bp.read_block && found.as_ref().is_some_and(|r| r.info.logical_number >= 1);
        let data = match (&found, read) {
            (Some(r), true) => {
                let old = self.read_its_block(r, to_double, qes)?;
                merge_replace(old, new, bp.from_start, bp.to_end)
            }
            _ => new,
        };
        let total = data.times.len();
        // Granularity: the stored times are seconds (minutes) whatever the granularity,
        // and readers divide them by it, so every merged time must be a whole multiple.
        let mut gran = plan.granularity;
        if data
            .times
            .iter()
            .any(|&t| (i64::from(t) * unit) % gran != 0)
        {
            if plan.granularity_explicit {
                return Err(bad_input(format!(
                    "stored values kept in block {} are not whole multiples of the \
                     {gran}-second granularity",
                    String::from_utf8_lossy(&path)
                )));
            }
            gran = unit;
        }
        let first = its_stamp(bp.julian, data.times[0], century);
        let last = its_stamp(bp.julian, data.times[total - 1], century);
        let logical = its_logical(bp.julian, bp.next, first, last, total as i64);
        let data_type = if to_double { ITD } else { ITS };
        let mut fixed = [0i32; ih::FIXED];
        // ztsWriteBlock() lines 174-176; ztsStoreIrreg7() line 212.
        fixed[ih::TIME_GRANULARITY] = gran as i32;
        fixed[ih::PRECISION] = input.precision;
        fixed[ih::TIME_OFFSET] = 0;
        fixed[ih::VALUE_ELEMENT_SIZE] = elem as i32;
        let words = data.values.words(0, total);
        let agg = block::aggregate_with_times(
            total,
            Some(&data.times),
            &words,
            elem as usize,
            data.quality.as_deref(),
            &mut fixed,
        );
        let ihw = block::pack_internal_header(&fixed, &plan.strings);
        let areas = Areas {
            data_type,
            ih: ihw,
            h2: agg.header2,
            // ztsIrregStoreBlock() reads no user header (`userHeader, 0` to
            // ztsIrregReadBlock(), line 532) and writes the caller's (none).
            uh: Vec::new(),
            v1: agg.values1,
            v2: Vec::new(),
            v3: Vec::new(),
            number_data: total as i64,
            logical,
            first: (first.0 as i32, first.1 as i32),
            last: (last.0 as i32, last.1 as i32),
            // ztsWriteBlock() lines 199-203: values, quality and the time array.
            total_expanded: logical * (elem + qes as i64) + logical,
        };
        Ok(Prepared { path, found, areas })
    }

    /// Refusals for an existing irregular record that a store would merge into or a
    /// delete would remove (P21 review M1, m4): only ITS/ITD records without notes,
    /// profile depths or multi-word quality are written or deleted. heclib's `zdelete()`
    /// is type-blind; the writer refuses rather than delete a record it would not store.
    fn check_existing_its(&self, r: &RecordRef) -> Result<(), WriteError> {
        if r.record_type != ITS && r.record_type != ITD || r.info.record_type != r.record_type {
            return Err(WriteError::Unsupported(format!(
                "existing record {} has type {} (info {}), not an irregular float/double \
                 time series (HEC DIFFERENT_RECORD_TYPE)",
                String::from_utf8_lossy(&r.pathname),
                r.record_type,
                r.info.record_type
            )));
        }
        let old_ih = self.read_area(&r.info.internal_header)?;
        let g = |i: usize| old_ih.get(i).copied().unwrap_or(0);
        if g(ih::PROFILE_DEPTHS) != 0
            || g(ih::INOTES_ELEMENT_SIZE) != 0
            || g(ih::CNOTES_LENGTH) != 0
            || g(ih::INOTES_NUMBER) != 0
        {
            return Err(WriteError::Unsupported(format!(
                "the existing block {} stores notes or profile depths",
                String::from_utf8_lossy(&r.pathname)
            )));
        }
        if g(ih::QUALITY_ELEMENT_SIZE) > 1 {
            return Err(WriteError::Unsupported(format!(
                "the existing block {} stores multi-word quality",
                String::from_utf8_lossy(&r.pathname)
            )));
        }
        Ok(())
    }

    /// Commit one prepared block.
    fn commit(&mut self, p: Prepared) -> Result<BlockReport, WriteError> {
        self.commit_areas(p.path, p.found, &p.areas)
    }

    /// Lookup with P20's handling of damaged and alias records.
    fn lookup_for_write(&self, path: &[u8]) -> Result<Option<RecordRef>, WriteError> {
        let mut rdr = self.reader()?;
        let found = match rdr.lookup(path) {
            Ok(r) => r,
            Err(Dss7Error::BadInfoBlock {
                info_address,
                problem,
            }) => {
                return Err(corrupt(format!(
                    "the existing record's info block at word {info_address} is damaged \
                     ({problem})"
                )))
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(r) = &found {
            if r.status == status::ALIAS {
                return Err(WriteError::Unsupported(
                    "the pathname is an alias; aliases are not written".into(),
                ));
            }
            if r.info.aliases_bin_address != 0 {
                return Err(WriteError::Unsupported(
                    "the record has aliases; records with aliases are not rewritten".into(),
                ));
            }
        }
        Ok(found)
    }

    /// Add, replace or skip one record (P20 §3.3 items 10-11, §4).
    fn commit_areas(
        &mut self,
        path: Vec<u8>,
        found: Option<RecordRef>,
        areas: &Areas,
    ) -> Result<BlockReport, WriteError> {
        match found {
            None => {
                let info_address = self.add_new(&path, areas)?;
                Ok(BlockReport {
                    pathname: path,
                    outcome: BlockOutcome::Added,
                    record_type: areas.data_type,
                    info_address,
                    number_data: areas.number_data,
                    logical_number: areas.logical,
                })
            }
            Some(r) => {
                if self.same_as_stored(&r, areas)? {
                    return Ok(BlockReport {
                        pathname: path,
                        outcome: BlockOutcome::Unchanged,
                        record_type: r.info.record_type,
                        info_address: r.info_address,
                        number_data: r.info.number_data,
                        logical_number: r.info.logical_number,
                    });
                }
                let info_address = self.replace(&r, areas)?;
                Ok(BlockReport {
                    pathname: path,
                    outcome: BlockOutcome::Replaced,
                    record_type: areas.data_type,
                    info_address,
                    number_data: areas.number_data,
                    logical_number: areas.logical,
                })
            }
        }
    }

    /// Existing records of blocks `[first, stop)` of this dataset, in block order: one
    /// lookup per block (`zdelete()` looks the canonical pathname up), or one scan of the
    /// bins for long gaps, keeping only entries whose pathname equals a block's canonical
    /// pathname (case-insensitive, as lookups compare).
    fn gap_records(
        &self,
        plan: &ItsPlan,
        first: i64,
        stop: i64,
    ) -> Result<Vec<RecordRef>, WriteError> {
        let bs = plan.path.block_size;
        let mut blocks = Vec::new();
        let mut jb = first;
        let mut small = true;
        while jb < stop {
            if blocks.len() >= GAP_LOOKUP_LIMIT {
                small = false;
                break;
            }
            blocks.push(jb);
            let next = iv::increment_block(jb, bs);
            if next <= jb {
                return Err(bad_input("block arithmetic did not advance"));
            }
            jb = next;
        }
        let paths: Vec<Vec<u8>> = if small {
            blocks
                .iter()
                .map(|&j| plan.path.block_pathname(j))
                .collect()
        } else {
            let mut hits: Vec<(i64, Vec<u8>)> = Vec::new();
            for (_, bin) in self.all_bins()? {
                for e in Self::bin_entries(&bin) {
                    if !(0..=1).contains(&e.2) {
                        continue;
                    }
                    let Some(parts) = path_parts(&e.4) else {
                        continue;
                    };
                    let Some(j) = iv::parse_date(parts[3]) else {
                        continue;
                    };
                    if j < first || j >= stop || iv::irreg_block_start(j, bs) != Some(j) {
                        continue;
                    }
                    let canonical = plan.path.block_pathname(j);
                    if canonical.eq_ignore_ascii_case(&e.4) {
                        hits.push((j, canonical));
                    }
                }
            }
            hits.sort();
            hits.dedup();
            hits.into_iter().map(|(_, p)| p).collect()
        };
        let mut out = Vec::new();
        for p in paths {
            if let Some(r) = self.lookup_for_write(&p)? {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Delete one record (`Internal/zdeleteInternal.c:zdeleteInternal()` lines 157-231)
    /// in the P20 §4 order, adapted (P21 §4): Allocation (marker = −info address, with
    /// `knumberDeletes`, `kcatSortDeletes`, `kcatSortStatus` and `kdead`),
    /// Retire (info status DELETED), Publish (bin status DELETED: the record is gone),
    /// Counters, Finish. Steps that do not apply write nothing.
    fn delete(&mut self, r: &RecordRef) -> Result<BlockReport, WriteError> {
        self.step(WriteStep::Data)?;
        self.step(WriteStep::EndOfFile)?;
        // Allocation: the marker (−info address: a delete) and the counters that only a
        // delete changes and that cannot be rebuilt from the bins (deleted entries can be
        // reused), so reconcile can take them back exactly (P21 review m6).
        let released = released_words(r);
        {
            let h = &mut self.hdr;
            h[k::NUMBER_DELETES] += 1;
            h[k::CAT_SORT_DELETES] += 1;
            if h[k::CAT_SORT_STATUS] > 0 {
                h[k::CAT_SORT_STATUS] = 3;
            }
            h[k::DEAD] += released;
        }
        let mut d = self.disk;
        for key in [
            k::NUMBER_DELETES,
            k::CAT_SORT_DELETES,
            k::CAT_SORT_STATUS,
            k::DEAD,
        ] {
            d[key] = self.hdr[key];
        }
        d[PENDING_WORD] = -r.info_address;
        self.write_header(d)?;
        self.sync()?;
        self.step(WriteStep::Allocation)?;
        self.step(WriteStep::Chain)?;
        self.write_words(
            r.info_address + info::key::STATUS as i64,
            &[status::DELETED],
        )?;
        self.sync()?;
        self.step(WriteStep::Retire)?;
        self.write_words(r.bin_entry_address + 1, &[status::DELETED])?;
        self.sync()?;
        self.step(WriteStep::Publish)?;
        // Counters (lines 219-231): the record and area counts. The released space went
        // to kdead in words at Allocation; reclaim tables are not used (P20 §5).
        let i = &r.info;
        let h = &mut self.hdr;
        h[k::NUMBER_RECORDS] -= 1;
        let dec = |h: &mut [i64; 100], key: usize, n: i64| {
            if n > 0 && h[key] > 0 {
                h[key] -= 1;
            }
        };
        dec(h, k::NUMBER_INTERNAL_HEADER, i.internal_header.number_ints);
        dec(h, k::NUMBER_HEADER2, i.header2.number_ints);
        dec(h, k::NUMBER_DATA_AREA3, i.values3.number_ints);
        dec(h, k::NUMBER_USER_HEADER, i.user_header.number_ints);
        dec(h, k::NUMBER_DATA_AREA1, i.values1.number_ints);
        dec(h, k::NUMBER_DATA_AREA2, i.values2.number_ints);
        self.write_counters()?;
        self.step(WriteStep::Link)?;
        self.step(WriteStep::Activate)?;
        self.finish()?;
        Ok(BlockReport {
            pathname: r.pathname.clone(),
            outcome: BlockOutcome::Deleted,
            record_type: r.info.record_type,
            info_address: 0,
            number_data: 0,
            logical_number: 0,
        })
    }
}

pub(super) fn write_its_planned(
    file: &File,
    input: &ItsInput,
    plan: &ItsPlan,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    // P38: the variant entry point keeps P21's preflight, and P21's path for plain blocks.
    write_its_v(file, input, plan, &Default::default(), opts)
}

// ---------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------

/// One locked write session: the P19 guard, the held lock, reconciliation of an
/// interrupted write, then the caller's transactions (P20 §2.1, §4).
struct Session<'a> {
    file: &'a File,
    eng: Engine<'a>,
    held: Option<lockhold::HeldLock<'a>>,
    readers_undetected: bool,
    reconciled: bool,
    lock_method: &'static str,
}

impl<'a> Session<'a> {
    fn begin(file: &'a File, opts: &'a WriteOptions) -> Result<Self, WriteError> {
        // G1: lock test on our own handle, then the explicit policy (P19a).
        let report = lock::lock_status_of(file).map_err(WriteError::Lock)?;
        let guard = lock::ensure_unlocked_for_write(&report, &opts.lock_policy)
            .map_err(WriteError::Locked)?;
        // Header checks before taking the lock (so a non-DSS7 file is refused cleanly).
        Engine::new(file, opts)?;
        let mut lock_method = "none";
        let held = if opts.lock_policy.hold_primary_lock_during_write {
            let mut head = vec![0u8; header::HEADER_BYTES];
            {
                let mut f = file;
                f.seek(SeekFrom::Start(0))?;
                f.read_exact(&mut head)?;
            }
            let h = Dss7Header::parse(&head, file.metadata()?.len())?;
            let (primary, write_array): (u64, Vec<u64>) = if h.old_locking {
                // Old scheme: fixed primary word 100, no passive arrays
                // (`Internal/zlockingOld.c` lines 127, 170).
                (100 * 8, Vec::new())
            } else {
                let warr = h.words[k::LOCK_WRITE_ARRAY];
                let n = h.words[k::LOCK_ARRAY_SIZES].clamp(0, 1024);
                (
                    h.words[k::LOCK_ADDRESS_WORD] as u64 * 8,
                    (0..n).map(|i| (warr + i) as u64 * 8).collect(),
                )
            };
            match lockhold::take(file, primary, &write_array) {
                Ok(l) => {
                    lock_method = l.method;
                    Some(l)
                }
                Err(lockhold::HoldError::Busy) => {
                    return Err(WriteError::Locked(LockRefusal {
                        reasons: vec![LockReason::PrimaryWrite],
                        readers_undetectable: false,
                        holder_pids: Vec::new(),
                    }))
                }
                Err(lockhold::HoldError::Io(e)) => return Err(e.into()),
            }
        } else {
            None
        };
        // HEC re-reads the header once it holds the lock (zlockActive()).
        let mut eng = Engine::new(file, opts)?;
        let reconciled = eng.reconcile()?;
        Ok(Session {
            file,
            eng,
            held,
            readers_undetected: guard.readers_undetected,
            reconciled,
            lock_method,
        })
    }

    fn finish(self, blocks: Vec<BlockReport>) -> Result<WriteReport, WriteError> {
        if self.eng.opts.sync {
            self.file.sync_all()?;
        }
        drop(self.held);
        Ok(WriteReport {
            blocks,
            readers_undetected: self.readers_undetected,
            reconciled_interrupted_write: self.reconciled,
            lock_method: self.lock_method,
        })
    }
}

pub(super) fn write_planned(
    file: &File,
    input: &RtsInput,
    plan: &Plan,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    // P38: the variant entry point keeps P20's preflight, and P20's path for plain blocks.
    write_rts_v(file, input, plan, &Default::default(), opts)
}

/// P24: store one record whose areas the caller built (a grid), as one P20 §4
/// transaction. `check` sees the existing record, if any, after the lookup and before
/// anything is written, and may refuse the write.
pub(super) fn write_areas(
    file: &File,
    path: &[u8],
    areas: &Areas,
    check: &dyn Fn(Option<&RecordRef>) -> Result<(), WriteError>,
    opts: &WriteOptions,
) -> Result<WriteReport, WriteError> {
    let mut session = Session::begin(file, opts)?;
    let found = session.eng.lookup_for_write(path)?;
    check(found.as_ref())?;
    session.eng.block_index = 0;
    let block = session.eng.commit_areas(path.to_vec(), found, areas)?;
    session.finish(vec![block])
}

/// See [`super::value_times`].
pub(super) fn value_times(pathname: &[u8], start: i64, n: usize) -> Result<Vec<i64>, WriteError> {
    let path = validate_rts_pathname(pathname)?;
    if n == 0 || n > i32::MAX as usize {
        return Err(bad_input("n must be 1..=2^31-1"));
    }
    if start == 0 {
        return Err(bad_input(
            "start 1899-12-31 00:00 is HEC's undefined time (julian 0, second 0)",
        ));
    }
    let mut sj = start.div_euclid(iv::SECS_IN_1_DAY);
    let mut ss = start.rem_euclid(iv::SECS_IN_1_DAY);
    if !year_ok(sj) {
        return Err(bad_input("start year must be 1..=9999"));
    }
    iv::clean_time(&mut sj, &mut ss, iv::SECOND_GRANULARITY);
    Ok((0..n as i64)
        .map(|i| {
            let (j, s) = iv::increment_time(path.interval_seconds, i, sj, ss);
            j * iv::SECS_IN_1_DAY + s
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_file_layout_matches_hecdss_empty_file() {
        // Non-zero words of an empty file created by hecdss 0.1.33 (heclib 7-JA).
        let w = new_file_words(1);
        assert_eq!(w.len(), 15_774);
        let expect: &[(usize, i64)] = &[
            (1, 100),
            (5, 15_773),
            (12, 1),
            (13, 1),
            (14, 100),
            (15, 8192),
            (19, 178),
            (21, 32),
            (22, 32),
            (23, 200),
            (24, 9372),
            (25, 9372),
            (39, 100),
            (42, 8370),
            (43, 8370),
            (45, 20),
            (47, 500),
            (48, 1002),
            (90, 25),
            (91, 103),
            (92, 128),
            (93, 153),
            (99, -97_531),
            (15_773, -97_532),
        ];
        let nonzero: Vec<(usize, i64)> = w
            .iter()
            .enumerate()
            .filter(|&(i, &v)| v != 0 && i != 0 && i != 2)
            .map(|(i, &v)| (i, v))
            .collect();
        assert_eq!(nonzero, expect);
        assert_eq!(&w[0].to_le_bytes()[..5], b"ZDSS\0");
        assert_eq!(&w[2].to_le_bytes()[..5], b"7-JA\0");
    }

    #[test]
    fn plan_blocks() {
        let v = vec![1.0f32; 48];
        // 1Hour from 01Jan2020 01:00: one monthly block.
        let start = (iv::ymd_to_julian(2020, 1, 1)) * 86_400 + 3_600;
        let input = RtsInput::new(
            b"/A/B/C//1Hour/F/",
            start,
            RtsValues::F32(&v),
            b"cfs",
            b"INST-VAL",
        );
        let p = plan(&input).unwrap();
        assert_eq!(p.blocks.len(), 1);
        assert_eq!(p.blocks[0].number_in_block, 744);
        assert_eq!(p.blocks[0].block_start_position, 0);
        assert_eq!(p.granularity, 60);
        assert_eq!(p.time_offset, 0);
        // Across a month boundary.
        let start = iv::ymd_to_julian(2020, 1, 31) * 86_400 + 3_600;
        let input = RtsInput::new(
            b"/A/B/C//1Hour/F/",
            start,
            RtsValues::F32(&v),
            b"cfs",
            b"INST-VAL",
        );
        let p = plan(&input).unwrap();
        assert_eq!(p.blocks.len(), 2);
        assert_eq!(p.blocks[0].block_start_position, 720);
        assert_eq!(p.blocks[0].number_to_store, 24);
        assert_eq!(p.blocks[1].number_in_block, 696);
        assert_eq!(p.blocks[1].number_to_store, 24);
        // Offset: 00:30 hourly keeps 1800 s.
        let start = iv::ymd_to_julian(2020, 1, 1) * 86_400 + 1_800;
        let input = RtsInput::new(
            b"/A/B/C//1Hour/F/",
            start,
            RtsValues::F32(&v),
            b"cfs",
            b"INST-VAL",
        );
        assert_eq!(plan(&input).unwrap().time_offset, 1_800);
        // 15Second: second granularity, daily blocks.
        let start = iv::ymd_to_julian(2025, 6, 1) * 86_400 + 15;
        let input = RtsInput::new(
            b"/A/B/C//15Second/F/",
            start,
            RtsValues::F32(&v),
            b"cfs",
            b"INST-VAL",
        );
        let p = plan(&input).unwrap();
        assert_eq!(p.granularity, 1);
        assert_eq!(p.blocks[0].number_in_block, 5_760);
    }

    #[test]
    fn plan_refusals() {
        let v = vec![1.0f32; 3];
        let ok = iv::ymd_to_julian(2020, 1, 1) * 86_400 + 3_600;
        let bad: Vec<RtsInput> = vec![
            RtsInput::new(
                b"/A/B/C//1Hour/F/",
                0,
                RtsValues::F32(&v),
                b"cfs",
                b"INST-VAL",
            ),
            RtsInput::new(
                b"/A/B/C//1Hour/F/",
                ok,
                RtsValues::F32(&[]),
                b"cfs",
                b"INST-VAL",
            ),
            RtsInput::new(
                b"/A/B/C//1Hour/F/",
                ok,
                RtsValues::F32(&v),
                b"cf\x01s",
                b"INST-VAL",
            ),
            RtsInput::new(
                b"/A/B/C//1Hour/F/",
                iv::ymd_to_julian(10_000, 1, 1) * 86_400,
                RtsValues::F32(&v),
                b"cfs",
                b"I",
            ),
        ];
        for b in &bad {
            assert!(matches!(plan(b), Err(WriteError::BadInput(_))), "{b:?}");
        }
        let mut q = RtsInput::new(
            b"/A/B/C//1Hour/F/",
            ok,
            RtsValues::F32(&v),
            b"cfs",
            b"INST-VAL",
        );
        q.quality = Some(&[1, 2]);
        assert!(plan(&q).is_err());
        let long = "U".repeat(190);
        let s = RtsInput::new(
            b"/A/B/C//1Hour/F/",
            ok,
            RtsValues::F32(&v),
            long.as_bytes(),
            b"INST-VAL",
        );
        assert!(plan(&s).is_err());
    }

    #[test]
    fn irregular_stamps_and_logical_number() {
        let jb = iv::ymd_to_julian(2021, 3, 1);
        // 24:00 of the block's first day.
        assert_eq!(its_stamp(jb, 86_400, false), (jb, 86_400));
        assert_eq!(its_stamp(jb, 3_600, false), (jb, 3_600));
        assert_eq!(its_stamp(jb, 1_440, true), (jb, 86_400));
        // Hourly values 01:00..24:00 of a daily block (days = next -
        // start + 1, `ztsIrregStoreBlock()` line 720).
        let f = (jb, 3_600);
        let l = (jb, 86_400);
        // span = 86400 + 82800 s, average 7050 s.
        assert_eq!(its_logical(jb, jb + 1, f, l, 24), 2 * 86_400 / 7_050);
        assert_eq!(its_logical(jb, jb + 1, f, f, 1), 2);
        // Blocks of 370 days or more work in minutes.
        let y = iv::ymd_to_julian(2020, 1, 1);
        let lg = its_logical(y, iv::ymd_to_julian(2030, 1, 1), (y, 60), (y, 120), 2);
        assert_eq!(lg, (3_654 * 1_440) / ((1_440 + 1) / 2));
    }

    #[test]
    fn irregular_plan_splits_and_gaps() {
        let p = b"/A/B/C//IR-Day/F/";
        let jb = iv::ymd_to_julian(2021, 3, 1);
        let times = [
            jb * 86_400 + 3_600,
            (jb + 1) * 86_400,
            (jb + 5) * 86_400 + 60,
        ];
        let v = [1.0f32, 2.0, 3.0];
        let input = ItsInput::new(p, &times, RtsValues::F32(&v), b"cfs", b"INST-VAL");
        let plan = plan_its(&input).unwrap();
        assert_eq!(plan.granularity, 60);
        let mut stores = Vec::new();
        let mut gaps = Vec::new();
        for s in &plan.steps {
            match s {
                ItsStep::Store(b) => {
                    stores.push((b.julian - jb, b.count, b.read_block, b.from_start, b.to_end))
                }
                ItsStep::DeleteGap { first, stop } => gaps.push((first - jb, stop - jb)),
            }
        }
        // 24:00 of 01Mar stays in the first block; 02..05 Mar are a gap.
        assert_eq!(
            stores,
            vec![(0, 2, true, false, true), (5, 1, true, true, false)]
        );
        assert_eq!(gaps, vec![(1, 5)]);
    }
}

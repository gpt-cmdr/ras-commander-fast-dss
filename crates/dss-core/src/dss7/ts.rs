//! DSS7 time-series reads (packet P08a): single records and stitched datasets.
//!
//! Format rules: `docs/format/dss7.md` §5.1 (info block areas), §7 (time series). The read
//! path is a port of HEC's `ztsRetrieve` chain so that values, times and metadata match
//! `hecdss` bit for bit:
//!
//! | Step | HEC source |
//! |---|---|
//! | E part → interval | `SemiPublic/ztsGetStandardInterval.c:ztsGetStandardInterval()` |
//! | time window | `Internal/ztsProcessTimes.c:ztsProcessTimes()`, `SemiPublic/ztsGetPathTimeWindow.c` |
//! | whole-dataset range | `Public/ztsGetDateRange.c`, `SemiPublic/ztsGetFirstLastPathnames.c`, `SemiPublic/ztsGetFirstLastRecordTimes.c` |
//! | regular blocks | `Internal/ztsRetrieveReg7.c:ztsRetrieveReg7()`, `Internal/ztsRegReadBlock.c` |
//! | irregular blocks | `Internal/ztsRetrieveIrreg7.c:ztsRetrieveIrreg7()`, `Internal/ztsIrregReadBlock.c`, `Utilities/DateTime/isTimeInRange.c` |
//! | expansion | `Internal/ztsDisaggregate.c` (see [`super::compress`]) |
//! | units / type / zone | `Internal/ztsInternalHeaderUnpack.c`, `Utilities/stringHeaderUtility.c:unitsHavePadding()` |
//! | regular times | `Internal/ztsRegAddTimes.c:ztsRegAddTimes()` |
//! | patterns | `Public/ztsRetrievePattern.c:ztsRetrievePattern()` |
//! | trim | `Internal/ztsTrim.c:ztsTrim()` |
//!
//! Values keep their stored bits (missing markers −901, −902 and −FLT_MAX are not
//! converted). Blocks that do not exist read as −FLT_MAX, quality 0
//! (`zsetMissing` / `zsetUndefined`).
//!
//! The reader never writes and never reads outside the data area (all reads go through
//! [`Dss7File::read_words`]); every stored length is checked before use.

use std::fmt;
use std::io::{Read, Seek};

use super::compress::{self, Corrupt, Disaggregate};
use super::info::{Area, InfoBlock};
use super::interval::{self as iv, StandardInterval};
use super::{word, Dss7Catalog, Dss7Error, Dss7File, RecordRef};
use crate::time;
use crate::timeseries::{IrregularBlock, TimeSeries, TsKind, TsValues};

/// `INT_HEAD_SIZE` (`headers/zStructAllocation.h`): HEC copies at most 200 internal
/// header words (`ztsRegReadBlock()`).
const INT_HEAD_SIZE: usize = 200;
/// `INT_HEAD_units`.
const IH_UNITS: usize = 17;
/// `DSS_VERSION 7-HA` (`Internal/ztsDisaggregate.c`): older files use the deprecated
/// values1 layout.
pub const FVER_7_HA: i32 = 70_801;
/// Largest dataset (values) a single call will assemble.
pub const MAX_DATASET_VALUES: i64 = 500_000_000;
/// Largest area (int32 words) read for one record.
const MAX_AREA_INTS: i64 = 1 << 30;

// Record types (`headers/zdataTypeDescriptions.h`).
pub const RTS: i32 = 100;
pub const RTS_PATTERN: i32 = 101;
/// Most depths the reader takes, and so the writer stores (P39 review finding 3).
pub const MAX_PROFILE_DEPTHS: usize = 100_000;
pub const RTS_PROFILE: i32 = 102;
pub const RTD: i32 = 105;
pub const RTD_PATTERN: i32 = 106;
pub const RTD_PROFILE: i32 = 107;
pub const ITS: i32 = 110;
pub const ITS_PATTERN: i32 = 111;
pub const ITS_PROFILE: i32 = 112;
pub const ITD: i32 = 115;
pub const ITD_PATTERN: i32 = 116;
pub const ITD_PROFILE: i32 = 117;

/// Errors from the time-series API.
#[derive(Debug)]
pub enum TsError {
    /// File-level error (I/O, out-of-bounds read, bad info block).
    Dss7(Dss7Error),
    /// No block of the dataset exists in the window (HEC `STATUS_RECORD_NOT_FOUND`).
    NotFound { pathname: Vec<u8> },
    /// The record is not a time series this API decodes (paired data, grid, text,
    /// location, ...; or a profile through the plain time-series call).
    UnsupportedRecordType { pathname: Vec<u8>, record_type: i32 },
    /// Pathname is not `/A/B/C/D/E/F/` or its E part is not a DSS7 interval.
    BadPathname {
        pathname: Vec<u8>,
        reason: &'static str,
    },
    /// The time window is empty or reversed.
    BadWindow,
    /// A stored array or header is inconsistent with its declared sizes.
    Corrupt {
        pathname: Vec<u8>,
        info_address: i64,
        what: &'static str,
    },
    /// The window would produce more than [`MAX_DATASET_VALUES`] values.
    TooManyValues { values: i64 },
}

impl fmt::Display for TsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match self {
            TsError::Dss7(e) => write!(f, "{e}"),
            TsError::NotFound { pathname } => {
                write!(f, "no time-series data found for {}", p(pathname))
            }
            TsError::UnsupportedRecordType {
                pathname,
                record_type,
            } => {
                write!(f, "record {} has type {record_type}, which this reader does not decode as a time series", p(pathname))
            }
            TsError::BadPathname { pathname, reason } => {
                write!(f, "bad pathname {}: {reason}", p(pathname))
            }
            TsError::BadWindow => write!(f, "time window is empty or reversed"),
            TsError::Corrupt {
                pathname,
                info_address,
                what,
            } => {
                write!(
                    f,
                    "record {} (info at word {info_address}) is corrupt: {what}",
                    p(pathname)
                )
            }
            TsError::TooManyValues { values } => write!(
                f,
                "time window spans {values} values (limit {MAX_DATASET_VALUES})"
            ),
        }
    }
}

impl std::error::Error for TsError {}

impl From<Dss7Error> for TsError {
    fn from(e: Dss7Error) -> Self {
        TsError::Dss7(e)
    }
}

/// Decoded TS internal header (`headers/zStructAllocation.h` lines 87–104;
/// `Internal/ztsInternalHeaderUnpack.c`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TsInternalHeader {
    /// Raw words (at most 200, as HEC copies).
    pub words: Vec<i32>,
    pub time_granularity: i32,
    pub precision: i32,
    pub time_offset: i32,
    pub profile_depths: i32,
    pub block_start_position: i32,
    pub block_end_position: i32,
    pub values_number: i32,
    pub value_size: i32,
    pub value_element_size: i32,
    pub values_compression: i32,
    pub quality_number: i32,
    pub quality_element_size: i32,
    pub quality_compression: i32,
    pub inotes_number: i32,
    pub inotes_element_size: i32,
    pub inotes_compression: i32,
    pub cnotes_length: i32,
    /// Units (profiles: the value units).
    pub units: Option<String>,
    /// Profile depth units.
    pub depth_units: Option<String>,
    pub data_type: Option<String>,
    pub time_zone: Option<String>,
    /// Strings start at int 18 (padded, 7-HA and later) instead of int 17.
    pub units_padded: bool,
}

/// Latin-1 decode (lossless for bytes) with HEC's `mallocAndCopyTrim` blank trimming.
fn latin1_trim(b: &[u8]) -> String {
    let s: String = b.iter().map(|&c| c as char).collect();
    s.trim_matches(' ').to_string()
}

impl TsInternalHeader {
    /// Decode `words` (the first `kinfoInternalHeadNumber` int32s, clamped to 200).
    pub fn decode(words: &[i32]) -> Self {
        let w = |i: usize| words.get(i).copied().unwrap_or(0);
        let mut h = TsInternalHeader {
            words: words.to_vec(),
            time_granularity: w(0),
            precision: w(1),
            time_offset: w(2),
            profile_depths: w(3),
            block_start_position: w(4),
            block_end_position: w(5),
            values_number: w(6),
            value_size: w(7),
            value_element_size: w(8),
            values_compression: w(9),
            quality_number: w(10),
            quality_element_size: w(11),
            quality_compression: w(12),
            inotes_number: w(13),
            inotes_element_size: w(14),
            inotes_compression: w(15),
            cnotes_length: w(16),
            ..Default::default()
        };
        // ztsInternalHeaderUnpack(): len = number - INT_HEAD_units; nothing when len <= 1.
        let n = words.len();
        if n <= IH_UNITS + 1 {
            return h;
        }
        let len = n - IH_UNITS;
        // unitsHavePadding(): int 17 is 0 or four blanks.
        let i17 = w(IH_UNITS);
        h.units_padded = i17 == 0 || i17.to_le_bytes() == *b"    ";
        let (start, nbytes) = if h.units_padded {
            (IH_UNITS + 1, (len - 1) * 4)
        } else {
            (IH_UNITS, len * 4)
        };
        // carray[200] in ztsInternalHeaderUnpack (charInt / charLong truncate to it).
        let nbytes = nbytes.min(200);
        let bytes: Vec<u8> = words[start..]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .take(nbytes)
            .collect();
        let cstr = |pos: usize| -> &[u8] {
            let s = bytes.get(pos..).unwrap_or(&[]);
            let e = s.iter().position(|&b| b == 0).unwrap_or(s.len());
            &s[..e]
        };
        let nonblank = |s: &[u8]| s.iter().any(|&b| b != b' ');
        let mut ipos = 0usize;
        let s = cstr(ipos);
        // As in HEC, an empty first string leaves ipos at 0, so type and zone are empty too.
        if !s.is_empty() {
            if h.profile_depths == 0 {
                if nonblank(s) {
                    h.units = Some(latin1_trim(s));
                }
                ipos += s.len() + 1;
            } else {
                if nonblank(s) {
                    h.depth_units = Some(latin1_trim(s));
                }
                ipos += s.len() + 1;
                let s2 = cstr(ipos);
                if nonblank(s2) {
                    h.units = Some(latin1_trim(s2));
                }
                ipos += s2.len() + 1;
            }
        }
        let s = cstr(ipos);
        if !s.is_empty() {
            if nonblank(s) {
                h.data_type = Some(latin1_trim(s));
            }
            ipos += s.len() + 1;
        }
        let s = cstr(ipos);
        if !s.is_empty() && nonblank(s) {
            h.time_zone = Some(latin1_trim(s));
        }
        h
    }
}

/// A time window in seconds since the HEC epoch (see [`crate::time`]), both ends
/// inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsWindow {
    pub start: i64,
    pub end: i64,
}

/// Options for [`Dss7File::read_ts`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TsReadOptions {
    /// Window to read. `None` reads the whole dataset: HEC's "retrieve all times"
    /// (`boolRetrieveAllTimes`, what `hec_dss_tsRetrieve` does with no dates): from the
    /// day of the first valid value of the first block to the end of the day of the last
    /// valid value of the last block (bin dates, `ztsGetFirstLastRecordTimes()`).
    pub window: Option<TsWindow>,
    /// Remove leading and trailing missing values (regular data only), as
    /// `ztsRetrieve(retrieveFlag = -1)` does. This is what HEC Monolith's
    /// `HecDss.get(path, true)` (used by ras-commander's `RasDss`) returns.
    pub trim_missing: bool,
}

/// Extra DSS7 facts for a read.
#[derive(Debug, Clone, PartialEq)]
pub struct TsMeta {
    pub interval: StandardInterval,
    /// Granularity HEC uses for the time array (stored internal-header value when > 0).
    pub time_granularity_seconds: i64,
    /// HEC's `julianBaseDate` for the time array.
    pub julian_base_date: i64,
    pub precision: i32,
    /// Record time offset (regular data), seconds into the interval.
    pub time_offset_seconds: i64,
    /// Words per quality value (0 when none).
    pub quality_element_size: usize,
    /// All quality words when `quality_element_size > 1` (`n × size`).
    pub quality_words: Option<Vec<i32>>,
    /// Integer notes (`n × inotes_element_size`) when stored.
    pub inotes: Option<Vec<i32>>,
    pub inotes_element_size: usize,
    /// Character notes, one per value, when stored.
    pub cnotes: Option<Vec<String>>,
    /// User header of the first block found.
    pub user_header: Vec<i32>,
    /// Internal header of the first block found.
    pub internal_header: TsInternalHeader,
    /// Pattern record (101/106; times are HEC's 01Jan3000 pseudo-dates).
    pub pattern: bool,
    /// Blocks in the window, and blocks actually found.
    pub blocks_in_window: usize,
    pub blocks_found: usize,
    /// Pathname of every block found.
    pub block_pathnames: Vec<Vec<u8>>,
}

/// A decoded read: the shared model plus DSS7 extras.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss7Ts {
    pub ts: TimeSeries,
    pub meta: TsMeta,
}

/// A regular-interval profile record (types 102/107): one row of `depths.len()` values
/// per time.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss7Profile {
    pub pathname: Vec<u8>,
    pub record_type: i32,
    pub times: Vec<i64>,
    pub depths: TsValues,
    /// Row-major, `times.len() × depths.len()`.
    pub values: TsValues,
    pub depth_units: String,
    pub value_units: String,
    pub data_type: String,
    pub time_zone: Option<String>,
    pub meta: TsMeta,
}

/// One record's areas, as `zreadInternal()` returns them.
struct Loaded {
    rec: RecordRef,
    ih_words: Vec<i32>,
    header2: Vec<i32>,
    user_header: Vec<i32>,
    values1: Vec<i32>,
    values2: Vec<i32>,
}

/// Pathname split into its six parts (`Public/zpathnameGetPart.c`).
pub(crate) fn path_parts(p: &[u8]) -> Option<[&[u8]; 6]> {
    if p.len() < 7 || p[0] != b'/' || p[p.len() - 1] != b'/' {
        return None;
    }
    let inner = &p[1..p.len() - 1];
    let parts: Vec<&[u8]> = inner.split(|&b| b == b'/').collect();
    if parts.len() != 6 {
        return None;
    }
    Some([parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]])
}

fn join_path(parts: [&[u8]; 6]) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.push(b'/');
    for p in parts {
        v.extend_from_slice(p);
        v.push(b'/');
    }
    v
}

fn ints_from_bytes(b: &[u8], n: usize) -> Vec<i32> {
    b.chunks_exact(4)
        .take(n)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Window and block layout resolved like `ztsProcessTimes()`.
#[derive(Debug, Clone, Copy)]
struct Window {
    start_j: i64,
    start_s: i64,
    end_j: i64,
    end_s: i64,
    /// Regular: number of values; irregular: unused.
    number_values: i64,
    block_size: i64,
    start_block: i64,
    end_block: i64,
    /// Granularity `ztsProcessTimes()` picks before any record is read.
    granularity: i64,
    julian_base: i64,
}

#[derive(Clone, Copy)]
enum WindowSource {
    /// From the pathname D part (`ztsGetPathTimeWindow()`), as for a single-record read.
    PathD(i64),
    /// Explicit start and end `(julian, seconds)`.
    Explicit((i64, i64), (i64, i64)),
}

fn julian_ok(j: i64) -> bool {
    (-iv::JULIAN_LIMIT..=iv::JULIAN_LIMIT).contains(&j)
}

fn resolve_window(interval: &StandardInterval, src: WindowSource) -> Result<Window, TsError> {
    let code = interval.code;
    let (mut sj, mut ss, mut ej, mut es, mut nv);
    let block_size;
    match src {
        WindowSource::PathD(dj) => {
            // ztsGetPathTimeWindow(): one block from the D date.
            if !julian_ok(dj) {
                return Err(TsError::BadWindow);
            }
            sj = dj;
            es = iv::SECS_IN_1_DAY;
            if code > 0 {
                ss = if code < iv::SECS_IN_1_DAY {
                    code
                } else {
                    iv::SECS_IN_1_DAY
                };
                let (sb, bs) = iv::reg_block_start(dj, code);
                block_size = bs;
                ej = iv::increment_block(sb, bs) - 1;
                // numberPeriods(intervalSeconds, startJulian, intervalSeconds, ...) + 1:
                // HEC passes the interval as the start's seconds.
                nv = iv::number_periods(code, sj, code, ej, es) + 1;
            } else {
                ss = 1;
                block_size = -code;
                let sb = iv::irreg_block_start(dj, block_size).ok_or(TsError::BadWindow)?;
                ej = iv::increment_block(sb, block_size) - 1;
                nv = 0;
            }
        }
        WindowSource::Explicit(s, e) => {
            (sj, ss) = s;
            (ej, es) = e;
            if !julian_ok(sj) || !julian_ok(ej) {
                return Err(TsError::BadWindow);
            }
            iv::clean_time(&mut sj, &mut ss, iv::SECOND_GRANULARITY);
            iv::clean_time(&mut ej, &mut es, iv::SECOND_GRANULARITY);
            if (ej, es) < (sj, ss) {
                return Err(TsError::BadWindow);
            }
            block_size = if code > 0 {
                iv::reg_block_start(sj, code).1
            } else {
                -code
            };
            nv = 0;
        }
    }
    let (start_block, end_block);
    if code > 0 {
        iv::offset_adjust_to_standard(code, &mut sj, &mut ss);
        iv::offset_adjust_to_standard(code, &mut ej, &mut es);
        start_block = iv::reg_block_start(sj, code).0;
        end_block = iv::reg_block_start(ej, code).0;
        if nv <= 0 {
            nv = iv::number_periods(code, sj, ss, ej, es) + 1;
        }
        if nv <= 0 {
            return Err(TsError::BadWindow);
        }
        if nv > MAX_DATASET_VALUES {
            return Err(TsError::TooManyValues { values: nv });
        }
    } else {
        start_block = iv::irreg_block_start(sj, block_size).ok_or(TsError::BadWindow)?;
        end_block = iv::irreg_block_start(ej, block_size).ok_or(TsError::BadWindow)?;
    }
    // ztsProcessTimes(): granularity and julian base date for the time array.
    let granularity = if code > 0 && code < iv::SECS_IN_1_MINUTE {
        iv::SECOND_GRANULARITY
    } else {
        let days = ej - sj;
        if days < 1_000_000 {
            iv::MINUTE_GRANULARITY
        } else if days < 30_000_000 {
            iv::HOUR_GRANULARITY
        } else {
            iv::DAY_GRANULARITY
        }
    };
    let mut julian_base = 0;
    if start_block != 0 {
        if granularity == iv::SECOND_GRANULARITY {
            julian_base = start_block;
        } else if granularity == iv::MINUTE_GRANULARITY {
            if !(-365 * 1000..=365 * 3100).contains(&sj) || ej > 365 * 3200 {
                julian_base = start_block;
            }
        } else {
            julian_base = start_block;
        }
    }
    Ok(Window {
        start_j: sj,
        start_s: ss,
        end_j: ej,
        end_s: es,
        number_values: nv,
        block_size,
        start_block,
        end_block,
        granularity,
        julian_base,
    })
}

/// `isTimeInRange()` with HEC's sticky `lastValueStatus`.
fn in_range(t: i64, last: &mut i32, g: i64, base: i64, w: &Window) -> bool {
    let g = if g < 1 { iv::MINUTE_GRANULARITY } else { g };
    if *last < 0 {
        let lt = (w.start_j - base) * (iv::SECS_IN_1_DAY / g) + w.start_s / g;
        if t < lt {
            return false;
        }
    }
    let lt = (w.end_j - base) * (iv::SECS_IN_1_DAY / g) + w.end_s / g;
    if t > lt {
        *last = 1;
        return false;
    }
    *last = 0;
    true
}

/// HEC's `zisMissingFloat` / `zisMissingDouble` (−FLT_MAX, −901, −902; not NaN):
/// [`crate::is_hec_missing`].
fn hec_missing(v: f64) -> bool {
    crate::is_hec_missing(v)
}

fn words_to_values(words: &[i32], elem: usize) -> TsValues {
    if elem == 2 {
        TsValues::F64(
            words
                .chunks_exact(2)
                .map(|c| f64::from_bits((c[0] as u32 as u64) | ((c[1] as u32 as u64) << 32)))
                .collect(),
        )
    } else {
        TsValues::F32(words.iter().map(|&w| f32::from_bits(w as u32)).collect())
    }
}

fn cnotes_split(bytes: &[u8], n: usize) -> Vec<String> {
    let mut out: Vec<String> = bytes
        .split(|&b| b == 0)
        .take(n)
        .map(|s| s.iter().map(|&c| c as char).collect())
        .collect();
    out.resize(n, String::new());
    out
}

/// Assembled arrays before conversion to the public model.
struct Assembled {
    pathname: Vec<u8>,
    record_type: i32,
    times: Vec<i64>,
    values: Vec<i32>,
    elem: usize,
    depths: usize,
    quality: Vec<i32>,
    qes: usize,
    inotes: Vec<i32>,
    nes: usize,
    cnotes: Option<Vec<u8>>,
    first: Option<Loaded>,
    profile_depths_words: Vec<i32>,
    granularity: i64,
    julian_base: i64,
    blocks_in_window: usize,
    block_pathnames: Vec<Vec<u8>>,
}

impl<R: Read + Seek> Dss7File<R> {
    fn corrupt(rec: &RecordRef, what: &'static str) -> TsError {
        TsError::Corrupt {
            pathname: rec.pathname.clone(),
            info_address: rec.info_address,
            what,
        }
    }

    /// Read one info-block area as int32 words (`zreadInternal()`).
    fn read_area(&mut self, rec: &RecordRef, a: Area, cap: usize) -> Result<Vec<i32>, TsError> {
        if a.number_ints <= 0 {
            return Ok(Vec::new());
        }
        if a.number_ints > MAX_AREA_INTS {
            return Err(Self::corrupt(rec, "area length out of range"));
        }
        let n = (a.number_ints as usize).min(cap);
        let words = (n as i64 + 1) / 2;
        let bytes = self.read_words(a.address, words).map_err(|e| match e {
            Dss7Error::ReadOutOfBounds { .. } => Self::corrupt(rec, "data area outside the file"),
            other => TsError::Dss7(other),
        })?;
        Ok(ints_from_bytes(&bytes, n))
    }

    fn load(&mut self, rec: RecordRef) -> Result<Loaded, TsError> {
        let info: InfoBlock = rec.info.clone();
        let ih_words = self.read_area(&rec, info.internal_header, INT_HEAD_SIZE)?;
        let header2 = self.read_area(&rec, info.header2, usize::MAX)?;
        let user_header = self.read_area(&rec, info.user_header, usize::MAX)?;
        let values1 = self.read_area(&rec, info.values1, usize::MAX)?;
        let values2 = self.read_area(&rec, info.values2, usize::MAX)?;
        Ok(Loaded {
            rec,
            ih_words,
            header2,
            user_header,
            values1,
            values2,
        })
    }

    fn deprecated_layout(&self) -> bool {
        self.header().fver < FVER_7_HA
    }

    /// Read one time-series record (one block), exactly as `hec_dss_tsRetrieve` reads a
    /// full pathname with no time window: the window is the record's block (from its D
    /// part), regular blocks are expanded to every interval of the block (missing where
    /// nothing is stored), irregular blocks return the values stored in the block.
    ///
    /// Use this for catalog entries (including recovered ones, [`Dss7File::record_at`]).
    pub fn read_ts_record(&mut self, rec: &RecordRef) -> Result<Dss7Ts, TsError> {
        let t = rec.record_type;
        if matches!(t, RTS_PROFILE | RTD_PROFILE | ITS_PROFILE | ITD_PROFILE)
            || !(100..200).contains(&t)
        {
            return Err(TsError::UnsupportedRecordType {
                pathname: rec.pathname.clone(),
                record_type: t,
            });
        }
        let a = self.read_record_inner(rec)?;
        self.finish(a, false, rec.record_type)
    }

    /// Read a regular-interval profile record (types 102/107) as `ztsRetrieveReg7()` does.
    pub fn read_ts_profile_record(&mut self, rec: &RecordRef) -> Result<Dss7Profile, TsError> {
        if !matches!(rec.record_type, RTS_PROFILE | RTD_PROFILE) {
            return Err(TsError::UnsupportedRecordType {
                pathname: rec.pathname.clone(),
                record_type: rec.record_type,
            });
        }
        let a = self.read_record_inner(rec)?;
        let depths_n = a.depths;
        let elem = a.elem;
        let depth_words = a.profile_depths_words.clone();
        let mut r = self.finish(a, true, rec.record_type)?;
        // P14: single-word quality also goes in `meta.quality_words` (as read_ts_profile).
        profile_quality_to_meta(&mut r);
        let ih = &r.meta.internal_header;
        let depths = words_to_values(
            &depth_words[..(depths_n * elem).min(depth_words.len())],
            elem,
        );
        Ok(Dss7Profile {
            pathname: r.ts.pathname.clone(),
            record_type: r.ts.record_type,
            times: r.ts.times.clone(),
            depths,
            values: r.ts.values.clone(),
            depth_units: ih.depth_units.clone().unwrap_or_default(),
            value_units: ih.units.clone().unwrap_or_default(),
            data_type: ih.data_type.clone().unwrap_or_default(),
            time_zone: ih.time_zone.clone(),
            meta: r.meta,
        })
    }

    fn read_record_inner(&mut self, rec: &RecordRef) -> Result<Assembled, TsError> {
        let parts = path_parts(&rec.pathname).ok_or(TsError::BadPathname {
            pathname: rec.pathname.clone(),
            reason: "not /A/B/C/D/E/F/",
        })?;
        let interval = standard_interval_or_err(&rec.pathname, parts[4])?;
        if matches!(
            rec.record_type,
            RTS_PATTERN | RTD_PATTERN | ITS_PATTERN | ITD_PATTERN
        ) {
            return self.read_pattern(rec.clone(), &interval);
        }
        let dj = iv::parse_date(parts[3]).ok_or(TsError::BadPathname {
            pathname: rec.pathname.clone(),
            reason: "D part is not a date",
        })?;
        let w = resolve_window(&interval, WindowSource::PathD(dj))?;
        self.read_blocks(&rec.pathname, &interval, &w, Some(rec))
    }

    /// Read a dataset: `pathname` with any (usually blank) D part and an optional time
    /// window, stitched from every block HEC would read (`ztsRetrieve`).
    ///
    /// Without a window, the dataset's range comes from a catalog walk (see
    /// [`TsReadOptions::window`]); use [`Dss7File::read_ts_with_catalog`] to reuse one.
    pub fn read_ts(&mut self, pathname: &[u8], opts: &TsReadOptions) -> Result<Dss7Ts, TsError> {
        if opts.window.is_none() {
            let cat = self.catalog()?;
            return self.read_ts_with_catalog(&cat, pathname, opts);
        }
        self.read_ts_impl(None, pathname, opts)
    }

    /// [`Dss7File::read_ts`] with a prebuilt catalog for the whole-dataset range.
    pub fn read_ts_with_catalog(
        &mut self,
        cat: &Dss7Catalog,
        pathname: &[u8],
        opts: &TsReadOptions,
    ) -> Result<Dss7Ts, TsError> {
        self.read_ts_impl(Some(cat), pathname, opts)
    }

    fn read_ts_impl(
        &mut self,
        cat: Option<&Dss7Catalog>,
        pathname: &[u8],
        opts: &TsReadOptions,
    ) -> Result<Dss7Ts, TsError> {
        let parts = path_parts(pathname).ok_or(TsError::BadPathname {
            pathname: pathname.to_vec(),
            reason: "not /A/B/C/D/E/F/",
        })?;
        let interval = standard_interval_or_err(pathname, parts[4])?;
        let ename = canonical_e(&interval);
        let mut p = parts;
        p[4] = ename.as_bytes();
        let canon = join_path(p);
        // Pattern records (`isTsPattern()`: D part starting with "TS-P").
        if parts[3].len() > 4 && parts[3][..4].eq_ignore_ascii_case(b"TS-P") {
            let rec = self.lookup(&canon)?.ok_or_else(|| TsError::NotFound {
                pathname: pathname.to_vec(),
            })?;
            if !matches!(
                rec.record_type,
                RTS_PATTERN | RTD_PATTERN | ITS_PATTERN | ITD_PATTERN
            ) {
                return Err(TsError::UnsupportedRecordType {
                    pathname: rec.pathname.clone(),
                    record_type: rec.record_type,
                });
            }
            let a = self.read_pattern(rec, &interval)?;
            let t = a.record_type;
            return self.finish(a, false, t);
        }
        let w = match opts.window {
            Some(win) => {
                if win.end < win.start {
                    return Err(TsError::BadWindow);
                }
                let s = (
                    win.start.div_euclid(time::DAY),
                    win.start.rem_euclid(time::DAY),
                );
                let e = (win.end.div_euclid(time::DAY), win.end.rem_euclid(time::DAY));
                resolve_window(&interval, WindowSource::Explicit(s, e))?
            }
            None => {
                let cat = cat.ok_or(TsError::BadWindow)?;
                let (fj, lj) = self
                    .dataset_date_range(cat, &canon, &interval)?
                    .ok_or_else(|| TsError::NotFound {
                        pathname: pathname.to_vec(),
                    })?;
                // ztsProcessTimes() boolRetrieveAllTimes branch.
                let ss = if interval.code >= iv::SECS_IN_1_MINUTE {
                    iv::SECS_IN_1_MINUTE
                } else {
                    1
                };
                let (mut sj, mut ss, mut ej, mut es) = (fj, ss, lj, iv::SECS_IN_1_DAY);
                if interval.code > 0 {
                    iv::offset_adjust_to_standard(interval.code, &mut sj, &mut ss);
                    iv::offset_adjust_to_standard(interval.code, &mut ej, &mut es);
                }
                resolve_window(&interval, WindowSource::Explicit((sj, ss), (ej, es)))?
            }
        };
        let mut a = self.read_blocks(&canon, &interval, &w, None)?;
        if a.depths > 0 {
            return Err(TsError::UnsupportedRecordType {
                pathname: a.pathname.clone(),
                record_type: a.record_type,
            });
        }
        if opts.trim_missing && interval.code > 0 {
            trim(&mut a);
        }
        let t = a.record_type;
        self.finish(a, false, t)
    }

    /// `ztsGetDateRange(path, boolFullSet = 1)`: first and last julian day of the dataset.
    fn dataset_date_range(
        &mut self,
        cat: &Dss7Catalog,
        canon: &[u8],
        interval: &StandardInterval,
    ) -> Result<Option<(i64, i64)>, TsError> {
        let want = path_parts(canon).ok_or(TsError::BadWindow)?;
        // ztsGetFirstLastPathnames(): catalog with D = "*", types 100..199.
        let mut range: Option<(i64, i64)> = None;
        for (i, e) in cat.catalog.entries.iter().enumerate() {
            let st = cat.meta.get(i).map(|m| m.raw_status).unwrap_or(-1);
            if !(0..=2).contains(&st) || !(100..200).contains(&e.record_type) {
                continue;
            }
            let Some(p) = path_parts(&e.pathname) else {
                continue;
            };
            let same = [0usize, 1, 2, 4, 5]
                .iter()
                .all(|&k| p[k].eq_ignore_ascii_case(want[k]));
            if !same {
                continue;
            }
            // HEC's dateToJulian() would yield UNDEFINED_TIME for a non-date D part;
            // such entries are skipped here.
            let Some(j) = iv::parse_date(p[3]) else {
                continue;
            };
            range = Some(match range {
                None => (j, j),
                Some((a, b)) => (a.min(j), b.max(j)),
            });
        }
        let Some((fj, lj)) = range else {
            return Ok(None);
        };
        let mut pf = want;
        let dfirst = iv::julian_to_d_part(fj);
        pf[3] = dfirst.as_bytes();
        let path_first = join_path(pf);
        let dlast = iv::julian_to_d_part(lj);
        let mut pl = want;
        pl[3] = dlast.as_bytes();
        let path_last = join_path(pl);
        // ztsGetFirstLastRecordTimes(..., boolGetSecondsAlso = 0): bin dates.
        let first_j = match self.bin_dates(&path_first)? {
            Some((a, _)) => a,
            None => fj,
        };
        let last_j = match self.bin_dates(&path_last)? {
            Some((_, b)) => b,
            None => {
                // ztsGetDateRange(): D date of the last block plus its block length.
                if interval.code > 0 {
                    let bs = iv::reg_block_start(lj, interval.code).1;
                    iv::increment_block(lj, bs) - 1
                } else {
                    iv::increment_block(lj, -interval.code) - 1
                }
            }
        };
        Ok(Some((first_j, last_j)))
    }

    /// Bin `kbinDates` (julian first, last) of the record a lookup finds.
    fn bin_dates(&mut self, path: &[u8]) -> Result<Option<(i64, i64)>, TsError> {
        let rec = match self.lookup(path) {
            Ok(Some(r)) => r,
            Ok(None) | Err(Dss7Error::BadInfoBlock { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !(100..200).contains(&rec.record_type) {
            return Ok(None);
        }
        let b = self.read_words(rec.bin_entry_address + super::bin::DATES as i64, 1)?;
        let (a, z) = super::info::split_i4(word(&b, 0));
        Ok(Some((i64::from(a), i64::from(z))))
    }

    fn lookup_block(&mut self, path: &[u8]) -> Result<Option<RecordRef>, TsError> {
        match self.lookup(path) {
            Ok(r) => Ok(r),
            // zreadInfo() fails for a zeroed or reused info block; HEC treats the block as
            // missing only when not found, so this is an error, as in hecdss.
            Err(e) => Err(e.into()),
        }
    }

    /// Read every block of the window (`ztsRetrieveReg7` / `ztsRetrieveIrreg7`).
    /// `given` supplies the record for the window's single block (record reads).
    fn read_blocks(
        &mut self,
        canon: &[u8],
        interval: &StandardInterval,
        w: &Window,
        given: Option<&RecordRef>,
    ) -> Result<Assembled, TsError> {
        let parts = path_parts(canon).ok_or(TsError::BadWindow)?;
        // Enumerate the blocks and look each one up (ztsGetSizesInternal() pass).
        let mut blocks: Vec<(i64, Vec<u8>, Option<RecordRef>)> = Vec::new();
        let mut j = w.start_block;
        let max_blocks = 2_000_000usize;
        while j <= w.end_block {
            if blocks.len() >= max_blocks {
                return Err(TsError::TooManyValues {
                    values: blocks.len() as i64,
                });
            }
            let d = iv::julian_to_d_part(j);
            let mut p = parts;
            p[3] = d.as_bytes();
            let path = join_path(p);
            let rec = match given {
                Some(g) if blocks.is_empty() => Some(g.clone()),
                Some(_) => None,
                None => self.lookup_block(&path)?,
            };
            let rec = rec.filter(|r| (100..200).contains(&r.record_type));
            blocks.push((j, path, rec));
            let next = iv::increment_block(j, w.block_size);
            if next <= j {
                break;
            }
            j = next;
        }
        let found: Vec<&RecordRef> = blocks.iter().filter_map(|b| b.2.as_ref()).collect();
        if found.is_empty() {
            return Err(TsError::NotFound {
                pathname: canon.to_vec(),
            });
        }
        // Sizes over all blocks found (ztsGetSizesInternal): data type of the last, and
        // the largest element sizes.
        let mut data_type = 0;
        let (mut elem, mut qes, mut nes, mut depths, mut cnotes_total) =
            (0usize, 0usize, 0usize, 0usize, 0i64);
        let mut sum_number = 0i64;
        let mut headers: Vec<Option<Vec<i32>>> = Vec::with_capacity(blocks.len());
        for b in &blocks {
            if let Some(r) = &b.2 {
                let ih = self.read_area(r, r.info.internal_header, INT_HEAD_SIZE)?;
                let g = |i: usize| ih.get(i).copied().unwrap_or(0).max(0) as usize;
                data_type = r.record_type;
                elem = elem.max(g(compress::IH_VALUE_ELEMENT_SIZE));
                qes = qes.max(g(compress::IH_QUALITY_ELEMENT_SIZE));
                nes = nes.max(g(compress::IH_INOTES_ELEMENT_SIZE));
                depths = g(compress::IH_PROFILE_DEPTHS);
                cnotes_total += g(compress::IH_CNOTES_LENGTH) as i64;
                sum_number = sum_number.saturating_add(r.info.number_data.max(0));
                headers.push(Some(ih));
            } else {
                headers.push(None);
            }
        }
        if !(1..=2).contains(&elem) {
            // HEC requests floats unless an element size of 2 is seen.
            elem = if elem == 0 { 1 } else { 2 };
        }
        if qes > 64 || nes > 64 {
            return Err(Self::corrupt(
                found[0],
                "quality or notes element size out of range",
            ));
        }
        let profile = matches!(
            data_type,
            RTS_PROFILE | RTD_PROFILE | ITS_PROFILE | ITD_PROFILE
        );
        let depths = if profile { depths } else { 0 };
        if depths > MAX_PROFILE_DEPTHS {
            return Err(Self::corrupt(found[0], "profile depth count out of range"));
        }
        let want_c = cnotes_total > 0;
        let deprecated = self.deprecated_layout();
        let stride = elem * depths.max(1);
        let mut out = Assembled {
            pathname: found[0].pathname.clone(),
            record_type: data_type,
            times: Vec::new(),
            values: Vec::new(),
            elem,
            depths,
            quality: Vec::new(),
            qes,
            inotes: Vec::new(),
            nes,
            cnotes: if want_c { Some(Vec::new()) } else { None },
            first: None,
            profile_depths_words: Vec::new(),
            granularity: w.granularity,
            julian_base: w.julian_base,
            blocks_in_window: blocks.len(),
            block_pathnames: found.iter().map(|r| r.pathname.clone()).collect(),
        };
        let regular = interval.code > 0;
        if regular {
            if w.number_values > MAX_DATASET_VALUES {
                return Err(TsError::TooManyValues {
                    values: w.number_values,
                });
            }
            out.values.reserve(
                (w.number_values as usize)
                    .saturating_mul(stride)
                    .min(1 << 26),
            );
            let mut current: i64 = 0;
            let n_total = w.number_values;
            for (bi, (bj, _path, rec)) in blocks.iter().enumerate() {
                let next = iv::increment_block(*bj, w.block_size);
                let in_block = iv::number_periods(
                    interval.code,
                    bj - 1,
                    iv::SECS_IN_1_DAY,
                    next - 1,
                    iv::SECS_IN_1_DAY,
                );
                let rel = iv::number_periods(
                    interval.code,
                    bj - 1,
                    iv::SECS_IN_1_DAY,
                    w.start_j,
                    w.start_s,
                ) - 1;
                let mut bsp = rel + current;
                if interval.code == iv::SECS_IN_1_WEEK && bsp == -1 {
                    bsp = 0;
                }
                if bsp < 0 {
                    return Err(TsError::BadWindow);
                }
                let to_read = (in_block - bsp).min(n_total - current).max(0);
                match (rec, &headers[bi]) {
                    (Some(r), Some(_)) => {
                        let l = self.load(r.clone())?;
                        let ih = &l.ih_words;
                        if ih.len() < 5 || !(RTS..ITS).contains(&r.record_type) {
                            return Err(Self::corrupt(r, "invalid time-series record header"));
                        }
                        let mut ihp = ih.clone();
                        ihp.resize(INT_HEAD_SIZE, 0);
                        let prfv = bsp - i64::from(ihp[4]);
                        let prlv = to_read + prfv - (i64::from(ihp[5]) - i64::from(ihp[4]) + 1);
                        let d = Disaggregate {
                            data_in: &l.values1,
                            data_in_position: 0,
                            header2: &l.header2,
                            ih: &ihp,
                            number_to_read: to_read,
                            number_stored: r.info.number_data,
                            position_relative_first_valid: prfv,
                            position_relative_last_valid: prlv,
                            deprecated,
                            values_requested: elem,
                            quality_requested: qes,
                            inotes_requested: nes,
                            want_cnotes: want_c,
                        };
                        let x = d.run().map_err(|Corrupt(what)| Self::corrupt(r, what))?;
                        append_block(&mut out, x, to_read as usize, stride);
                        if out.first.is_none() {
                            if profile {
                                out.profile_depths_words = l.values2.clone();
                                if l.values2.len() < depths * elem {
                                    return Err(Self::corrupt(
                                        r,
                                        "profile depths shorter than the depth count",
                                    ));
                                }
                            }
                            out.first = Some(l);
                        }
                    }
                    _ => {
                        // Record does not exist: missing values, zero quality and notes.
                        missing_block(&mut out, to_read as usize, stride, elem);
                    }
                }
                current += to_read;
            }
            // ztsRegAddTimes(), using the record's granularity and offset.
            let ih = out
                .first
                .as_ref()
                .map(|l| TsInternalHeader::decode(&l.ih_words))
                .ok_or_else(|| TsError::NotFound {
                    pathname: canon.to_vec(),
                })?;
            if ih.time_granularity > 0 {
                out.granularity = i64::from(ih.time_granularity);
            }
            let offset = i64::from(ih.time_offset).max(0);
            out.times = reg_times(
                interval.code,
                w,
                offset,
                out.granularity,
                out.julian_base,
                current,
            );
        } else {
            // Irregular: ztsRetrieveIrreg7() / ztsIrregReadBlock().
            let block_gran = if w.block_size < iv::BLOCK_1_CENTURY {
                iv::SECOND_GRANULARITY
            } else {
                iv::MINUTE_GRANULARITY
            };
            let max_values = sum_number;
            let mut current: i64 = 0;
            let mut gran_set = false;
            for (bi, (bj, _path, rec)) in blocks.iter().enumerate() {
                let (Some(r), Some(_)) = (rec, &headers[bi]) else {
                    continue;
                };
                let l = self.load(r.clone())?;
                if l.ih_words.len() < 5 || !(ITS..200).contains(&r.record_type) {
                    return Err(Self::corrupt(r, "invalid time-series record header"));
                }
                let mut ihp = l.ih_words.clone();
                ihp.resize(INT_HEAD_SIZE, 0);
                let number_stored = r.info.number_data;
                if number_stored < 0 || number_stored as usize > l.values1.len() {
                    return Err(Self::corrupt(
                        r,
                        "stored value count exceeds the values area",
                    ));
                }
                // HEC sizes this read for every value left in the window; only the
                // block's own values can be expanded, so the smaller count gives the same
                // result without the quadratic allocation.
                let max_to_read = (max_values - current).max(0).min(number_stored);
                let d = Disaggregate {
                    data_in: &l.values1,
                    data_in_position: number_stored as usize,
                    header2: &l.header2,
                    ih: &ihp,
                    number_to_read: max_to_read,
                    number_stored,
                    position_relative_first_valid: 0,
                    position_relative_last_valid: 0,
                    deprecated,
                    values_requested: elem,
                    quality_requested: qes,
                    inotes_requested: nes,
                    want_cnotes: want_c,
                };
                let x = d.run().map_err(|Corrupt(what)| Self::corrupt(r, what))?;
                if out.first.is_none() {
                    let ih = TsInternalHeader::decode(&l.ih_words);
                    if ih.time_granularity > 0 {
                        out.granularity = i64::from(ih.time_granularity);
                    }
                    if out.julian_base < 0
                        || (out.granularity == iv::SECOND_GRANULARITY && out.julian_base == 0)
                    {
                        out.julian_base = *bj;
                    }
                    gran_set = true;
                }
                let g = if out.granularity == 0 {
                    iv::MINUTE_GRANULARITY
                } else {
                    out.granularity
                };
                let base_to_start = (bj - out.julian_base)
                    * match out.granularity {
                        iv::SECOND_GRANULARITY => iv::SECS_IN_1_DAY,
                        iv::MINUTE_GRANULARITY => 1_440,
                        iv::HOUR_GRANULARITY => 24,
                        _ => 1,
                    };
                let mut last = -1i32;
                let exp = x.number_expanded.max(0) as usize;
                let times_stored = &l.values1[..number_stored as usize];
                let cn = x
                    .cnotes
                    .split(|&b| b == 0)
                    .map(|s| s.to_vec())
                    .collect::<Vec<_>>();
                for (i, &ts) in times_stored.iter().enumerate().take(exp) {
                    let t = i64::from(ts);
                    if !in_range(t, &mut last, block_gran, *bj, w) {
                        if last > 0 {
                            break;
                        }
                        continue;
                    }
                    let raw = if w.block_size == iv::BLOCK_1_CENTURY {
                        t * iv::SECS_IN_1_MINUTE / g
                    } else {
                        t / g
                    };
                    let tt = raw + base_to_start;
                    out.times.push(out.julian_base * iv::SECS_IN_1_DAY + tt * g);
                    out.values
                        .extend_from_slice(&x.values[i * stride..(i + 1) * stride]);
                    if qes > 0 {
                        out.quality
                            .extend_from_slice(&x.quality[i * qes..(i + 1) * qes]);
                    }
                    if nes > 0 {
                        out.inotes
                            .extend_from_slice(&x.inotes[i * nes..(i + 1) * nes]);
                    }
                    if let Some(c) = out.cnotes.as_mut() {
                        c.extend_from_slice(cn.get(i).map_or(&[][..], |v| v.as_slice()));
                        c.push(0);
                    }
                    current += 1;
                }
                if out.first.is_none() {
                    out.first = Some(l);
                }
            }
            let _ = gran_set;
        }
        Ok(out)
    }

    /// `ztsRetrievePattern()`: values1 holds the values as stored, no internal-header
    /// aggregation. HEC's C API returns no times for patterns (`ztsRegAddTimes()` runs
    /// after the time window is freed, so `hecdss` reports 0); `ztsRetrievePattern()` sets
    /// the start to one interval after 01Jan3000 00:00. Times here follow the time array
    /// HEC Monolith returns: the first value at 01Jan3000 00:00, then one per interval
    /// (verified on 4,586 pattern records, P08a report).
    fn read_pattern(
        &mut self,
        rec: RecordRef,
        interval: &StandardInterval,
    ) -> Result<Assembled, TsError> {
        let l = self.load(rec.clone())?;
        let n = rec.info.number_data;
        let elem = match rec.record_type {
            RTD_PATTERN | ITD_PATTERN => 2usize,
            _ => 1usize,
        };
        if n < 0 {
            return Err(Self::corrupt(&rec, "negative value count"));
        }
        let n = n as usize;
        let irregular = matches!(rec.record_type, ITS_PATTERN | ITD_PATTERN);
        let (values, times_raw): (Vec<i32>, Vec<i32>) = if irregular {
            let need = n + n * elem;
            if l.values1.len() < need {
                return Err(Self::corrupt(
                    &rec,
                    "pattern values shorter than the value count",
                ));
            }
            (l.values1[n..n + n * elem].to_vec(), l.values1[..n].to_vec())
        } else {
            if l.values1.len() < n * elem {
                return Err(Self::corrupt(
                    &rec,
                    "pattern values shorter than the value count",
                ));
            }
            (l.values1[..n * elem].to_vec(), Vec::new())
        };
        let ih = TsInternalHeader::decode(&l.ih_words);
        let granularity = if ih.time_granularity > 0 {
            i64::from(ih.time_granularity)
        } else {
            iv::MINUTE_GRANULARITY
        };
        let mut times = Vec::with_capacity(n);
        let j3000 = iv::ymd_to_julian(3000, 1, 1);
        if interval.code > 0 {
            // P25: the stored INT_HEAD_timeOffset is not applied (as for DSS6); it is
            // reported raw by `Dss7Ts::pattern_origin`.
            let (j, s) = iv::increment_time(interval.code, 0, j3000, 0);
            for i in 0..n as i64 {
                let (jj, ss) = iv::increment_time(interval.code, i, j, s);
                times.push(jj * iv::SECS_IN_1_DAY + ss);
            }
        } else {
            // P32: irregular patterns (111/116) store their times in granularity units
            // from julian base 0 (`Public/ztsRetrievePattern.c`, hec-dss `7-IU-15`: line 330
            // copies values1 into `tss->times`, line 365 sets `julianBaseDate = 0`;
            // `zStructTimeSeries.h` line 33: times count `timeGranularitySeconds`). HEC's
            // C library and Monolith 3.3.27 both give 1800 s for a stored 30 at minute
            // granularity; these times are seconds like every other time array.
            times.extend(times_raw.iter().map(|&t| i64::from(t) * granularity));
        }
        Ok(Assembled {
            pathname: rec.pathname.clone(),
            record_type: rec.record_type,
            times,
            values,
            elem,
            depths: 0,
            quality: Vec::new(),
            qes: 0,
            inotes: Vec::new(),
            nes: 0,
            cnotes: None,
            first: Some(l),
            profile_depths_words: Vec::new(),
            granularity,
            julian_base: 0,
            blocks_in_window: 1,
            block_pathnames: vec![rec.pathname.clone()],
        })
    }

    fn finish(&mut self, a: Assembled, profile: bool, _t: i32) -> Result<Dss7Ts, TsError> {
        let first = a.first.as_ref();
        let ih = first
            .map(|l| TsInternalHeader::decode(&l.ih_words))
            .unwrap_or_default();
        let parts = path_parts(&a.pathname);
        let interval =
            parts
                .and_then(|p| iv::standard_interval(p[4]))
                .unwrap_or(StandardInterval {
                    code: 0,
                    name: "",
                    pseudo_regular: false,
                });
        let pattern = matches!(
            a.record_type,
            RTS_PATTERN | RTD_PATTERN | ITS_PATTERN | ITD_PATTERN
        );
        let n = a.times.len();
        let stride = a.elem * a.depths.max(1);
        let mut vwords = a.values;
        vwords.truncate(n * stride);
        let values = words_to_values(&vwords, a.elem);
        let quality = if a.qes > 0 {
            Some(
                (0..n)
                    .map(|i| a.quality.get(i * a.qes).copied().unwrap_or(0))
                    .collect(),
            )
        } else {
            None
        };
        let kind = if interval.code > 0 {
            TsKind::Regular {
                interval_seconds: interval.code,
            }
        } else {
            TsKind::Irregular {
                block: match -interval.code {
                    iv::BLOCK_1_DAY => IrregularBlock::Day,
                    iv::BLOCK_1_MONTH => IrregularBlock::Month,
                    iv::BLOCK_1_YEAR => IrregularBlock::Year,
                    iv::BLOCK_1_DECADE => IrregularBlock::Decade,
                    _ => IrregularBlock::Century,
                },
            }
        };
        let _ = profile;
        let meta = TsMeta {
            interval,
            time_granularity_seconds: a.granularity,
            julian_base_date: a.julian_base,
            precision: ih.precision,
            // Applied offset: 0 for patterns (P25; the stored one is in pattern_origin).
            time_offset_seconds: if pattern {
                0
            } else {
                i64::from(ih.time_offset)
            },
            quality_element_size: a.qes,
            quality_words: if a.qes > 1 {
                Some(a.quality.clone())
            } else {
                None
            },
            inotes: if a.nes > 0 {
                Some(a.inotes.clone())
            } else {
                None
            },
            inotes_element_size: a.nes,
            cnotes: a.cnotes.as_ref().map(|c| cnotes_split(c, n)),
            user_header: first.map(|l| l.user_header.clone()).unwrap_or_default(),
            internal_header: ih.clone(),
            pattern,
            blocks_in_window: a.blocks_in_window,
            blocks_found: a.block_pathnames.len(),
            block_pathnames: a.block_pathnames,
        };
        let _ = first.map(|l| &l.rec);
        Ok(Dss7Ts {
            ts: TimeSeries {
                pathname: a.pathname,
                record_type: a.record_type,
                kind,
                times: a.times,
                values,
                quality,
                units: ih.units.clone().unwrap_or_default(),
                data_type: ih.data_type.clone().unwrap_or_default(),
                time_zone: ih.time_zone.clone(),
            },
            meta,
        })
    }
}

fn standard_interval_or_err(path: &[u8], e: &[u8]) -> Result<StandardInterval, TsError> {
    // ztsProcessTimes(): an E part shorter than 4 characters is not a time series.
    if e.len() < 4 {
        return Err(TsError::BadPathname {
            pathname: path.to_vec(),
            reason: "E part is not a DSS7 interval",
        });
    }
    iv::standard_interval(e).ok_or(TsError::BadPathname {
        pathname: path.to_vec(),
        reason: "E part is not a DSS7 interval",
    })
}

fn canonical_e(i: &StandardInterval) -> String {
    if i.pseudo_regular {
        format!("~{}", i.name)
    } else {
        i.name.to_string()
    }
}

fn append_block(out: &mut Assembled, x: compress::Expanded, n: usize, stride: usize) {
    let mut v = x.values;
    v.resize(n * stride, 0);
    out.values.extend_from_slice(&v);
    if out.qes > 0 {
        let mut q = x.quality;
        q.resize(n * out.qes, 0);
        out.quality.extend_from_slice(&q);
    }
    if out.nes > 0 {
        let mut q = x.inotes;
        q.resize(n * out.nes, 0);
        out.inotes.extend_from_slice(&q);
    }
    if let Some(c) = out.cnotes.as_mut() {
        let mut lines: Vec<&[u8]> = x.cnotes.split(|&b| b == 0).collect();
        lines.truncate(n);
        for k in 0..n {
            c.extend_from_slice(lines.get(k).copied().unwrap_or(&[]));
            c.push(0);
        }
    }
}

fn missing_block(out: &mut Assembled, n: usize, stride: usize, elem: usize) {
    let per = stride / elem.max(1);
    for _ in 0..n * per {
        if elem == 2 {
            out.values.push(compress::MISSING_F64_BITS as u32 as i32);
            out.values
                .push((compress::MISSING_F64_BITS >> 32) as u32 as i32);
        } else {
            out.values.push(compress::MISSING_F32_BITS as i32);
        }
    }
    out.quality.resize(out.quality.len() + n * out.qes, 0);
    out.inotes.resize(out.inotes.len() + n * out.nes, 0);
    if let Some(c) = out.cnotes.as_mut() {
        c.resize(c.len() + n, 0);
    }
}

/// `ztsRegAddTimes()`: absolute seconds of each regular value, with HEC's integer
/// arithmetic in granularity units (`times[i] = (julian - base) * (86400 / g) + secs / g`).
fn reg_times(interval: i64, w: &Window, offset: i64, gran: i64, base: i64, n: i64) -> Vec<i64> {
    let g = if gran < 1 {
        iv::MINUTE_GRANULARITY
    } else {
        gran
    };
    let (mut j0, mut s0) = (w.start_j, w.start_s);
    iv::offset_adjust_to_offset(offset, interval, &mut j0, &mut s0);
    let abs = |j: i64, s: i64| -> i64 {
        let t = (j - base) * (iv::SECS_IN_1_DAY / g) + s / g;
        base * iv::SECS_IN_1_DAY + t * g
    };
    let n = n.max(0) as usize;
    let mut out = Vec::with_capacity(n);
    if interval <= iv::SECS_IN_1_DAY {
        let (mut je, mut se) = iv::increment_time(interval, 0, j0, s0);
        if offset != 0 {
            iv::offset_adjust_to_offset(offset, interval, &mut je, &mut se);
        }
        let t0 = (je - base) * (iv::SECS_IN_1_DAY / g) + se / g;
        let step = interval / g;
        for i in 0..n as i64 {
            out.push(base * iv::SECS_IN_1_DAY + (t0 + i * step) * g);
        }
    } else {
        for i in 0..n as i64 {
            let (mut je, mut se) = iv::increment_time(interval, i, j0, s0);
            if offset != 0 {
                iv::offset_adjust_to_offset(offset, interval, &mut je, &mut se);
            }
            out.push(abs(je, se));
        }
    }
    out
}

/// `ztsTrim()`: drop leading and trailing missing values (HEC's missing test; for
/// profiles a row is missing when every element is).
fn trim(a: &mut Assembled) {
    let n = a.times.len();
    let stride = a.elem * a.depths.max(1);
    let per = a.depths.max(1);
    let row_missing = |i: usize| -> bool {
        (0..per).all(|k| {
            let at = i * stride + k * a.elem;
            let v = if a.elem == 2 {
                f64::from_bits(
                    (a.values[at] as u32 as u64) | ((a.values[at + 1] as u32 as u64) << 32),
                )
            } else {
                f64::from(f32::from_bits(a.values[at] as u32))
            };
            hec_missing(v)
        })
    };
    let first = (0..n).find(|&i| !row_missing(i));
    let Some(first) = first else {
        a.times.clear();
        a.values.clear();
        a.quality.clear();
        a.inotes.clear();
        if let Some(c) = a.cnotes.as_mut() {
            c.clear();
        }
        return;
    };
    let last = (0..n).rev().find(|&i| !row_missing(i)).unwrap_or(first);
    let keep = first..last + 1;
    a.times = a.times[keep.clone()].to_vec();
    a.values = a.values[keep.start * stride..keep.end * stride].to_vec();
    if a.qes > 0 {
        a.quality = a.quality[keep.start * a.qes..keep.end * a.qes].to_vec();
    }
    if a.nes > 0 {
        a.inotes = a.inotes[keep.start * a.nes..keep.end * a.nes].to_vec();
    }
    if let Some(c) = a.cnotes.as_mut() {
        let lines: Vec<&[u8]> = c.split(|&b| b == 0).collect();
        let mut v = Vec::new();
        for l in lines.iter().take(keep.end).skip(keep.start) {
            v.extend_from_slice(l);
            v.push(0);
        }
        *c = v;
    }
}

// ---------------------------------------------------------------------------------
// P14: profile dataset reads (types 102/107), additive.
// ---------------------------------------------------------------------------------

impl<R: Read + Seek> Dss7File<R> {
    /// Read a regular-interval profile dataset (types 102/107): `pathname` with any
    /// (usually blank) D part, stitched from every block in the window, as
    /// `ztsRetrieve()` → `ztsRetrieveReg7()` does for a `zStructTimeSeries` with
    /// `profileDepthsNumber > 0` (`Public/ztsRetrieve.c`, `Internal/ztsRetrieveReg7.c`).
    ///
    /// Window and trim semantics are those of [`Dss7File::read_ts`]: `window: None` is
    /// HEC's "retrieve all times" (bin dates of the first and last block), explicit
    /// windows are moved forward to the end of their standard interval, missing blocks
    /// read as -FLT_MAX rows, and `trim_missing` applies `ztsTrim()` (a profile row is
    /// missing when every depth is). Depths come from the first block found (values2,
    /// `ztsRetrieveReg7()` copies them once).
    ///
    /// Quality, when stored, is in `meta.quality_words` (`n × quality_element_size`,
    /// one entry per time) for every element size.
    ///
    /// Non-profile records, and irregular profiles (112/117, which have no corpus
    /// evidence), return [`TsError::UnsupportedRecordType`].
    pub fn read_ts_profile(
        &mut self,
        pathname: &[u8],
        opts: &TsReadOptions,
    ) -> Result<Dss7Profile, TsError> {
        if opts.window.is_none() {
            let cat = self.catalog()?;
            return self.read_ts_profile_with_catalog(&cat, pathname, opts);
        }
        self.read_ts_profile_impl(None, pathname, opts)
    }

    /// [`Dss7File::read_ts_profile`] with a prebuilt catalog for the whole-dataset
    /// range.
    pub fn read_ts_profile_with_catalog(
        &mut self,
        cat: &Dss7Catalog,
        pathname: &[u8],
        opts: &TsReadOptions,
    ) -> Result<Dss7Profile, TsError> {
        self.read_ts_profile_impl(Some(cat), pathname, opts)
    }

    fn read_ts_profile_impl(
        &mut self,
        cat: Option<&Dss7Catalog>,
        pathname: &[u8],
        opts: &TsReadOptions,
    ) -> Result<Dss7Profile, TsError> {
        let parts = path_parts(pathname).ok_or(TsError::BadPathname {
            pathname: pathname.to_vec(),
            reason: "not /A/B/C/D/E/F/",
        })?;
        let interval = standard_interval_or_err(pathname, parts[4])?;
        let ename = canonical_e(&interval);
        let mut p = parts;
        p[4] = ename.as_bytes();
        let canon = join_path(p);
        // Pattern records (`isTsPattern()`: D part starting with "TS-P") are never
        // profiles: refuse with the pattern record's own type.
        if parts[3].len() > 4 && parts[3][..4].eq_ignore_ascii_case(b"TS-P") {
            let rec = self.lookup(&canon)?.ok_or_else(|| TsError::NotFound {
                pathname: pathname.to_vec(),
            })?;
            return Err(TsError::UnsupportedRecordType {
                pathname: rec.pathname,
                record_type: rec.record_type,
            });
        }
        // ztsProcessTimes(): the same window resolution as read_ts_impl().
        let w = match opts.window {
            Some(win) => {
                if win.end < win.start {
                    return Err(TsError::BadWindow);
                }
                let s = (
                    win.start.div_euclid(time::DAY),
                    win.start.rem_euclid(time::DAY),
                );
                let e = (win.end.div_euclid(time::DAY), win.end.rem_euclid(time::DAY));
                resolve_window(&interval, WindowSource::Explicit(s, e))?
            }
            None => {
                let cat = cat.ok_or(TsError::BadWindow)?;
                let (fj, lj) = self
                    .dataset_date_range(cat, &canon, &interval)?
                    .ok_or_else(|| TsError::NotFound {
                        pathname: pathname.to_vec(),
                    })?;
                // ztsProcessTimes() boolRetrieveAllTimes branch.
                let ss = if interval.code >= iv::SECS_IN_1_MINUTE {
                    iv::SECS_IN_1_MINUTE
                } else {
                    1
                };
                let (mut sj, mut ss, mut ej, mut es) = (fj, ss, lj, iv::SECS_IN_1_DAY);
                if interval.code > 0 {
                    iv::offset_adjust_to_standard(interval.code, &mut sj, &mut ss);
                    iv::offset_adjust_to_standard(interval.code, &mut ej, &mut es);
                }
                resolve_window(&interval, WindowSource::Explicit((sj, ss), (ej, es)))?
            }
        };
        self.check_profile_blocks(&canon, &w)?;
        let mut a = self.read_blocks(&canon, &interval, &w, None)?;
        if a.depths == 0 || !matches!(a.record_type, RTS_PROFILE | RTD_PROFILE) {
            return Err(TsError::UnsupportedRecordType {
                pathname: a.pathname.clone(),
                record_type: a.record_type,
            });
        }
        if opts.trim_missing {
            trim(&mut a);
        }
        let depths_n = a.depths;
        let elem = a.elem;
        let depth_words = a.profile_depths_words.clone();
        if depth_words.len() < depths_n * elem {
            return Err(TsError::Corrupt {
                pathname: a.pathname.clone(),
                info_address: a.first.as_ref().map(|l| l.rec.info_address).unwrap_or(0),
                what: "profile depths shorter than the depth count",
            });
        }
        let t = a.record_type;
        let mut r = self.finish(a, true, t)?;
        profile_quality_to_meta(&mut r);
        let ih = &r.meta.internal_header;
        let depths = words_to_values(&depth_words[..depths_n * elem], elem);
        Ok(Dss7Profile {
            pathname: r.ts.pathname.clone(),
            record_type: r.ts.record_type,
            times: r.ts.times.clone(),
            depths,
            values: r.ts.values.clone(),
            depth_units: ih.depth_units.clone().unwrap_or_default(),
            value_units: ih.units.clone().unwrap_or_default(),
            data_type: ih.data_type.clone().unwrap_or_default(),
            time_zone: ih.time_zone.clone(),
            meta: r.meta,
        })
    }
}

impl<R: Read + Seek> Dss7File<R> {
    /// Every block a profile window reads must have the same depth count and depth
    /// values as the first block found.
    ///
    /// HEC does not guarantee this: `ztsGetSizesInternal()` keeps the depth count of the
    /// LAST block (`recordSize->tsProfileDepthsNumber`), and `ztsRetrieveReg7()` only
    /// fails with `DIFFERENT_PROFILE_NUMBER` (warning severity) when the first block
    /// found has fewer depths than that; depth values are never compared. Stitching
    /// blocks with different depths would misalign rows (or pad them with 0.0, a valid
    /// value), so this reader refuses such datasets with [`TsError::Corrupt`].
    fn check_profile_blocks(&mut self, canon: &[u8], w: &Window) -> Result<(), TsError> {
        let parts = path_parts(canon).ok_or(TsError::BadWindow)?;
        // (depth count, depths widened to f64 bits) of the first profile block.
        let mut first: Option<(usize, Vec<u64>)> = None;
        let mut j = w.start_block;
        let mut n_blocks = 0usize;
        while j <= w.end_block {
            n_blocks += 1;
            if n_blocks > 2_000_000 {
                return Err(TsError::TooManyValues {
                    values: n_blocks as i64,
                });
            }
            let d = iv::julian_to_d_part(j);
            let mut p = parts;
            p[3] = d.as_bytes();
            let path = join_path(p);
            if let Some(r) = self.lookup_block(&path)? {
                if matches!(r.record_type, RTS_PROFILE | RTD_PROFILE) {
                    let ih = self.read_area(&r, r.info.internal_header, INT_HEAD_SIZE)?;
                    let g = |i: usize| ih.get(i).copied().unwrap_or(0).max(0) as usize;
                    let n = g(compress::IH_PROFILE_DEPTHS);
                    let elem = if g(compress::IH_VALUE_ELEMENT_SIZE) == 2 {
                        2
                    } else {
                        1
                    };
                    let v2 = self.read_area(&r, r.info.values2, usize::MAX)?;
                    let need = n.saturating_mul(elem);
                    if v2.len() < need {
                        return Err(Self::corrupt(
                            &r,
                            "profile depths shorter than the depth count",
                        ));
                    }
                    let bits: Vec<u64> = match words_to_values(&v2[..need], elem) {
                        TsValues::F32(v) => v.iter().map(|x| f64::from(*x).to_bits()).collect(),
                        TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
                    };
                    match &first {
                        None => first = Some((n, bits)),
                        Some((n0, b0)) => {
                            if *n0 != n {
                                return Err(Self::corrupt(
                                    &r,
                                    "profile depth count differs between blocks",
                                ));
                            }
                            if *b0 != bits {
                                return Err(Self::corrupt(
                                    &r,
                                    "profile depth values differ between blocks",
                                ));
                            }
                        }
                    }
                }
            }
            let next = iv::increment_block(j, w.block_size);
            if next <= j {
                break;
            }
            j = next;
        }
        Ok(())
    }
}

/// `Dss7Profile` has no quality field: a single-word quality array (one word per time,
/// `ztsRetrieveReg7()` `numberToRead * qualityElementSize`) goes in `meta.quality_words`
/// too, so profile quality is always found there.
fn profile_quality_to_meta(r: &mut Dss7Ts) {
    if r.meta.quality_words.is_none() && r.meta.quality_element_size == 1 {
        r.meta.quality_words = r.ts.quality.take();
    }
}

/// HEC seconds of the first value of a regular pattern as heclib C reports it
/// (`Public/ztsRetrievePattern.c` lines 360-372): start at 01Jan3000 00:00, step one
/// interval (`incrementTime(+1)`), then, when `offset_seconds` is nonzero, move to that
/// offset within the interval (`Public/ztsOffsetAdjustToOffset.c` lines 93-120). Used for
/// both versions (P25). `None` for a non-positive interval.
pub fn pattern_heclib_first_seconds(interval_seconds: i64, offset_seconds: i64) -> Option<i64> {
    if interval_seconds < 1 {
        return None;
    }
    let j3000 = iv::ymd_to_julian(3000, 1, 1);
    let (mut j, mut s) = iv::increment_time(interval_seconds, 1, j3000, 0);
    if offset_seconds != 0 {
        iv::offset_adjust_to_offset(offset_seconds, interval_seconds, &mut j, &mut s);
    }
    Some(j.saturating_mul(iv::SECS_IN_1_DAY).saturating_add(s))
}

impl Dss7Ts {
    /// Time-origin facts of a pattern record (P25); `None` for other records. The stored
    /// offset is `INT_HEAD_timeOffset` (`headers/zStructAllocation.h`; word 2), unpacked
    /// as `Internal/ztsInternalHeaderUnpack.c` does, and is never applied to `ts.times`.
    pub fn pattern_origin(&self) -> Option<crate::timeseries::PatternOrigin> {
        if !self.meta.pattern {
            return None;
        }
        let ih = &self.meta.internal_header;
        let stored = (ih.words.len() > 2).then(|| i64::from(ih.time_offset));
        let code = self.meta.interval.code;
        let irregular = matches!(self.ts.record_type, ITS_PATTERN | ITD_PATTERN);
        Some(crate::timeseries::PatternOrigin {
            stored_offset_seconds: stored,
            stored_offset_source: crate::timeseries::PatternOffsetSource::Dss7IntHeadTimeOffset,
            heclib_first_seconds: if irregular {
                None
            } else {
                pattern_heclib_first_seconds(code, stored.unwrap_or(0))
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &[u8]) -> Vec<i32> {
        let mut b = s.to_vec();
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.chunks(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn internal_header_strings_padded_and_old() {
        let mut ih = vec![60, -1, 0, 0, 0, 23, 24, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        ih.push(i32::from_le_bytes(*b"    "));
        ih.extend(words(b"CFS\0INST-VAL\0UTC\0"));
        let h = TsInternalHeader::decode(&ih);
        assert!(h.units_padded);
        assert_eq!(h.units.as_deref(), Some("CFS"));
        assert_eq!(h.data_type.as_deref(), Some("INST-VAL"));
        assert_eq!(h.time_zone.as_deref(), Some("UTC"));
        // 7-10: strings start at int 17.
        let mut ih = vec![60, -1, 0, 0, 0, 23, 24, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        ih.extend(words(b"CFS\0INST-VAL\0"));
        let h = TsInternalHeader::decode(&ih);
        assert!(!h.units_padded);
        assert_eq!(h.units.as_deref(), Some("CFS"));
        assert_eq!(h.data_type.as_deref(), Some("INST-VAL"));
        assert_eq!(h.time_zone, None);
        // Empty units hide the type, as in HEC.
        let mut ih = vec![0; 17];
        ih.push(0);
        ih.extend(words(b"\0PER-AVER\0"));
        let h = TsInternalHeader::decode(&ih);
        assert_eq!(h.units, None);
        assert_eq!(h.data_type, None);
    }

    #[test]
    fn parts() {
        let p = path_parts(b"/A/B/C/01Jan2020/1Hour/F/").unwrap();
        assert_eq!(p[3], b"01Jan2020");
        assert!(path_parts(b"/A/B/C/D/E/").is_none());
        assert!(path_parts(b"A/B/C/D/E/F/").is_none());
    }

    #[test]
    fn single_block_windows() {
        let hour = iv::standard_interval(b"1Hour").unwrap();
        let j = iv::ymd_to_julian(2018, 9, 1);
        let w = resolve_window(&hour, WindowSource::PathD(j)).unwrap();
        assert_eq!(w.number_values, 30 * 24);
        assert_eq!((w.start_j, w.start_s), (j, 3600));
        assert_eq!(w.start_block, w.end_block);
        let day = iv::standard_interval(b"1Day").unwrap();
        let w = resolve_window(&day, WindowSource::PathD(iv::ymd_to_julian(1877, 1, 1))).unwrap();
        assert_eq!(w.number_values, 365);
        let mon = iv::standard_interval(b"1Month").unwrap();
        let w = resolve_window(&mon, WindowSource::PathD(iv::ymd_to_julian(2010, 1, 1))).unwrap();
        assert_eq!(w.number_values, 120);
        assert_eq!(
            (w.start_j, w.start_s),
            (iv::ymd_to_julian(2010, 1, 31), 86_400)
        );
    }
}

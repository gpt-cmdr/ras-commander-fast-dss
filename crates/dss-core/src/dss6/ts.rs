//! DSS6 time-series record read (packet P08b): one block into [`TimeSeries`].
//!
//! Format rules (dss6.md §5, §7, §8) and the `heclib_f` routines they come from:
//! - Regular TS (100/105): `zrrtsi.f:ZRRTSI6` (no-time-window path, lines 189-206 and
//!   328-380; header words lines 576-603) and `zrrtsb6.f:ZRRTSB6` (data layouts,
//!   lines 71-221). Value `k` is at block start 00:00 + `k` intervals (`INCTIM`); a
//!   nonzero offset (internal header word 1) moves each stamp to `standard - interval +
//!   offset` (`zofset6.f` lines 142-154).
//! - Pattern TS (D part `TS-...`, types 101/111): `zrrtsi.f` lines 161-187 and
//!   `zrrtpa6.f:ZRRTPA6` (regular: `KINDAT` raw `REAL*4` values) and `zritsi6.f` lines
//!   150-191 (irregular: `IIHEAD(2)` time/value pairs).
//! - Irregular TS (110/115): `zritsi6.f:ZRITSI6` (no-window path lines 194-277; header
//!   words lines 726-751), `zircpy6.f:ZIRCPY6` (group layout).
//! - Compression: [`super::compress`].
//! - Access: `zrdinf6.f:ZRDINF6` lines 64-73 (password word set and file version third
//!   character `>= 'E'` means no access).
//!
//! Values keep their stored bits; HEC's missing markers are not converted.

use super::compress::{self, CompressError, CompressionHeader};
use super::info::unpack_chars;
use super::interval::{self, BlockSpan, EPart, DAY_MINUTES};
use super::lookup::Dss6Record;
use super::pathname::split_parts;
use super::source::byte_offset_word;
use super::{Dss6Error, Dss6File, Dss6Info, ReadAt};
use crate::catalog::CatalogEntry;
use crate::{TimeSeries, TsKind, TsValues};
use std::fmt;

/// Internal header words HEC reads for regular TS (`zrrtsi.f` line 80, `KIHEAD = 24`).
pub const RTS_HEADER_WORDS: usize = 24;
/// Internal header words HEC reads for irregular TS (`zritsi6.f` line 57, `KIHEAD = 28`).
pub const ITS_HEADER_WORDS: usize = 28;
/// Compression header words HEC reads (`headers/zdssdc.h`, `KDCH = 300`).
pub const MAX_COMPRESSION_HEADER_WORDS: usize = 300;
/// Sanity cap on header lengths before any read (no HEC writer exceeds a few hundred).
const MAX_HEADER_WORDS: i32 = 100_000;

/// Why a time-series record could not be read.
#[derive(Debug)]
pub enum TsError {
    /// File-level or structural failure from the DSS6 reader.
    Dss6(Dss6Error),
    /// No live record with this pathname.
    NotFound { pathname: Vec<u8> },
    /// The record is not a time series this reader decodes (paired data, grids, text,
    /// or an unknown code). Lists in catalogs only (OPEN_QUESTIONS D29).
    UnsupportedRecordType { record_type: i32 },
    /// Compression scheme not decoded (4/5 sig-digits with `allow_sig_digits = false`).
    UnsupportedCompression { scheme: i32 },
    /// Record password set; HEC denies access for files `6-E` and later.
    PasswordProtected,
    /// The pathname cannot address a time series (parts, D part date, E part interval).
    BadPathname { pathname: Vec<u8>, detail: String },
    /// Header, length or compression data inconsistent.
    Corrupt {
        address: Option<i64>,
        detail: String,
    },
    /// A dataset read would produce more than `limit` values or blocks (an oversized
    /// window, or block dates far apart in a damaged file).
    TooLarge { count: u64, limit: u64 },
    /// P29, only with [`NoDataPolicy::Refuse`]: HEC DSS6 "no data". The record (or every
    /// block of the dataset window) exists, but no value read is other than -901/-902,
    /// so HEC's `zrrtsi.f:ZRRTSI6` sets `ISTAT = 4` (line 651; see [`NO_DATA_STATUS`]).
    /// `pathname` is the record, or for a dataset read the requested dataset pathname.
    NoData { pathname: Vec<u8>, status: i32 },
}

impl fmt::Display for TsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TsError::Dss6(e) => write!(f, "{e}"),
            TsError::NotFound { pathname } => {
                write!(f, "record not found: {}", lossy(pathname))
            }
            TsError::UnsupportedRecordType { record_type } => {
                write!(
                    f,
                    "record type {record_type} is not a supported DSS6 time series"
                )
            }
            TsError::UnsupportedCompression { scheme } => {
                write!(f, "DSS6 compression scheme {scheme} is not supported")
            }
            TsError::PasswordProtected => write!(f, "record is password protected"),
            TsError::BadPathname { pathname, detail } => {
                write!(
                    f,
                    "invalid time-series pathname {}: {detail}",
                    lossy(pathname)
                )
            }
            TsError::TooLarge { count, limit } => {
                write!(
                    f,
                    "dataset read needs {count} values or blocks (limit {limit})"
                )
            }
            TsError::NoData { pathname, status } => {
                write!(
                    f,
                    "no data: every value read for {} is missing (-901/-902; HEC DSS6 status {status})",
                    lossy(pathname)
                )
            }
            TsError::Corrupt { address, detail } => match address {
                Some(a) => write!(f, "corrupt time-series record at word {a}: {detail}"),
                None => write!(f, "corrupt time-series record: {detail}"),
            },
        }
    }
}

impl std::error::Error for TsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TsError::Dss6(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Dss6Error> for TsError {
    fn from(e: Dss6Error) -> Self {
        TsError::Dss6(e)
    }
}

pub(crate) fn lossy(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// Read options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsReadOptions {
    /// Decode compression schemes 4/5 (3-significant-digit packing, `dusd3r.f`/`sd32r.f`).
    /// Default `true` since P33: they are absent from the corpus, but every value of the
    /// HEC-written scheme 4/5 fixture `synthetic_hec_compress6.dss` (Monolith 3.3.27,
    /// `Heclib.zsrtsx` with `JCOMP` 4/5) is bit-equal to heclib's `zrrtsx` decode.
    /// `false` refuses them with [`TsError::UnsupportedCompression`], as before P33.
    pub allow_sig_digits: bool,
    /// P29: what a regular read does when HEC would report "no data" (dss6.md §7.4).
    /// Default [`NoDataPolicy::Return`]: the values are returned.
    pub no_data: NoDataPolicy,
}

/// HEC's `ISTAT` for "no data found, although a pathname was read"
/// (`heclib_f/src/zrrtsi.f:ZRRTSI6` line 56, hec-dss fork tag `7-IU-15`).
pub const NO_DATA_STATUS: i32 = 4;

/// P29: whether a DSS6 regular read refuses data HEC reports as "no data" (dss6.md §7.4).
///
/// HEC DSS6 (`zrrtsi.f:ZRRTSI6`, hec-dss fork tag `7-IU-15`) reads every block of the
/// request, then (lines 618-649) sets `LFOUND` only for a value that is neither -901 nor
/// -902 (floats line 631, doubles line 645); line 651 `IF (.NOT.LFOUND) ISTAT = 4` when
/// a block was read but no such value exists. `Dss6/ztsRetrieveReg6.c` lines 255-259
/// turn status 4 into `STATUS_RECORD_NOT_FOUND` (-1), and Monolith 3.3.27
/// `HecTimeSeries.read` (bytecode 327-334) returns -2 with no values.
///
/// The rule is for the regular path only: record types 100 and 105 (`zrrtsi.f` line 290
/// accepts only these), not patterns (the `TS-` D part branch, lines 165-185, returns
/// before line 618), not irregular data (`zritsi6.f` has no such status), not DSS7
/// (`ztsRetrieveReg7.c` treats the block as found). -FLT_MAX and NaN are values for the
/// rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoDataPolicy {
    /// Return the values (the engine's behavior before P29).
    #[default]
    Return,
    /// Raise [`TsError::NoData`] as HEC DSS6 reports "no data".
    Refuse,
}

impl Default for TsReadOptions {
    fn default() -> Self {
        Self {
            allow_sig_digits: true,
            no_data: NoDataPolicy::default(),
        }
    }
}

impl TsReadOptions {
    /// The same options with `no_data` replaced.
    pub fn with_no_data(self, no_data: NoDataPolicy) -> Self {
        Self { no_data, ..self }
    }
}

/// P29: no value is other than -901 or -902 (`zrrtsi.f` lines 618-651: `EQ.-901.0`,
/// `EQ.-902.0`; any other value, -FLT_MAX and NaN included, sets `LFOUND`). True for
/// an empty array, as HEC's loop over zero values leaves `LFOUND` false.
pub fn all_hec_no_data(values: &TsValues) -> bool {
    match values {
        TsValues::F32(v) => v.iter().all(|&x| x == -901.0 || x == -902.0),
        TsValues::F64(v) => v.iter().all(|&x| x == -901.0 || x == -902.0),
    }
}

/// How the values of a block are laid out (for reports and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsLayout {
    /// Regular, uncompressed.
    Regular,
    /// Regular, compressed with scheme `KICOMP`.
    Compressed(i32),
    /// Regular pattern (`TS-` D part).
    RegularPattern,
    /// Irregular.
    Irregular,
    /// Irregular pattern.
    IrregularPattern,
}

/// A decoded DSS6 time-series block with its DSS6-specific metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss6TsRecord {
    pub pathname: Vec<u8>,
    pub record_type: i32,
    pub layout: TsLayout,
    /// Regular interval in minutes (`zgintl6`); `None` for irregular data.
    pub interval_minutes: Option<i32>,
    /// Block span (regular: by convention from the interval; irregular: from the E part).
    pub block: Option<BlockSpan>,
    /// Julian day of the block start (regular: D part; irregular: internal header word 3).
    pub block_start_julian: Option<i64>,
    /// One stamp per value, seconds since 1899-12-31 00:00 (24:00 is the next day 00:00).
    pub times: Vec<i64>,
    pub values: TsValues,
    pub quality: Option<Vec<i32>>,
    pub units: String,
    pub data_type: String,
    /// Regular: offset in minutes from the standard boundary (internal header word 1).
    pub time_offset_minutes: i32,
    /// Time-zone offset word (RTS word 6, ITS word 10) when the header has it; -1 = unset.
    pub time_zone_offset: Option<i32>,
    /// Time-zone name (12 or 24 characters) when stored and not blank.
    pub time_zone_name: Option<String>,
    /// Coordinates and their descriptors (24-word RTS / 28-word ITS headers), when any
    /// coordinate is nonzero.
    pub coordinates: Option<[f64; 3]>,
    pub coordinate_descriptors: Option<[i32; 6]>,
    /// `KICOMP`, `KIPREC`, `KILNDA`, `KINDAT`.
    pub compression: i32,
    pub precision: i32,
    pub logical_values: i32,
    pub data_words: i32,
    /// Compression header fields, for compressed records.
    pub compression_header: Option<CompressionHeader>,
    /// Irregular: allocated capacity in values (word 1) and stored values (word 2).
    pub irregular_capacity: Option<i32>,
    /// Pseudo-regular interval in minutes (irregular word 5), when nonzero.
    pub pseudo_interval_minutes: Option<i32>,
    /// User header words.
    pub user_header: Vec<i32>,
    /// Regular: the block's value count differs from the count the interval's standard
    /// block holds (dss6.md §12 O21, e.g. 15-minute data in daily blocks).
    pub nonstandard_block: bool,
    /// Raw internal header words as read.
    pub internal_header: Vec<i32>,
}

impl Dss6TsRecord {
    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    /// Version-neutral view.
    pub fn to_timeseries(&self) -> TimeSeries {
        TimeSeries {
            pathname: self.pathname.clone(),
            record_type: self.record_type,
            kind: self.kind(),
            times: self.times.clone(),
            values: self.values.clone(),
            quality: self.quality.clone(),
            units: self.units.clone(),
            data_type: self.data_type.clone(),
            time_zone: self.time_zone_name.clone(),
        }
    }

    pub fn kind(&self) -> TsKind {
        match self.interval_minutes {
            Some(m) => TsKind::Regular {
                interval_seconds: i64::from(m) * 60,
            },
            None => TsKind::Irregular {
                block: self.block.unwrap_or(BlockSpan::Day).to_irregular(),
            },
        }
    }
}

/// Header text: `HOLCHR` of `n` characters starting at 0-based word `w` (`zrrtsi.f`
/// lines 577-580, `zritsi6.f` line 439), returned as heclib's C layer does: trailing
/// blanks removed, then cut at the first NUL (`ztsRetrieveReg6.c` lines 187, 217-220 and
/// `ztsRetrieveIrreg6.c` lines 153, 187-189 call `stringFortToC`; every consumer reads the
/// C string). Missing words read as blanks. Used for the data type and time-zone name;
/// units go through [`units_text`].
fn header_text(words: &[i32], w: usize, n: usize) -> String {
    let nw = n.div_ceil(4);
    let mut buf = vec![0x20202020i32; nw];
    for (i, b) in buf.iter_mut().enumerate() {
        if let Some(&x) = words.get(w + i) {
            *b = x;
        }
    }
    super::header::fort_to_c(&unpack_chars(&buf, n))
}

/// Units: [`header_text`], then characters at or below U+0020 trimmed at both ends (Java
/// `String.trim()`), as Monolith's
/// `HecTimeSeries.read` does after `ztsRetrieve` (`tsc.units = tsc.units.trim()`,
/// Monolith 3.3.27 offsets 341-349). P60 E1: a pydsstools-written units word `CFS`, NUL,
/// 0x1e, 0x02 reads `CFS`, as in Monolith and pydsstools (it read `CFS` plus the three
/// trailing bytes before P60).
fn units_text(words: &[i32], w: usize) -> String {
    // Java `String.trim()` removes every character at or below U+0020, not only blanks.
    header_text(words, w, 8)
        .trim_matches(|c: char| c <= ' ')
        .to_string()
}

fn word(words: &[i32], w: usize) -> Option<i32> {
    words.get(w).copied()
}

fn f64_from_words(lo: i32, hi: i32) -> f64 {
    f64::from_bits((u64::from(hi as u32) << 32) | u64::from(lo as u32))
}

/// Whether the D part marks pattern data (`zrrtsi.f` lines 165-167: more than 3
/// characters and starting `TS-`, case-insensitive).
pub fn is_pattern_d_part(d: &[u8]) -> bool {
    d.len() > 3 && d[..3].eq_ignore_ascii_case(b"TS-")
}

/// Time-zone, coordinate words of a TS internal header. `tz_word` is the 0-based word of
/// the offset; the name follows it (`zrrtsi.f` lines 582-602; `zritsi6.f` lines 731-750).
type ZoneCoords = (
    Option<i32>,
    Option<String>,
    Option<[f64; 3]>,
    Option<[i32; 6]>,
);

fn zone_and_coords(ih: &[i32], nihead: usize, tz_word: usize) -> ZoneCoords {
    let tz_offset = if nihead > tz_word {
        word(ih, tz_word)
    } else {
        None
    };
    // Name: 12 characters when the header is exactly tz_word + 4 words long (9 / 13),
    // 24 characters when it reaches tz_word + 7 words (12 / 16) or more.
    let name = if nihead == tz_word + 4 {
        Some(header_text(ih, tz_word + 1, 12))
    } else if nihead >= tz_word + 7 {
        Some(header_text(ih, tz_word + 1, 24))
    } else {
        None
    }
    .filter(|s| !s.trim().is_empty());
    let (mut coords, mut desc) = (None, None);
    if nihead >= tz_word + 19 && ih.len() >= tz_word + 19 {
        let c0 = tz_word + 7;
        let c = [
            f64_from_words(ih[c0], ih[c0 + 1]),
            f64_from_words(ih[c0 + 2], ih[c0 + 3]),
            f64_from_words(ih[c0 + 4], ih[c0 + 5]),
        ];
        if c.iter().any(|&x| x != 0.0) {
            coords = Some(c);
            let mut d = [0i32; 6];
            d.copy_from_slice(&ih[c0 + 6..c0 + 12]);
            desc = Some(d);
        }
    }
    (tz_offset, name, coords, desc)
}

impl<S: ReadAt> Dss6File<S> {
    /// Look up `pathname` and decode it as one time-series block.
    pub fn read_ts_record(&self, pathname: &[u8]) -> Result<Dss6TsRecord, TsError> {
        self.read_ts_record_with(pathname, &TsReadOptions::default())
    }

    /// [`Self::read_ts_record`] with options.
    pub fn read_ts_record_with(
        &self,
        pathname: &[u8],
        opts: &TsReadOptions,
    ) -> Result<Dss6TsRecord, TsError> {
        let rec = self.lookup(pathname)?.ok_or_else(|| TsError::NotFound {
            pathname: pathname.to_vec(),
        })?;
        self.decode_ts_info(&rec.info, opts)
    }

    /// Read one time-series block as the version-neutral [`TimeSeries`].
    pub fn read_ts(&self, pathname: &[u8]) -> Result<TimeSeries, TsError> {
        Ok(self.read_ts_record(pathname)?.to_timeseries())
    }

    /// Decode a record already found by [`Self::lookup`].
    pub fn decode_ts(&self, rec: &Dss6Record) -> Result<Dss6TsRecord, TsError> {
        self.decode_ts_info(&rec.info, &TsReadOptions::default())
    }

    /// Decode the record a catalog entry points to (its info block is re-read and
    /// checked).
    pub fn read_ts_entry(
        &self,
        entry: &CatalogEntry,
        opts: &TsReadOptions,
    ) -> Result<Dss6TsRecord, TsError> {
        let addr = byte_offset_word(entry.info_address).ok_or_else(|| TsError::Corrupt {
            address: None,
            detail: format!("catalog info offset {} is not a word", entry.info_address),
        })?;
        let npath = i32::try_from(entry.pathname.len()).unwrap_or(i32::MAX);
        let info = self.read_info(addr, npath)?;
        if !info.has_valid_flag() || info.pathname != entry.pathname {
            return Err(TsError::Corrupt {
                address: Some(addr),
                detail: "info block does not match catalog entry".into(),
            });
        }
        self.decode_ts_info(&info, opts)
    }

    /// Whether HEC would deny access to this record (`zrdinf6.f` lines 64-73).
    pub fn is_record_denied(&self, info: &Dss6Info) -> bool {
        let v = self.header().version_string.as_bytes();
        info.password[0] != 0 && v.get(2).is_some_and(|&c| c >= b'E')
    }

    /// Decode the time series an info block describes.
    pub fn decode_ts_info(
        &self,
        info: &Dss6Info,
        opts: &TsReadOptions,
    ) -> Result<Dss6TsRecord, TsError> {
        if self.is_record_denied(info) {
            return Err(TsError::PasswordProtected);
        }
        let rt = info.record_type;
        if !matches!(rt, 100 | 101 | 105 | 110 | 111 | 115) {
            return Err(TsError::UnsupportedRecordType { record_type: rt });
        }
        let path = info.pathname.clone();
        let bad = |detail: &str| TsError::BadPathname {
            pathname: path.clone(),
            detail: detail.into(),
        };
        let parts = split_parts(&path).ok_or_else(|| bad("not /A/B/C/D/E/F/"))?;
        let d = &path[parts[3].clone()];
        let e = &path[parts[4].clone()];
        let pattern = is_pattern_d_part(d);
        let regular = matches!(rt, 100 | 101 | 105);
        if regular {
            let rec = self.decode_regular(info, d, e, pattern, opts)?;
            // P29: HEC's "no data" rule for one block (`NoDataPolicy`).
            if opts.no_data == NoDataPolicy::Refuse
                && !pattern
                && matches!(rt, 100 | 105)
                && all_hec_no_data(&rec.values)
            {
                return Err(TsError::NoData {
                    pathname: rec.pathname,
                    status: NO_DATA_STATUS,
                });
            }
            Ok(rec)
        } else {
            self.decode_irregular(info, d, e, pattern)
        }
    }

    fn read_internal_header(&self, info: &Dss6Info, max: usize) -> Result<Vec<i32>, TsError> {
        let (addr, n) = info.internal_header;
        if !(0..=MAX_HEADER_WORDS).contains(&n) {
            return Err(TsError::Corrupt {
                address: Some(info.address),
                detail: format!("internal header length {n}"),
            });
        }
        let n = (n as usize).min(max);
        Ok(self.read_words(addr.into(), n)?)
    }

    fn read_user_header(&self, info: &Dss6Info) -> Result<Vec<i32>, TsError> {
        let (addr, n) = info.user_header;
        if !(0..=MAX_HEADER_WORDS).contains(&n) {
            return Err(TsError::Corrupt {
                address: Some(info.address),
                detail: format!("user header length {n}"),
            });
        }
        Ok(self.read_words(addr.into(), n as usize)?)
    }

    fn data_words_checked(&self, info: &Dss6Info, need: i64) -> Result<Vec<i32>, TsError> {
        // KINDAT is the stored length; never read past it (dss6.md §5.1).
        if need < 0 || need > i64::from(info.data_words) {
            return Err(TsError::Corrupt {
                address: Some(info.address),
                detail: format!(
                    "data needs {need} words, record stores {} (KINDAT)",
                    info.data_words
                ),
            });
        }
        Ok(self.read_words(info.data_address.into(), need as usize)?)
    }

    fn decode_regular(
        &self,
        info: &Dss6Info,
        d: &[u8],
        e: &[u8],
        pattern: bool,
        opts: &TsReadOptions,
    ) -> Result<Dss6TsRecord, TsError> {
        let rt = info.record_type;
        let bad = |detail: String| TsError::BadPathname {
            pathname: info.pathname.clone(),
            detail,
        };
        let (intl, _) = interval::regular_interval(e)
            .ok_or_else(|| bad(format!("E part '{}' is not a regular interval", lossy(e))))?;
        let ih = self.read_internal_header(info, RTS_HEADER_WORDS)?;
        let nihead = info.internal_header.1 as usize;
        let user_header = self.read_user_header(info)?;
        let units = units_text(&ih, 1);
        let data_type = header_text(&ih, 3, 8);
        let offset = word(&ih, 0).unwrap_or(0);
        let (tz_offset, tz_name, coords, desc) = zone_and_coords(&ih, nihead, 5);
        let mut rec = Dss6TsRecord {
            pathname: info.pathname.clone(),
            record_type: rt,
            layout: TsLayout::Regular,
            interval_minutes: Some(intl),
            block: Some(interval::regular_block_for(
                intl,
                self.header().version_number,
            )),
            block_start_julian: None,
            times: Vec::new(),
            values: TsValues::F32(Vec::new()),
            quality: None,
            units,
            data_type,
            time_offset_minutes: 0,
            time_zone_offset: tz_offset,
            time_zone_name: tz_name,
            coordinates: coords,
            coordinate_descriptors: desc,
            compression: info.compression,
            precision: info.precision,
            logical_values: info.logical_values,
            data_words: info.data_words,
            compression_header: None,
            irregular_capacity: None,
            pseudo_interval_minutes: None,
            user_header,
            nonstandard_block: false,
            internal_header: ih,
        };

        if pattern {
            // zrrtpa6.f: KINDAT raw REAL*4 values, no compression or quality; the
            // internal header still holds units/type (lines 42-45). No dates.
            rec.layout = TsLayout::RegularPattern;
            rec.block = None;
            let n = info.data_words;
            let words = self.data_words_checked(info, i64::from(n))?;
            rec.values = TsValues::F32(words.iter().map(|&w| f32::from_bits(w as u32)).collect());
            rec.times = (1..=i64::from(n.max(0)))
                .map(|k| k * i64::from(intl) * 60)
                .collect();
            return Ok(rec);
        }
        if rt == 101 {
            return Err(bad("pattern record type 101 without a TS- D part".into()));
        }

        let julsd = interval::parse_d_part(d)
            .ok_or_else(|| bad(format!("D part '{}' is not a block date", lossy(d))))?;
        let span = interval::regular_block_for(intl, self.header().version_number);
        let ymd = interval::jliymd(julsd);
        let base = interval::position_base(i64::from(intl), ymd);
        rec.block_start_julian = Some(julsd);
        rec.time_offset_minutes = offset;
        let n = info.logical_values;
        if n < 0 {
            return Err(TsError::Corrupt {
                address: Some(info.address),
                detail: format!("logical value count {n} (KILNDA)"),
            });
        }
        let nu = n as usize;
        // dss6.md §7.2: KILNDA = NOPERS(block start, next block start) for a standard block.
        let next = interval::next_block_ymd(span, interval::block_start_ymd(julsd, span));
        let next_base = interval::position_base(i64::from(intl), next);
        let expect = interval::nopers(i64::from(intl), base, 0, next_base, 0);
        rec.nonstandard_block =
            i64::from(n) != expect || interval::block_start_ymd(julsd, span) != ymd;

        let quality_flag = info.quality != 0;
        if info.compression > 0 {
            // zrrtsb6.f lines 73-121; compression implies REAL*4 without quality.
            if rt == 105 {
                return Err(TsError::Corrupt {
                    address: Some(info.address),
                    detail: "compressed record stored as doubles".into(),
                });
            }
            let (ca, cn) = info.compression_header;
            if !(1..=MAX_HEADER_WORDS).contains(&cn) {
                return Err(TsError::Corrupt {
                    address: Some(info.address),
                    detail: format!("compression header length {cn}"),
                });
            }
            let cn = (cn as usize).min(MAX_COMPRESSION_HEADER_WORDS);
            let ch = self.read_words(ca.into(), cn)?;
            let data = self.data_words_checked(info, i64::from(info.data_words.max(0)))?;
            let (hdr, vals) =
                compress::decode(&ch, &data, opts.allow_sig_digits).map_err(|e| match e {
                    CompressError::UnsupportedScheme(s) => {
                        TsError::UnsupportedCompression { scheme: s }
                    }
                    other => TsError::Corrupt {
                        address: Some(info.address),
                        detail: other.to_string(),
                    },
                })?;
            if hdr.scheme != info.compression {
                return Err(TsError::Corrupt {
                    address: Some(info.address),
                    detail: format!(
                        "compression scheme {} in header, {} in info block",
                        hdr.scheme, info.compression
                    ),
                });
            }
            // dss6.md §8 step 4: NELMS must equal KILNDA.
            if vals.len() != nu {
                return Err(TsError::Corrupt {
                    address: Some(info.address),
                    detail: format!("compressed NELMS {} != KILNDA {n}", vals.len()),
                });
            }
            rec.layout = TsLayout::Compressed(info.compression);
            rec.compression_header = Some(hdr);
            rec.values = TsValues::F32(vals);
        } else if rt == 100 && quality_flag {
            // zrrtsb6.f lines 186-217: (REAL*4 value, INTEGER*4 quality) interleaved.
            let w = self.data_words_checked(info, 2 * i64::from(n))?;
            rec.values = TsValues::F32(w.chunks(2).map(|c| f32::from_bits(c[0] as u32)).collect());
            rec.quality = Some(w.chunks(2).map(|c| c[1]).collect());
        } else if rt == 100 {
            let w = self.data_words_checked(info, i64::from(n))?;
            rec.values = TsValues::F32(w.iter().map(|&x| f32::from_bits(x as u32)).collect());
        } else {
            // 105: REAL*8 values (low word first), then KILNDA quality words
            // (zrrtsb6.f lines 129-184).
            let need = if quality_flag { 3 } else { 2 } * i64::from(n);
            let w = self.data_words_checked(info, need)?;
            rec.values = TsValues::F64(
                (0..nu)
                    .map(|i| f64_from_words(w[2 * i], w[2 * i + 1]))
                    .collect(),
            );
            if quality_flag {
                rec.quality = Some(w[2 * nu..3 * nu].to_vec());
            }
        }
        rec.times = regular_times(i64::from(intl), base, nu, i64::from(offset));
        Ok(rec)
    }

    fn decode_irregular(
        &self,
        info: &Dss6Info,
        _d: &[u8],
        e: &[u8],
        pattern: bool,
    ) -> Result<Dss6TsRecord, TsError> {
        let rt = info.record_type;
        let ih = self.read_internal_header(info, ITS_HEADER_WORDS)?;
        let nihead = info.internal_header.1 as usize;
        let user_header = self.read_user_header(info)?;
        let corrupt = |detail: String| TsError::Corrupt {
            address: Some(info.address),
            detail,
        };
        let npairs =
            word(&ih, 1).ok_or_else(|| corrupt("internal header shorter than 2 words".into()))?;
        let capacity = word(&ih, 0);
        let jdate = word(&ih, 2);
        let pseudo = word(&ih, 4).filter(|&m| m > 0);
        let (tz_offset, tz_name, coords, desc) = zone_and_coords(&ih, nihead, 9);
        let block = match interval::parse_e_part(e) {
            Some(EPart::Irregular { block }) => Some(block),
            Some(EPart::Pseudo { block, .. }) => Some(block),
            _ => None,
        };
        if npairs < 0 {
            return Err(corrupt(format!(
                "value count {npairs} (internal header word 2)"
            )));
        }
        let np = npairs as usize;
        let double = rt == 115;
        let qual = info.quality != 0 && !pattern;
        // zritsi6.f lines 233-245: IMULT = 2 + quality + double. Patterns are pairs
        // (lines 171-184).
        let imult = if pattern {
            2
        } else {
            2 + usize::from(qual) + usize::from(double)
        };
        // dss6.md §7.3 / O18: KINDAT may exceed NPAIRS*IMULT; read only NPAIRS groups.
        let w = self.data_words_checked(info, (np as i64) * imult as i64)?;
        let mut times = Vec::with_capacity(np);
        let mut quality = qual.then(|| Vec::with_capacity(np));
        let values = if double && !pattern {
            let mut v = Vec::with_capacity(np);
            for g in w.chunks(imult) {
                v.push(f64_from_words(g[1], g[2]));
                if let Some(q) = quality.as_mut() {
                    q.push(g[3]);
                }
            }
            TsValues::F64(v)
        } else {
            let mut v = Vec::with_capacity(np);
            for g in w.chunks(imult) {
                v.push(f32::from_bits(g[1] as u32));
                if let Some(q) = quality.as_mut() {
                    q.push(g[2]);
                }
            }
            TsValues::F32(v)
        };
        let base_min = if pattern {
            0
        } else {
            let j = jdate.ok_or_else(|| corrupt("internal header shorter than 3 words".into()))?;
            i64::from(j) * DAY_MINUTES
        };
        for g in w.chunks(imult) {
            // ZIRCPY6: time is minutes from the block start day at 00:00.
            times.push((base_min + i64::from(g[0])) * 60);
        }
        if !pattern && block.is_none() {
            return Err(TsError::BadPathname {
                pathname: info.pathname.clone(),
                detail: format!("E part '{}' is not an irregular block", lossy(e)),
            });
        }
        Ok(Dss6TsRecord {
            pathname: info.pathname.clone(),
            record_type: rt,
            layout: if pattern {
                TsLayout::IrregularPattern
            } else {
                TsLayout::Irregular
            },
            interval_minutes: None,
            block,
            block_start_julian: if pattern { None } else { jdate.map(i64::from) },
            times,
            values,
            quality,
            units: units_text(&ih, 5),
            data_type: header_text(&ih, 7, 8),
            time_offset_minutes: 0,
            time_zone_offset: tz_offset,
            time_zone_name: tz_name,
            coordinates: coords,
            coordinate_descriptors: desc,
            compression: info.compression,
            precision: info.precision,
            logical_values: info.logical_values,
            data_words: info.data_words,
            compression_header: None,
            irregular_capacity: capacity,
            pseudo_interval_minutes: pseudo,
            user_header,
            nonstandard_block: false,
            internal_header: ih,
        })
    }
}

/// Stamps (seconds) of the `n` values of a regular block whose positions count from
/// `base` 00:00: value `k` at `INCTIM(interval, k)`, shifted by the record offset.
pub fn regular_times(interval_min: i64, base: i64, n: usize, offset: i64) -> Vec<i64> {
    let mut out = Vec::with_capacity(n);
    let calendar = interval_min > 10_080;
    for k in 1..=n as i64 {
        let (j, m) = if calendar {
            interval::inctim(interval_min, k, base, 0)
        } else {
            // Plain minute arithmetic; same as INCTIM for these intervals.
            let t = base * DAY_MINUTES + k * interval_min;
            (t.div_euclid(DAY_MINUTES), t.rem_euclid(DAY_MINUTES))
        };
        let (j, m) = if offset != 0 {
            interval::apply_offset(j, m, interval_min, offset)
        } else {
            (j, m)
        };
        out.push((j * DAY_MINUTES + m) * 60);
    }
    out
}

impl Dss6TsRecord {
    /// Time-origin facts of a pattern record (P25); `None` for other records.
    ///
    /// The stored offset is internal header word 0 in minutes, reported × 60 and never
    /// applied. It is unreliable: `zsrtpa6.f` writes the uninitialized local `IOFSET`,
    /// and heclib's Fortran reader resets it to 0 (`zrrtsi.f` line 183). heclib C
    /// applies it when the header has at least 5 words (`Public/ztsRetrievePattern.c`
    /// lines 279-288, `internalHeader[0] * SECS_IN_1_MINUTE` in C `int`), which is what
    /// `heclib_first_seconds` reproduces. Irregular DSS6 patterns keep the irregular
    /// header (word 0 is the capacity, `zritsi6.f`), so they report no stored offset.
    pub fn pattern_origin(&self) -> Option<crate::timeseries::PatternOrigin> {
        let source = crate::timeseries::PatternOffsetSource::Dss6InternalHeaderWord0Minutes;
        match self.layout {
            TsLayout::RegularPattern => {
                let word0 = self.internal_header.first().copied();
                let applied = if self.internal_header.len() >= 5 {
                    word0.unwrap_or(0).wrapping_mul(60)
                } else {
                    0
                };
                let interval = self.interval_minutes.map(|m| i64::from(m) * 60);
                Some(crate::timeseries::PatternOrigin {
                    stored_offset_seconds: word0.map(|w| i64::from(w) * 60),
                    stored_offset_source: source,
                    heclib_first_seconds: interval.and_then(|i| {
                        crate::dss7::ts::pattern_heclib_first_seconds(i, i64::from(applied))
                    }),
                })
            }
            TsLayout::IrregularPattern => Some(crate::timeseries::PatternOrigin {
                stored_offset_seconds: None,
                stored_offset_source: source,
                heclib_first_seconds: None,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod p60_header_text_tests {
    use super::*;

    fn words(b: &[u8; 8]) -> Vec<i32> {
        b.chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    /// P60 E1, checked against Monolith 3.3.27 on patched copies of a pydsstools file:
    /// units cut at the first NUL and trimmed (HecTimeSeries.read `units.trim()`); the
    /// data type keeps heclib's C string (`stringFortToC`, then `strlen`).
    #[test]
    fn units_and_type_follow_hec() {
        let cases: [(&[u8; 8], &str, &str); 6] = [
            (b"CFS\0\x1e\x02\0\0", "CFS", "CFS"),
            (b"CFS \0\x1e\x02 ", "CFS", "CFS "),
            (b" CF \0\x1e\x02 ", "CF", " CF "),
            (b"INST\0ABC", "INST", "INST"),
            (b"CFS     ", "CFS", "CFS"),
            (b"PER-AVER", "PER-AVER", "PER-AVER"),
        ];
        for (raw, units, text) in cases {
            let w = words(raw);
            assert_eq!(units_text(&w, 0), units, "{raw:?}");
            assert_eq!(header_text(&w, 0, 8), text, "{raw:?}");
        }
        // All NUL: empty; missing words read as blanks.
        assert_eq!(units_text(&[0, 0], 0), "");
        assert_eq!(header_text(&[], 0, 8), "");
    }

    /// Monolith applies Java `String.trim()` to units, which also removes tabs and other
    /// control characters (P60 review; Monolith 3.3.27 returns `CFS` for both cases).
    #[test]
    fn units_trim_matches_java_trim() {
        assert_eq!(units_text(&words(b"	CFS	   "), 0), "CFS");
        assert_eq!(units_text(&words(b" CFS	 XX"), 0), "CFS");
    }
}

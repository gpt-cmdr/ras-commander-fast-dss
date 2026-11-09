//! DSS7 paired-data reads: record types 200 (PD, float) and 205 (PDD,
//! double).
//!
//! The read path is a port of the full-record branch of
//! `heclib_c/src/Public/zpdRetrieve.c:zpdRetrieve()` (lines 189–492), so that ordinates,
//! values, units, labels and precision match `hecdss` (`hec_dss_pdRetrieve`) bit for bit:
//!
//! | Item | Where it is stored | HEC source |
//! |---|---|---|
//! | ordinates | values1, `numberOrdinates` floats (200) or doubles (205) | `zpdRetrieve()` lines 429–482 |
//! | curves | values2, `numberOrdinates * numberCurves`, curve-major | `zpdRetrieve()` lines 429–482; header comment lines 98–106 |
//! | sizes, axis flag, precision | internal header ints 0–4 (`INT_HEAD_pd*`) | `headers/zStructAllocation.h` lines 107–112; `zpdRetrieve()` lines 329–344 |
//! | units, types, time zone | internal header from int 5 (int 6 when padded) | `Internal/zpdUnitsToHead.c:zpdHeadToUnits()` lines 93–176; `Utilities/stringHeaderUtility.c:unitsHavePadding()` |
//! | labels | header2, `labelsLength` bytes of NUL-terminated strings | `zpdRetrieve()` lines 367–416 |
//! | user header | user header area | `zpdRetrieve()` lines 361–366 |
//!
//! Values keep their stored bits; missing markers are not converted. Vertical-datum
//! conversion (`zpdRetrieve()` lines 704–1033) only runs when a caller sets HEC's `VDTM`
//! option, which neither HEC reader used for comparison sets (HEC's C library and HEC
//! Monolith); this reader always returns the stored (native)
//! values. Partial reads (`startingCurve`, `startingOrdinate`, ...) are not ported:
//! callers slice the full record.
//!
//! Stored sizes are checked before every read: an internal header, ordinate or value
//! area shorter than its declared size is reported as [`PdError::Corrupt`] where HEC
//! would read past the end of its buffer.

use std::fmt;
use std::io::{Read, Seek};

use super::info::Area;
use super::{Dss7Error, Dss7File, RecordRef};
use crate::paireddata::{PairedData, PdValues, PD, PDD};

/// `INT_HEAD_pdNumberOrdinates` .. `INT_HEAD_pdUnits` (`headers/zStructAllocation.h`
/// lines 107–112).
const IH_NUMBER_ORDINATES: usize = 0;
const IH_NUMBER_CURVES: usize = 1;
const IH_INDEPENDENT_IS_X: usize = 2;
const IH_LABELS_LENGTH: usize = 3;
const IH_PRECISION: usize = 4;
const IH_UNITS: usize = 5;
/// `char carray[400]` in `zpdHeadToUnits()`: the unit strings are truncated to it.
const UNITS_CARRAY: usize = 400;
/// Internal-header ints kept in [`Dss7PdMeta::internal_header`] (only the first
/// `IH_UNITS + 1 + 100` can affect the decode).
const MAX_IH_KEPT: usize = 1024;
/// Largest area (int32 words) read for one record.
const MAX_AREA_INTS: i64 = 1 << 30;

/// Errors from the DSS7 paired-data API.
#[derive(Debug)]
pub enum PdError {
    /// File-level error (I/O, out-of-bounds read, bad info block).
    Dss7(Dss7Error),
    /// No live record with this pathname (HEC `STATUS_RECORD_NOT_FOUND`).
    NotFound { pathname: Vec<u8> },
    /// The record is not paired data (types 200/205): time series, grid, text, location,
    /// ... (`zpdRetrieve()` lines 289–296 return `WRONG_RECORD_TYPE`).
    UnsupportedRecordType { pathname: Vec<u8>, record_type: i32 },
    /// A stored size is inconsistent with the areas it describes.
    Corrupt {
        pathname: Vec<u8>,
        info_address: i64,
        what: &'static str,
    },
}

impl fmt::Display for PdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = |b: &[u8]| -> String { b.iter().map(|&c| c as char).collect() };
        match self {
            PdError::Dss7(e) => write!(f, "{e}"),
            PdError::NotFound { pathname } => {
                write!(f, "no paired-data record {}", p(pathname))
            }
            PdError::UnsupportedRecordType {
                pathname,
                record_type,
            } => write!(
                f,
                "record {} has type {record_type}, which is not paired data (200/205)",
                p(pathname)
            ),
            PdError::Corrupt {
                pathname,
                info_address,
                what,
            } => write!(
                f,
                "paired-data record {} (info at word {info_address}) is corrupt: {what}",
                p(pathname)
            ),
        }
    }
}

impl std::error::Error for PdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PdError::Dss7(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Dss7Error> for PdError {
    fn from(e: Dss7Error) -> Self {
        PdError::Dss7(e)
    }
}

/// DSS7 facts about a paired-data read that the shared model does not carry.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss7PdMeta {
    /// Internal header ints as stored (at most 1024 kept).
    pub internal_header: Vec<i32>,
    /// `kinfoInternalHeadNumber`, `kinfoHeader2Number`, `kinfoValues1Number`,
    /// `kinfoValues2Number`, `kinfoUserHeadNumber` (int32 counts as stored).
    pub internal_header_ints: i64,
    pub header2_ints: i64,
    pub values1_ints: i64,
    pub values2_ints: i64,
    pub user_header_ints: i64,
    /// `INT_HEAD_pdLabelsLength` as stored.
    pub labels_length_stored: i32,
    /// HEC's returned label buffer (`pds->labels`, `pds->labelsLength` bytes): each
    /// label followed by a NUL. Empty when no labels.
    pub labels_raw: Vec<u8>,
    /// Unit strings start at int 6 (blank-padded int 5, 7-HA and later writers) rather
    /// than int 5 (`unitsHavePadding()`).
    pub units_padded: bool,
    /// `INT_HEAD_pdPrecision` as stored (−1 or `10*x + y`).
    pub precision_raw: i32,
    /// `numberOrdinates * numberCurves` differs from the stored value count (HEC only
    /// prints a diagnostic, `zpdRetrieve()` lines 346–352; extra stored values are
    /// ignored).
    pub value_count_mismatch: bool,
    /// Program that wrote the record and its last-write time (ms since 1970).
    pub program: String,
    pub last_write_ms: i64,
}

/// A decoded DSS7 paired-data record: the shared model plus DSS7 extras.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss7Pd {
    pub pd: PairedData,
    pub meta: Dss7PdMeta,
}

/// Units, types and time zone decoded from the internal header (`zpdHeadToUnits()`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PdStrings {
    pub units_independent: Option<String>,
    pub type_independent: Option<String>,
    pub units_dependent: Option<String>,
    pub type_dependent: Option<String>,
    pub time_zone: Option<String>,
    pub padded: bool,
}

/// Latin-1 decode (lossless for bytes) with `mallocAndCopyTrim()` blank trimming.
fn latin1_trim(b: &[u8]) -> String {
    let s: String = b.iter().map(|&c| c as char).collect();
    s.trim_matches(' ').to_string()
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// `zpdHeadToUnits()` (`Internal/zpdUnitsToHead.c` lines 93–176) on the internal header
/// ints `ih` (all `internalHeaderNumber` of them, or a prefix of at least 106).
pub(crate) fn decode_units(ih: &[i32], internal_header_number: usize) -> PdStrings {
    let mut out = PdStrings::default();
    // len = internalHeaderNumber - INT_HEAD_pdUnits; nothing when len <= 0.
    if internal_header_number <= IH_UNITS {
        return out;
    }
    let len = internal_header_number - IH_UNITS;
    let w = |i: usize| ih.get(i).copied().unwrap_or(0);
    // unitsHavePadding(): int 5 is 0 or four blanks.
    let i5 = w(IH_UNITS);
    out.padded = i5 == 0 || i5.to_le_bytes() == *b"    ";
    let (start, nbytes) = if out.padded {
        (IH_UNITS + 1, (len - 1) * 4)
    } else {
        (IH_UNITS, len * 4)
    };
    // charInt / charLong copy at most sizeof(carray) bytes.
    let nbytes = nbytes.min(UNITS_CARRAY);
    let bytes: Vec<u8> = ih
        .get(start..)
        .unwrap_or(&[])
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .take(nbytes)
        .collect();
    // strlen() at each position; the end of the copied bytes terminates (HEC's carray is
    // uninitialized there).
    let cstr = |pos: usize| -> &[u8] {
        let s = bytes.get(pos..).unwrap_or(&[]);
        let e = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        &s[..e]
    };
    let nonblank = |s: &[u8]| s.iter().any(|&b| b != b' ');
    let mut ipos = 0usize;
    let slots: [&mut Option<String>; 5] = [
        &mut out.units_independent,
        &mut out.type_independent,
        &mut out.units_dependent,
        &mut out.type_dependent,
        &mut out.time_zone,
    ];
    for slot in slots {
        let s = cstr(ipos);
        // "if (!len) return 0;": an empty string ends the decode.
        if s.is_empty() {
            break;
        }
        // trimLength(): an all-blank string leaves the field unset.
        if nonblank(s) {
            *slot = Some(latin1_trim(s));
        }
        ipos += s.len() + 1;
    }
    out
}

/// Labels as `zpdRetrieve()` (lines 367–416) extracts them from header2 for a full read.
/// Returns `(labels, raw)`; `raw` is HEC's `pds->labels` buffer. `None` when header2 is
/// empty (no labels).
pub(crate) fn decode_labels(
    header2: &[u8],
    header2_ints: i64,
    labels_length: i32,
    number_curves: usize,
) -> Option<(Vec<String>, Vec<u8>)> {
    if header2_ints <= 0 {
        return None;
    }
    // charInt(header2, clabels, labelsLength, ...): labelsLength bytes into a zeroed
    // buffer of (numberLongsInInts(header2Number) + 1) * 8 bytes.
    let ll = usize::try_from(labels_length).unwrap_or(0);
    let copied = ll.min(header2.len());
    let buf = &header2[..copied];
    let cstr = |pos: usize| -> &[u8] {
        let s = buf.get(pos..).unwrap_or(&[]);
        let e = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        &s[..e]
    };
    let mut labels = Vec::new();
    let mut raw = Vec::new();
    let mut ipos = 0usize;
    for _ in 0..number_curves {
        let s = cstr(ipos);
        labels.push(latin1(s));
        raw.extend_from_slice(s);
        raw.push(0);
        ipos += s.len() + 1;
        // "if (ipos >= pds->labelsLength) break;" (signed compare in HEC). A labelsLength
        // past the stored header2 bytes is corrupt: HEC would read past its buffer; this
        // reader stops at the end of the stored bytes.
        if (ipos as i64) >= i64::from(labels_length) || ipos >= buf.len() {
            break;
        }
    }
    Some((labels, raw))
}

/// Precision split (`zpdRetrieve()` lines 337–344).
pub(crate) fn split_precision(p: i32) -> (i32, i32) {
    if p < 0 {
        (-1, -1)
    } else {
        let x = p / 10;
        (x, p - x * 10)
    }
}

fn ints_from_bytes(b: &[u8], n: usize) -> Vec<i32> {
    b.chunks_exact(4)
        .take(n)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl<R: Read + Seek> Dss7File<R> {
    fn pd_corrupt(rec: &RecordRef, what: &'static str) -> PdError {
        PdError::Corrupt {
            pathname: rec.pathname.clone(),
            info_address: rec.info_address,
            what,
        }
    }

    /// Read the first `n` int32s of an area (`n` ≤ the stored count, checked by callers).
    fn pd_area_bytes(&mut self, rec: &RecordRef, a: Area, n: usize) -> Result<Vec<u8>, PdError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let words = (n as i64 + 1) / 2;
        let mut bytes = self.read_words(a.address, words).map_err(|e| match e {
            Dss7Error::ReadOutOfBounds { .. } => {
                Self::pd_corrupt(rec, "data area outside the file")
            }
            other => PdError::Dss7(other),
        })?;
        bytes.truncate(n * 4);
        Ok(bytes)
    }

    fn checked_count(rec: &RecordRef, a: Area, what: &'static str) -> Result<usize, PdError> {
        if a.number_ints < 0 || a.number_ints > MAX_AREA_INTS {
            return Err(Self::pd_corrupt(rec, what));
        }
        Ok(a.number_ints as usize)
    }

    /// Look up `pathname` and read it as paired data.
    pub fn read_pd(&mut self, pathname: &[u8]) -> Result<Dss7Pd, PdError> {
        let rec = self.lookup(pathname)?.ok_or_else(|| PdError::NotFound {
            pathname: pathname.to_vec(),
        })?;
        self.read_pd_record(&rec)
    }

    /// Read one paired-data record already found by [`Dss7File::lookup`] or
    /// [`Dss7File::record_at`], as `zpdRetrieve(ifltab, pds, 0)` does for a full read
    /// (values in their stored width).
    pub fn read_pd_record(&mut self, rec: &RecordRef) -> Result<Dss7Pd, PdError> {
        let info = rec.info.clone();
        // zread() takes the data type from the info block.
        let rt = info.record_type;
        if rt != PD && rt != PDD {
            return Err(PdError::UnsupportedRecordType {
                pathname: rec.pathname.clone(),
                record_type: rt,
            });
        }
        // Internal header: at least INT_HEAD_pdUnits ints (zpdRetrieve() lines 289–296).
        let ih_n = Self::checked_count(rec, info.internal_header, "internal header length")?;
        if ih_n < IH_UNITS {
            return Err(Self::pd_corrupt(
                rec,
                "internal header shorter than 5 ints (HEC: WRONG_RECORD_TYPE)",
            ));
        }
        let ih_bytes = self.pd_area_bytes(rec, info.internal_header, ih_n.min(MAX_IH_KEPT))?;
        let ih = ints_from_bytes(&ih_bytes, ih_n.min(MAX_IH_KEPT));
        let no = ih[IH_NUMBER_ORDINATES];
        let nc = ih[IH_NUMBER_CURVES];
        if no < 0 || nc < 0 {
            return Err(Self::pd_corrupt(rec, "negative ordinate or curve count"));
        }
        let (no, nc) = (no as usize, nc as usize);
        let elem: usize = if rt == PD { 1 } else { 2 };
        let n_values = no
            .checked_mul(nc)
            .ok_or_else(|| Self::pd_corrupt(rec, "ordinate × curve count overflows"))?;
        let need1 = no
            .checked_mul(elem)
            .ok_or_else(|| Self::pd_corrupt(rec, "ordinate count overflows"))?;
        let need2 = n_values
            .checked_mul(elem)
            .ok_or_else(|| Self::pd_corrupt(rec, "value count overflows"))?;
        let v1_n = Self::checked_count(rec, info.values1, "values1 length")?;
        let v2_n = Self::checked_count(rec, info.values2, "values2 length")?;
        if need1 > v1_n {
            return Err(Self::pd_corrupt(
                rec,
                "fewer ordinates stored than numberOrdinates",
            ));
        }
        if need2 > v2_n {
            return Err(Self::pd_corrupt(
                rec,
                "fewer values stored than numberOrdinates × numberCurves",
            ));
        }
        let v1 = self.pd_area_bytes(rec, info.values1, need1)?;
        let v2 = self.pd_area_bytes(rec, info.values2, need2)?;
        let (ordinates, values) = if rt == PD {
            let f = |b: &[u8]| -> Vec<f32> {
                b.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            };
            (PdValues::F32(f(&v1)), PdValues::F32(f(&v2)))
        } else {
            let f = |b: &[u8]| -> Vec<f64> {
                b.chunks_exact(8)
                    .map(|c| {
                        let mut a = [0u8; 8];
                        a.copy_from_slice(c);
                        f64::from_le_bytes(a)
                    })
                    .collect()
            };
            (PdValues::F64(f(&v1)), PdValues::F64(f(&v2)))
        };

        let strings = decode_units(&ih, ih_n);

        let h2_n = Self::checked_count(rec, info.header2, "header2 length")?;
        let labels_length = ih[IH_LABELS_LENGTH];
        let want_h2 = usize::try_from(labels_length)
            .unwrap_or(0)
            .div_ceil(4)
            .min(h2_n);
        let h2 = self.pd_area_bytes(rec, info.header2, want_h2)?;
        let (labels, labels_raw) =
            decode_labels(&h2, info.header2.number_ints, labels_length, nc).unwrap_or_default();

        let uh_n = Self::checked_count(rec, info.user_header, "user header length")?;
        let uh_bytes = self.pd_area_bytes(rec, info.user_header, uh_n)?;
        let user_header = ints_from_bytes(&uh_bytes, uh_n);

        let precision_raw = ih[IH_PRECISION];
        let (x_precision, y_precision) = split_precision(precision_raw);
        let pd = PairedData {
            pathname: rec.pathname.clone(),
            record_type: rt,
            number_ordinates: no,
            number_curves: nc,
            ordinates,
            values,
            units_independent: strings.units_independent.unwrap_or_default(),
            type_independent: strings.type_independent.unwrap_or_default(),
            units_dependent: strings.units_dependent.unwrap_or_default(),
            type_dependent: strings.type_dependent.unwrap_or_default(),
            labels,
            independent_is_x_axis: ih[IH_INDEPENDENT_IS_X],
            x_precision,
            y_precision,
            time_zone: strings.time_zone,
            user_header,
        };
        let meta = Dss7PdMeta {
            internal_header: ih,
            internal_header_ints: info.internal_header.number_ints,
            header2_ints: info.header2.number_ints,
            values1_ints: info.values1.number_ints,
            values2_ints: info.values2.number_ints,
            user_header_ints: info.user_header.number_ints,
            labels_length_stored: labels_length,
            labels_raw,
            units_padded: strings.padded,
            precision_raw,
            value_count_mismatch: need2 != v2_n,
            program: info.program.clone(),
            last_write_ms: info.last_write_ms,
        };
        Ok(Dss7Pd { pd, meta })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(s: &[u8]) -> Vec<i32> {
        let mut b = s.to_vec();
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn padded_units_and_time_zone() {
        let mut ih = vec![3, 1, 1, 0, 21, i32::from_le_bytes(*b"    ")];
        ih.extend(pack(b"FEET   \0Linear \0CFS    \0Log    \0UTC    \0"));
        let n = ih.len();
        let s = decode_units(&ih, n);
        assert!(s.padded);
        assert_eq!(s.units_independent.as_deref(), Some("FEET"));
        assert_eq!(s.type_independent.as_deref(), Some("Linear"));
        assert_eq!(s.units_dependent.as_deref(), Some("CFS"));
        assert_eq!(s.type_dependent.as_deref(), Some("Log"));
        assert_eq!(s.time_zone.as_deref(), Some("UTC"));
    }

    #[test]
    fn empty_string_stops_and_blank_is_unset() {
        // Unpadded (deprecated) layout from int 5; blank type; empty units-dependent.
        let mut ih = vec![3, 1, 1, 0, -1];
        ih.extend(pack(b"FT\0   \0\0CFS\0"));
        let n = ih.len();
        let s = decode_units(&ih, n);
        assert!(!s.padded);
        assert_eq!(s.units_independent.as_deref(), Some("FT"));
        assert_eq!(s.type_independent, None);
        assert_eq!(s.units_dependent, None);
        assert_eq!(s.type_dependent, None);
        // Header of exactly 5 ints: nothing.
        assert_eq!(decode_units(&[1, 1, 0, 0, 0], 5), PdStrings::default());
        // Declared longer than present: no panic.
        let _ = decode_units(&[1, 1, 0, 0, 0, 0x2020_2020], 400);
    }

    #[test]
    fn labels_follow_zpd_retrieve() {
        let h2 = b"Total\0Residential\0Commercial\0\0\0\0".to_vec();
        let (l, raw) = decode_labels(&h2, 8, 29, 3).unwrap();
        assert_eq!(l, vec!["Total", "Residential", "Commercial"]);
        assert_eq!(raw.len(), 29);
        // labelsLength shorter than the curve count: stops early.
        let (l, _) = decode_labels(&h2, 8, 6, 3).unwrap();
        assert_eq!(l, vec!["Total"]);
        // header2 present, labelsLength 0: HEC returns one empty label.
        let (l, raw) = decode_labels(&h2, 8, 0, 3).unwrap();
        assert_eq!(l, vec![String::new()]);
        assert_eq!(raw, vec![0]);
        // No header2: no labels.
        assert!(decode_labels(&[], 0, 10, 3).is_none());
        // labelsLength past the stored bytes: stops at the end of header2.
        let (l, _) = decode_labels(&h2[..4], 1, i32::MAX, usize::MAX).unwrap();
        assert_eq!(l, vec!["Tota"]);
        assert_eq!(split_precision(-1), (-1, -1));
        assert_eq!(split_precision(32), (3, 2));
        assert_eq!(split_precision(7), (0, 7));
    }
}

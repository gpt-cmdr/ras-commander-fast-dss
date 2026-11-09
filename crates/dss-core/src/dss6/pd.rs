//! DSS6 paired-data reads: record types 200 (PD, `REAL*4`) and 205 (PDD,
//! `REAL*8`).
//!
//! The read path ports what HEC's C API returns for a DSS6 record,
//! `heclib_c/src/DssInterface/v6and7/zpdRetrieve6.c:zpdRetrieve6()` over the Fortran
//! reader `heclib_f/src/zrpdi6.f:ZRPDI6`:
//!
//! | Item | Layout | HEC source |
//! |---|---|---|
//! | sizes | internal header word 1 `NORD`, 2 `NCURVE`, 3 `IHORIZ` (1-based) | `zrpdi6.f` lines 171–173 |
//! | units / types | words 4–5, 6–7, 8–9, 10–11: 8 characters each | `zrpdi6.f` lines 180–188; `zpdRetrieve6.c` lines 120–123, 187–190 (`stringFortToC`) |
//! | short labels | 12 characters at word `3*I+9` for curve `I` | `zrpdi6.f` lines 214–230 |
//! | long labels | word `3*NCURVE+12` = `NWDS`, then `NWDS` words per label | `zrpdi6.f` lines 192–213 |
//! | label strings | at most 100 labels of 101 characters, trailing blanks removed | `zpdRetrieve6.c` lines 44, 124–126, 196–212 |
//! | data | `NORD` ordinates then `NCURVE*NORD` values, curve-major, `REAL*4` or `REAL*8` | `zpdRetrieve6.c` lines 163–185; `zrpdi6.f` lines 83–106 |
//! | precision | info `KIPREC` (`10*x + y`, negative = unset) | `zrecordInfo6.f` line 72; `zpdRetrieve6.c` lines 129–136 |
//!
//! Sizes come from the internal header, never from `KILNDA`, which is stale for many PD
//! records (dss6.md §5.2). The data length `KINDAT` bounds every read: HEC reads
//! `KINDAT` words into a zeroed buffer, so a record whose header declares more values
//! than `KINDAT` holds would come back zero-filled; this reader reports it as
//! [`PdError::Corrupt`] instead (see [`Dss6PdMeta`]). Vertical-datum conversion
//! (`zrpdi6.f` lines 237–392) only runs when HEC's `VDTM` option is set; values are
//! returned as stored. Words HEC would take from its stale common buffer (headers
//! shorter than the fields it reads) read as blanks here.

use super::header::fort_to_c;
use super::lookup::Dss6Record;
use super::source::byte_offset_word;
use super::{Dss6Error, Dss6File, Dss6Info, ReadAt};
use crate::catalog::CatalogEntry;
use crate::paireddata::{PairedData, PdValues, PD, PDD};
use std::fmt;

/// `NGBUFF` (`heclib_f/src/headers/zdssbf.h` line 5): internal-header words HEC reads.
pub const NGBUFF: usize = 10_020;
/// `clabel[100][101]` in `zpdRetrieve6()`: label count and length HEC returns.
pub const MAX_LABELS: usize = 100;
pub const LABEL_CHARS: usize = 101;
/// `CPPATH` is `CHARACTER(len=dss_maxpath)` (`headers/zdsscc.h`, `dss_parameters.h`).
const CPPATH_CHARS: usize = 392;
/// Sanity cap on the user-header length before any read.
const MAX_HEADER_WORDS: i32 = 1_000_000;

/// Why a DSS6 paired-data record could not be read.
#[derive(Debug)]
pub enum PdError {
    /// File-level or structural failure from the DSS6 reader.
    Dss6(Dss6Error),
    /// No live record with this pathname.
    NotFound { pathname: Vec<u8> },
    /// Not paired data (200/205): `zrpdi6.f` lines 68–80 (`ISTAT = 20`).
    UnsupportedRecordType { record_type: i32 },
    /// Record password set; HEC denies access for files `6-E` and later.
    PasswordProtected,
    /// Header or length inconsistent.
    Corrupt {
        address: Option<i64>,
        detail: String,
    },
}

impl fmt::Display for PdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdError::Dss6(e) => write!(f, "{e}"),
            PdError::NotFound { pathname } => write!(
                f,
                "record not found: {}",
                pathname.iter().map(|&c| c as char).collect::<String>()
            ),
            PdError::UnsupportedRecordType { record_type } => {
                write!(f, "record type {record_type} is not paired data (200/205)")
            }
            PdError::PasswordProtected => write!(f, "record is password protected"),
            PdError::Corrupt { address, detail } => match address {
                Some(a) => write!(f, "corrupt paired-data record at word {a}: {detail}"),
                None => write!(f, "corrupt paired-data record: {detail}"),
            },
        }
    }
}

impl std::error::Error for PdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PdError::Dss6(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Dss6Error> for PdError {
    fn from(e: Dss6Error) -> Self {
        PdError::Dss6(e)
    }
}

/// How curve labels are stored in the internal header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelStyle {
    /// No labels returned (`LABEL = .FALSE.`).
    None,
    /// 12-character labels, three words per curve after word 11.
    Short,
    /// Long labels: a word count, then that many words per curve.
    Long { words_per_label: i32 },
}

/// DSS6 facts about a paired-data read that the shared model does not carry.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss6PdMeta {
    /// Internal header words as read (`min(KINIHE, NGBUFF)`).
    pub internal_header: Vec<i32>,
    /// `KINIHE` as stored.
    pub internal_header_words: i32,
    pub label_style: LabelStyle,
    /// `KINDAT` (stored data words) and `KILNDA` (logical count; stale for PD).
    pub data_words: i32,
    pub logical_values: i32,
    /// `KILNDA` differs from `NORD*(NCURVE+1)` (informational).
    pub logical_values_stale: bool,
    /// Data words stored beyond `NORD*(NCURVE+1)` values (ignored, as HEC does).
    pub extra_data_words: i64,
    /// `KIPREC` as stored.
    pub precision_raw: i32,
    /// Program, last-write date and time, tag from the info block.
    pub program: String,
    pub last_write_date: String,
    pub last_write_time: String,
    pub tag: String,
}

/// A decoded DSS6 paired-data record: the shared model plus DSS6 extras.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss6Pd {
    pub pd: PairedData,
    pub meta: Dss6PdMeta,
}

const BLANK_WORD: i32 = 0x2020_2020;

/// Characters `n` from 0-based word `w`, words past the header reading as blanks.
fn chars(ih: &[i32], w: usize, n: usize) -> Vec<u8> {
    let nw = n.div_ceil(4);
    let mut out: Vec<u8> = (0..nw)
        .flat_map(|i| {
            w.checked_add(i)
                .and_then(|k| ih.get(k))
                .copied()
                .unwrap_or(BLANK_WORD)
                .to_le_bytes()
        })
        .collect();
    out.truncate(n);
    out
}

/// Labels from the internal header (`zrpdi6.f` lines 192–230, then `zpdRetrieve6.c`
/// lines 196–212). `nihead` = `min(KINIHE, NGBUFF)`; `ih` holds those words.
pub(crate) fn decode_labels(ih: &[i32], nihead: usize, ncurve: usize) -> (Vec<String>, LabelStyle) {
    // clabel[i] starts as 101 blanks; ZRPDI6 fills those it reaches.
    let mut clabel: Vec<Vec<u8>> = Vec::new();
    let ncurve_i = ncurve as i64;
    let nihead_i = nihead as i64;
    // NH = NCURVE*3 + 11 (1-based word count of the short-label area).
    let nh = ncurve_i * 3 + 11;
    let style;
    let mut label = false;
    if nihead_i > nh {
        // Word NH+1 (1-based) holds NWDS.
        let nwds = ih.get(nh as usize).copied().unwrap_or(BLANK_WORD);
        let n_end = nh + 1 + i64::from(nwds);
        if nihead_i >= n_end {
            label = true;
            style = LabelStyle::Long {
                words_per_label: nwds,
            };
            // First label at 1-based word NH+2.
            let mut pos = nh + 2;
            for i in 1..=ncurve_i {
                if pos + i64::from(nwds) > NGBUFF as i64 {
                    break;
                }
                if i as usize > MAX_LABELS {
                    break;
                }
                // HOL2CH(IGBUFF(NH), CPPATH, NWDS): NWDS words into CPPATH (blank).
                let mut cp = vec![b' '; CPPATH_CHARS];
                if nwds > 0 {
                    let n = (nwds as usize * 4).min(CPPATH_CHARS);
                    let b = chars(ih, (pos - 1) as usize, n);
                    cp[..n].copy_from_slice(&b);
                }
                cp.truncate(LABEL_CHARS);
                clabel.push(cp);
                pos += i64::from(nwds);
            }
        } else {
            style = LabelStyle::None;
        }
    } else {
        for i in 1..=ncurve_i {
            // JLOC = I*3 + 9 (1-based).
            let jloc = i * 3 + 9;
            if jloc + 2 > nihead_i || i as usize > MAX_LABELS {
                break;
            }
            let mut c = chars(ih, (jloc - 1) as usize, 12);
            // CLABEL(I) (101 chars) = CTEMP (12 chars), blank padded.
            if c[..8].iter().any(|&b| b != b' ') {
                label = true;
            }
            c.resize(LABEL_CHARS, b' ');
            clabel.push(c);
        }
        style = if label {
            LabelStyle::Short
        } else {
            LabelStyle::None
        };
    }
    if !label {
        return (Vec::new(), LabelStyle::None);
    }
    // zpdRetrieve6.c: one label per curve; labels ZRPDI6 did not reach stay blank. HEC's
    // clabel array holds 100 rows and reads past it for more curves (undefined); this
    // reader returns at most 100 labels.
    let labels = (0..ncurve.min(MAX_LABELS))
        .map(|i| clabel.get(i).map(|c| fort_to_c(c)).unwrap_or_default())
        .collect();
    (labels, style)
}

fn f64_from_words(lo: i32, hi: i32) -> f64 {
    f64::from_bits((u64::from(hi as u32) << 32) | u64::from(lo as u32))
}

impl<S: ReadAt> Dss6File<S> {
    /// Look up `pathname` and read it as paired data.
    pub fn read_pd(&self, pathname: &[u8]) -> Result<Dss6Pd, PdError> {
        let rec = self.lookup(pathname)?.ok_or_else(|| PdError::NotFound {
            pathname: pathname.to_vec(),
        })?;
        self.decode_pd_info(&rec.info)
    }

    /// Decode a record already found by [`Self::lookup`].
    pub fn decode_pd(&self, rec: &Dss6Record) -> Result<Dss6Pd, PdError> {
        self.decode_pd_info(&rec.info)
    }

    /// Decode the record a catalog entry points to (its info block is re-read and
    /// checked).
    pub fn read_pd_entry(&self, entry: &CatalogEntry) -> Result<Dss6Pd, PdError> {
        let addr = byte_offset_word(entry.info_address).ok_or_else(|| PdError::Corrupt {
            address: None,
            detail: format!("catalog info offset {} is not a word", entry.info_address),
        })?;
        let npath = i32::try_from(entry.pathname.len()).unwrap_or(i32::MAX);
        let info = self.read_info(addr, npath)?;
        if !info.has_valid_flag() || info.pathname != entry.pathname {
            return Err(PdError::Corrupt {
                address: Some(addr),
                detail: "info block does not match catalog entry".into(),
            });
        }
        self.decode_pd_info(&info)
    }

    /// Decode the paired data an info block describes.
    pub fn decode_pd_info(&self, info: &Dss6Info) -> Result<Dss6Pd, PdError> {
        if self.is_record_denied(info) {
            return Err(PdError::PasswordProtected);
        }
        let rt = info.record_type;
        if rt != PD && rt != PDD {
            return Err(PdError::UnsupportedRecordType { record_type: rt });
        }
        let corrupt = |detail: String| PdError::Corrupt {
            address: Some(info.address),
            detail,
        };
        // zreadx6: min(KINIHE, KIHEAD) internal-header words into IGBUFF(NGBUFF).
        let (ih_addr, kinihe) = info.internal_header;
        if kinihe < 0 {
            return Err(corrupt(format!("internal header length {kinihe}")));
        }
        let nihead = (kinihe as usize).min(NGBUFF);
        if nihead < 3 {
            return Err(corrupt(format!(
                "internal header has {kinihe} words; paired data needs NORD, NCURVE, IHORIZ"
            )));
        }
        let ih = self.read_words(ih_addr.into(), nihead)?;
        let (nord, ncurve, ihoriz) = (ih[0], ih[1], ih[2]);
        if nord < 0 || ncurve < 0 {
            return Err(corrupt(format!("NORD {nord}, NCURVE {ncurve}")));
        }
        let elem: i64 = if rt == PD { 1 } else { 2 };
        // NORD ordinates followed by NCURVE*NORD values (zpdRetrieve6.c lines 163-185).
        let n_values = i64::from(nord) * (i64::from(ncurve) + 1);
        let need = n_values * elem;
        if need > i64::from(info.data_words) {
            return Err(corrupt(format!(
                "header declares {n_values} values ({need} words), record stores {} words (KINDAT); HEC would zero-fill",
                info.data_words
            )));
        }
        let data = self.read_words(info.data_address.into(), need as usize)?;
        let nord_u = nord as usize;
        let (ordinates, values) = if rt == PD {
            let v: Vec<f32> = data.iter().map(|&w| f32::from_bits(w as u32)).collect();
            (
                PdValues::F32(v[..nord_u].to_vec()),
                PdValues::F32(v[nord_u..].to_vec()),
            )
        } else {
            // In a little-endian file a REAL*8 is its two words in order (dss6.md §1.4).
            let v: Vec<f64> = data
                .chunks_exact(2)
                .map(|p| f64_from_words(p[0], p[1]))
                .collect();
            (
                PdValues::F64(v[..nord_u].to_vec()),
                PdValues::F64(v[nord_u..].to_vec()),
            )
        };

        // Units and types: HOLCHR 8 characters from words 4, 6, 8, 10 (1-based).
        let units_independent = fort_to_c(&chars(&ih, 3, 8));
        let type_independent = fort_to_c(&chars(&ih, 5, 8));
        let units_dependent = fort_to_c(&chars(&ih, 7, 8));
        let type_dependent = fort_to_c(&chars(&ih, 9, 8));
        let (labels, label_style) = decode_labels(&ih, nihead, ncurve as usize);

        let (uh_addr, nuhead) = info.user_header;
        if !(0..=MAX_HEADER_WORDS).contains(&nuhead) {
            return Err(corrupt(format!("user header length {nuhead}")));
        }
        let user_header = self.read_words(uh_addr.into(), nuhead as usize)?;

        let (x_precision, y_precision) = super::super::dss7::pd::split_precision(info.precision);
        let pd = PairedData {
            pathname: info.pathname.clone(),
            record_type: rt,
            number_ordinates: nord_u,
            number_curves: ncurve as usize,
            ordinates,
            values,
            units_independent,
            type_independent,
            units_dependent,
            type_dependent,
            labels,
            independent_is_x_axis: ihoriz,
            x_precision,
            y_precision,
            time_zone: None,
            user_header,
        };
        let meta = Dss6PdMeta {
            internal_header: ih,
            internal_header_words: kinihe,
            label_style,
            data_words: info.data_words,
            logical_values: info.logical_values,
            logical_values_stale: i64::from(info.logical_values) != n_values,
            extra_data_words: i64::from(info.data_words) - need,
            precision_raw: info.precision,
            program: info.program.clone(),
            last_write_date: info.last_write_date.clone(),
            last_write_time: info.last_write_time.clone(),
            tag: info.tag.clone(),
        };
        Ok(Dss6Pd { pd, meta })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &[u8]) -> Vec<i32> {
        let mut b = s.to_vec();
        while b.len() % 4 != 0 {
            b.push(b' ');
        }
        b.chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    fn header(ncurve: i32) -> Vec<i32> {
        let mut ih = vec![5, ncurve, 1];
        ih.extend(words(b"FEET    UNT     CFS     UNT     "));
        ih
    }

    #[test]
    fn no_labels_for_11_word_header() {
        let ih = header(1);
        assert_eq!(decode_labels(&ih, ih.len(), 1), (vec![], LabelStyle::None));
        assert_eq!(fort_to_c(&chars(&ih, 3, 8)), "FEET");
        assert_eq!(fort_to_c(&chars(&ih, 9, 8)), "UNT");
        // Past the header: blanks.
        assert_eq!(fort_to_c(&chars(&ih, 50, 8)), "");
    }

    #[test]
    fn short_labels() {
        let mut ih = header(2);
        ih.extend(words(b"Total       Residential "));
        let (l, s) = decode_labels(&ih, ih.len(), 2);
        assert_eq!(s, LabelStyle::Short);
        assert_eq!(l, vec!["Total", "Residential"]);
        // Blank first 8 characters on every label: no labels.
        let mut ih = header(1);
        ih.extend(words(b"        abcd"));
        assert_eq!(decode_labels(&ih, ih.len(), 1).1, LabelStyle::None);
        // Only room for the first label: the second is blank.
        let mut ih = header(2);
        ih.extend(words(b"Total       "));
        let (l, _) = decode_labels(&ih, ih.len(), 2);
        assert_eq!(l, vec!["Total", ""]);
    }

    #[test]
    fn long_labels() {
        let mut ih = header(2);
        ih.extend(words(b"Total       Other       "));
        ih.push(5); // NWDS
        ih.extend(words(b"A long first label  "));
        ih.extend(words(b"Second\0xx            "));
        let (l, s) = decode_labels(&ih, ih.len(), 2);
        assert_eq!(s, LabelStyle::Long { words_per_label: 5 });
        assert_eq!(l, vec!["A long first label", "Second"]);
        // Negative or absurd NWDS: no panic.
        let mut ih2 = header(1);
        ih2.extend(words(b"x           "));
        ih2.push(-3);
        ih2.push(0);
        let (l, _) = decode_labels(&ih2, ih2.len(), 1);
        assert_eq!(l, vec![""]);
        let mut ih3 = header(1);
        ih3.extend(words(b"x           "));
        ih3.push(i32::MAX);
        let (_, s) = decode_labels(&ih3, ih3.len(), 1);
        assert_eq!(s, LabelStyle::None);
    }

    #[test]
    fn many_curves_do_not_panic() {
        let ih = header(1_000_000);
        let (l, _) = decode_labels(&ih, ih.len(), 1_000_000);
        assert!(l.is_empty());
    }
}

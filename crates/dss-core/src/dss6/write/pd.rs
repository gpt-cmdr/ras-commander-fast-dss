//! DSS6 paired-data store: the full-record path of
//! `DssInterface/v6and7/zpdStore6.c:zpdStore6()` (`heclib_c`) → `zspdi6.f:ZSPDI6` with
//! `IPLAN = 0` (normal write) → `zwritex6.f` (hec-dss tag `7-IU-15`; Fortran paths
//! relative to `heclib/heclib_f/src/`). This is the path HEC Monolith 3.3.27's
//! `HecPairedData` takes for a DSS6 file (`javaHeclib` `Hec_zpdStore` → `zpdStore()` →
//! `zpdStore6()`, `Public/zpdStore.c` lines 280-316).
//!
//! Record (`ZSPDI6` lines 366-526):
//! - internal header (`IGBUFF`), 1-based words: 1 `NORD`, 2 `NCURVE`, 3 `IHORIZ` (1, or 2
//!   when given 2), 4-5, 6-7, 8-9, 10-11 the X units, X type, Y units and Y type
//!   (`CHRHOL` of 8 characters, blank-padded); `NIHEAD` 11 without labels;
//! - labels: `NIHEAD = 3·NCURVE + 11`, the first 12 characters of label `I` at words
//!   `3I+9 .. 3I+11`; when the longest label (trailing blanks trimmed, `CHRLNB`) is longer
//!   than 12, `NWDS = (MAXLABEL-1)/4 + 1` follows at word `NIHEAD+1` and then every label in
//!   `NWDS` words (the long labels);
//! - data: the `NORD` ordinates, then the `NCURVE·NORD` values curve-major
//!   (`zpdStore6()` lines 120-140), `REAL*4` (200) or `REAL*8` (205, low word first);
//!   `NDATA` words, `KILNDA` = `NDATA` (`NLDATA` is -1 after every write, `znwrit6.f` line
//!   220);
//! - `KIPREC` from `zset6('PREC')` (`zpdStore6()` lines 165-171): `10·x + y`, or -1
//!   (an x precision of 0 is refused: HEC stores -1 for it).

use super::engine::{blank_padded, Engine, Found, Lookup, Record6, I_NUHE, I_TYPE};
use super::path::check_string;
use crate::dss7::write::pd::{check_common, PdInput};
use crate::dss7::write::{validate_pd_pathname, BlockReport, WriteError};
use crate::paireddata::{PD, PDD};

/// `zspdi6.f` line 366: at most 100 curves in one DSS6 record.
pub const MAX_CURVES6: usize = 100;
/// `CTEMP*100` in `ZSPDI6` (labels are copied through it): longer labels are truncated by
/// HEC, refused here.
pub const MAX_LABEL_CHARS6: usize = 100;

fn bad(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

/// Validated input: the stored (upper-case) pathname and the record words.
pub(super) struct Pd6Plan {
    pub path: Vec<u8>,
    pub record_type: i32,
    pub internal_header: Vec<i32>,
    pub data: Vec<i32>,
    pub precision: i32,
}

fn trimmed_len(s: &[u8]) -> usize {
    s.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1)
}

pub(super) fn plan(input: &PdInput) -> Result<Pd6Plan, WriteError> {
    validate_pd_pathname(input.pathname)?;
    // zcheck6.f lines 70-82: DSS6 stores pathnames in upper case.
    let path = input.pathname.to_ascii_uppercase();
    check_common(input)?;
    // '|' in an elevation-side unit (crack_unit_spec, zspdi6.f lines 187-196).
    crate::dss7::write::pd_elev_units(input)?;
    let nord = input.data.number_ordinates();
    let ncurve = input.number_curves;
    if ncurve > MAX_CURVES6 {
        return Err(bad(format!(
            "{ncurve} curves; a DSS6 paired-data record holds at most {MAX_CURVES6} \
             (zspdi6.f line 366)"
        )));
    }
    // zpdStore6.c lines 165-171 store -1 unless `xprecision > 0`, so HEC never writes an
    // x precision of 0 to DSS6; Monolith 3.3.27 reads a stored `0·10 + y` as (y, y)
    // (observed). Refused rather than dropped to -1 or misread.
    if input.x_precision == 0 {
        return Err(bad(
            "x_precision 0 cannot be stored in DSS6 (heclib stores -1, and Monolith reads 0*10+y as x=y); use -1 or 1..=9",
        ));
    }
    if input.time_zone.is_some() {
        return Err(WriteError::Unsupported(
            "DSS6 paired data stores no time zone; pass none".into(),
        ));
    }
    // IGBUFF(3) = 1, or 2 when IHORIZ is 2 (ZSPDI6): any other value would be changed.
    if input.independent_is_x_axis != 1 && input.independent_is_x_axis != 2 {
        return Err(bad(format!(
            "independent_is_x_axis must be 1 or 2 for DSS6 (HEC stores 1 for any other \
             value), not {}",
            input.independent_is_x_axis
        )));
    }
    for (what, s) in input.strings() {
        check_string(what, s)?;
    }
    let mut ih: Vec<i32> = vec![nord as i32, ncurve as i32, input.independent_is_x_axis];
    for (_, s) in input.strings() {
        ih.extend(blank_padded(s, 2));
    }
    if let Some(labels) = input.labels {
        let mut maxlabel = 0usize;
        for (i, l) in labels.iter().enumerate() {
            if l.len() > MAX_LABEL_CHARS6 {
                return Err(bad(format!(
                    "label {i} is {} characters; DSS6 stores at most {MAX_LABEL_CHARS6}",
                    l.len()
                )));
            }
            if trimmed_len(l) != l.len() {
                return Err(bad(format!(
                    "label {i} ends in blanks; DSS6 readers trim them (zpdRetrieve6.c lines \
                     196-212), so the label would not read back as given"
                )));
            }
            maxlabel = maxlabel.max(l.len());
        }
        // zrpdi6.f / zpdRetrieve6.c return labels only when one of them has a non-blank
        // character in its first 8.
        if !labels.iter().any(|l| l.iter().take(8).any(|&c| c != b' ')) {
            return Err(bad(
                "every label is blank in its first 8 characters; DSS6 readers would return \
                 no labels (pass labels=None)",
            ));
        }
        // Short labels: 12 characters at words 3I+9..3I+11 (1-based).
        for l in labels {
            ih.extend(blank_padded(l, 3));
        }
        debug_assert_eq!(ih.len(), 3 * ncurve + 11);
        if maxlabel > 12 {
            let nwds = (maxlabel - 1) / 4 + 1;
            ih.push(nwds as i32);
            for l in labels {
                ih.extend(blank_padded(l, nwds));
            }
        }
    }
    let (ords, vals) = input.data.words();
    let mut data = ords;
    data.extend(vals);
    let record_type = input.data.record_type();
    Ok(Pd6Plan {
        path,
        record_type,
        internal_header: ih,
        data,
        precision: input.precision_word(),
    })
}

fn build(plan: &Pd6Plan, look: &Lookup) -> Result<Record6, WriteError> {
    if let Found::Yes(ex) = &look.found {
        let t = ex.f(I_TYPE);
        if t != PD && t != PDD {
            // zwritex6/zowrit6 overwrite a record of any type; the writer refuses.
            return Err(WriteError::Unsupported(format!(
                "{}: the existing record has type {t}, not paired data (200/205)",
                String::from_utf8_lossy(&plan.path)
            )));
        }
        if ex.bin_type != t {
            return Err(WriteError::Corrupt(format!(
                "{}: bin type {} differs from info type {t}",
                String::from_utf8_lossy(&plan.path),
                ex.bin_type
            )));
        }
        // ZSPDI6 lines 106-306: for an elevation C part HEC applies vertical-datum handling
        // and re-stores the record's user header when none is given (the DSS6 time-series
        // writer refuses the same case).
        if is_elevation_pd(&plan.path) && ex.f(I_NUHE) > 0 {
            return Err(WriteError::Unsupported(format!(
                "{}: an elevation paired-data record with a user header (HEC applies \
                 vertical-datum handling and keeps the header, zspdi6.f lines 106-306)",
                String::from_utf8_lossy(&plan.path)
            )));
        }
    }
    Ok(Record6 {
        pathname: plan.path.clone(),
        record_type: plan.record_type,
        internal_header: plan.internal_header.clone(),
        logical: plan.data.len() as i32,
        data: plan.data.clone(),
        quality: false,
        precision: plan.precision,
    })
}

/// `Utilities/verticalDatum.c:pathnameIsElevPd()`: either parameter of the C part
/// (split at `-`) starts with `ELEV`.
fn is_elevation_pd(path: &[u8]) -> bool {
    let c = path.split(|&b| b == b'/').nth(3).unwrap_or(&[]);
    c.split(|&b| b == b'-')
        .filter(|p| !p.is_empty())
        .take(2)
        .any(|p| p.len() >= 4 && p[..4].eq_ignore_ascii_case(b"ELEV"))
}

/// Look the record up, build it and store it (one transaction).
pub(super) fn store(eng: &mut Engine, plan: &Pd6Plan) -> Result<Vec<BlockReport>, WriteError> {
    let look = eng.lookup(&plan.path)?;
    let rec = build(plan, &look)?;
    eng.store_all(&[rec])
}

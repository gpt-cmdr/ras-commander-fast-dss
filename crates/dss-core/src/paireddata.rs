//! Version-neutral paired-data model shared by the DSS6 and DSS7 readers.
//!
//! The DSS7 reader (`dss7::pd`) and the DSS6 reader (`dss6::pd`) decode their own on-disk
//! layouts and map them into [`PairedData`]. Field meanings follow HEC's
//! `heclib_c/src/headers/zStructPairedData.h` (`zStructPairedData`), which both
//! `Public/zpdRetrieve.c:zpdRetrieve()` (DSS7) and
//! `DssInterface/v6and7/zpdRetrieve6.c:zpdRetrieve6()` (DSS6) fill. This file holds no
//! on-disk layout knowledge; keep changes additive because both readers depend on it.
//!
//! Values keep the stored width and bits ([`PdValues`]): type 200 (PD) stores `f32`, type
//! 205 (PDD) stores `f64`. Missing markers (−901.0, −902.0, −FLT_MAX) are not converted;
//! use [`crate::is_missing`] to map them. The `*_f64` accessors widen losslessly.

use crate::timeseries::TsValues;

/// Record type code of float paired data (`DATA_TYPE_PD`, `headers/zdataTypeDescriptions.h`).
pub const PD: i32 = 200;
/// Record type code of double paired data (`DATA_TYPE_PDD`).
pub const PDD: i32 = 205;

/// Stored value array of a paired-data record (the same type as time-series values).
pub type PdValues = TsValues;

/// Stored width of the ordinates and values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueWidth {
    /// `REAL*4` / `float` (type 200).
    F32,
    /// `REAL*8` / `double` (type 205).
    F64,
}

/// One decoded paired-data record: a set of ordinates (the independent variable, "rows")
/// and one or more curves (the dependent variable, "columns") over those ordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct PairedData {
    /// Pathname as stored.
    pub pathname: Vec<u8>,
    /// 200 (PD, float) or 205 (PDD, double).
    pub record_type: i32,
    /// `numberOrdinates`: rows.
    pub number_ordinates: usize,
    /// `numberCurves`: columns.
    pub number_curves: usize,
    /// Independent-variable values, `number_ordinates` long, stored width.
    pub ordinates: PdValues,
    /// Dependent-variable values, curve-major as stored: curve 0 (all ordinates), then
    /// curve 1, ... (`number_curves * number_ordinates` long).
    pub values: PdValues,
    /// Units of the ordinates (`unitsIndependent`), "" when not stored.
    pub units_independent: String,
    /// Type of the ordinates (`typeIndependent`, e.g. `Linear`, `Log`, `Probability`,
    /// `UNT`), "" when not stored.
    pub type_independent: String,
    /// Units of the curves (`unitsDependent`).
    pub units_dependent: String,
    /// Type of the curves (`typeDependent`).
    pub type_dependent: String,
    /// Curve labels as HEC returns them: at most one per curve, possibly fewer than
    /// `number_curves` (DSS7 stops at `labelsLength`), possibly empty strings. Empty when
    /// the record stores no labels.
    pub labels: Vec<String>,
    /// `boolIndependentIsXaxis` as stored (HEC writes 0 or 1; other values are kept).
    pub independent_is_x_axis: i32,
    /// `xprecision` / `yprecision`: decimal places, −1 when not set.
    pub x_precision: i32,
    pub y_precision: i32,
    /// Record time-zone name (DSS7 internal header; DSS6 stores none). Never applied.
    pub time_zone: Option<String>,
    /// User header words, opaque.
    pub user_header: Vec<i32>,
}

impl PairedData {
    /// Stored width of [`Self::ordinates`] and [`Self::values`].
    pub fn width(&self) -> ValueWidth {
        match self.values {
            TsValues::F32(_) => ValueWidth::F32,
            TsValues::F64(_) => ValueWidth::F64,
        }
    }

    /// Ordinates widened to `f64` (exact).
    pub fn ordinates_f64(&self) -> Vec<f64> {
        self.ordinates.to_f64()
    }

    /// All curve values widened to `f64`, curve-major.
    pub fn values_f64(&self) -> Vec<f64> {
        self.values.to_f64()
    }

    /// Curve `i` (0-based) widened to `f64`, or `None` when out of range.
    pub fn curve_f64(&self, i: usize) -> Option<Vec<f64>> {
        if i >= self.number_curves {
            return None;
        }
        let n = self.number_ordinates;
        let lo = i.checked_mul(n)?;
        let hi = lo.checked_add(n)?;
        Some(match &self.values {
            TsValues::F32(v) => v.get(lo..hi)?.iter().map(|&x| f64::from(x)).collect(),
            TsValues::F64(v) => v.get(lo..hi)?.to_vec(),
        })
    }

    /// Label of curve `i`, when stored.
    pub fn label(&self, i: usize) -> Option<&str> {
        self.labels.get(i).map(String::as_str)
    }

    /// Independent and dependent parameter names from the pathname C part (see
    /// [`pd_parameters`]).
    pub fn parameters(&self) -> (String, String) {
        pd_parameters(&self.pathname)
    }
}

/// Independent and dependent parameter names of a paired-data pathname.
///
/// HEC names paired data `<independent>-<dependent>` in the C part (for example
/// `STAGE-FLOW`, `FREQ-FLOW`, `ELEV-STOR`). The split follows
/// `heclib_c/src/Utilities/verticalDatum.c:pathnameIsElevPd()`: `strtok` on `-`, so empty
/// tokens are skipped; the first token is the independent parameter. HEC only looks at
/// the second token; here the dependent name is every remaining token joined with `-`.
/// Returns empty strings when the pathname has no C part.
pub fn pd_parameters(pathname: &[u8]) -> (String, String) {
    // C part = third part of /A/B/C/D/E/F/ (Public/zpathnameGetPart.c).
    let c = pathname
        .strip_prefix(b"/")
        .and_then(|rest| rest.split(|&b| b == b'/').nth(2))
        .unwrap_or(&[]);
    let mut tokens = c
        .split(|&b| b == b'-')
        .filter(|t| !t.is_empty())
        .map(|t| t.iter().map(|&b| b as char).collect::<String>());
    let independent = tokens.next().unwrap_or_default();
    let dependent = tokens.collect::<Vec<_>>().join("-");
    (independent, dependent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pd(values: PdValues, ords: PdValues, nc: usize, no: usize) -> PairedData {
        PairedData {
            pathname: b"/A/B/STAGE-FLOW///F/".to_vec(),
            record_type: PD,
            number_ordinates: no,
            number_curves: nc,
            ordinates: ords,
            values,
            units_independent: String::new(),
            type_independent: String::new(),
            units_dependent: String::new(),
            type_dependent: String::new(),
            labels: vec!["a".into()],
            independent_is_x_axis: 1,
            x_precision: -1,
            y_precision: -1,
            time_zone: None,
            user_header: Vec::new(),
        }
    }

    #[test]
    fn parameters_follow_strtok() {
        assert_eq!(
            pd_parameters(b"/A/B/STAGE-FLOW///F/"),
            ("STAGE".into(), "FLOW".into())
        );
        assert_eq!(
            pd_parameters(b"//B/--FREQ--FLOW-PEAK///"),
            ("FREQ".into(), "FLOW-PEAK".into())
        );
        assert_eq!(pd_parameters(b"/A/B/ELEV///F/"), ("ELEV".into(), "".into()));
        assert_eq!(pd_parameters(b"bad"), ("".into(), "".into()));
        assert_eq!(pd_parameters(b""), ("".into(), "".into()));
    }

    #[test]
    fn curves_and_widths() {
        let p = pd(
            TsValues::F32(vec![1.0, 2.0, 3.0, 4.0]),
            TsValues::F32(vec![0.5, 1.5]),
            2,
            2,
        );
        assert_eq!(p.width(), ValueWidth::F32);
        assert_eq!(p.curve_f64(1), Some(vec![3.0, 4.0]));
        assert_eq!(p.curve_f64(2), None);
        assert_eq!(p.label(0), Some("a"));
        assert_eq!(p.label(1), None);
        assert_eq!(p.ordinates_f64(), vec![0.5, 1.5]);
        // Inconsistent sizes never panic.
        let q = pd(TsValues::F64(vec![1.0]), TsValues::F64(vec![]), 3, 5);
        assert_eq!(q.curve_f64(0), None);
        assert_eq!(q.width(), ValueWidth::F64);
    }
}

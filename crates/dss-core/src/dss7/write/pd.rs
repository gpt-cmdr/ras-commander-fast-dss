//! Paired-data writer input, shared by the DSS7 writer
//! ([`super::write_pd`]) and the DSS6 writer (`crate::dss6::write::write_pd6`).
//!
//! A paired-data record is a set of ordinates (the independent variable, "rows") and one
//! or more curves (the dependent variable, "columns") over them. The record type follows
//! the arrays given: `f32` makes a PD (200) record and `f64` a PDD (205) record
//! (`Public/zpdStore.c:zpdStore()` storage flag 0, lines 688-725); values are never
//! converted to the other width.
//!
//! Labels are the curve names (`zStructPairedData.labels`); the X and Y *types*
//! (`typeIndependent`, `typeDependent`: `Linear`, `Log`, `Probability`, ...) are separate
//! fields. They are never filled from one another.

use super::WriteError;

/// Ordinates and curve values in the stored width.
#[derive(Debug, Clone, Copy)]
pub enum PdArrays<'a> {
    /// PD (200): `REAL*4` / `float` ordinates and values.
    F32 {
        ordinates: &'a [f32],
        /// Curve-major: curve 0 (one value per ordinate), then curve 1, ...
        values: &'a [f32],
    },
    /// PDD (205): `REAL*8` / `double` ordinates and values.
    F64 {
        ordinates: &'a [f64],
        values: &'a [f64],
    },
}

impl PdArrays<'_> {
    pub fn number_ordinates(&self) -> usize {
        match self {
            PdArrays::F32 { ordinates, .. } => ordinates.len(),
            PdArrays::F64 { ordinates, .. } => ordinates.len(),
        }
    }

    pub fn number_values(&self) -> usize {
        match self {
            PdArrays::F32 { values, .. } => values.len(),
            PdArrays::F64 { values, .. } => values.len(),
        }
    }

    pub fn is_double(&self) -> bool {
        matches!(self, PdArrays::F64 { .. })
    }

    /// Record type the arrays make: 200 (PD) or 205 (PDD).
    pub fn record_type(&self) -> i32 {
        if self.is_double() {
            crate::paireddata::PDD
        } else {
            crate::paireddata::PD
        }
    }

    /// The ordinates, then the values, as 32-bit words in the stored width (doubles low
    /// word first, as little-endian heclib stores them).
    pub(crate) fn words(&self) -> (Vec<i32>, Vec<i32>) {
        fn f(v: &[f32]) -> Vec<i32> {
            v.iter().map(|x| x.to_bits() as i32).collect()
        }
        fn d(v: &[f64]) -> Vec<i32> {
            v.iter()
                .flat_map(|x| {
                    let b = x.to_bits();
                    [b as u32 as i32, (b >> 32) as u32 as i32]
                })
                .collect()
        }
        match self {
            PdArrays::F32 { ordinates, values } => (f(ordinates), f(values)),
            PdArrays::F64 { ordinates, values } => (d(ordinates), d(values)),
        }
    }
}

/// One paired-data record to store (full record: HEC's `boolStoreEntire`).
#[derive(Debug, Clone)]
pub struct PdInput<'a> {
    /// `/A/B/C/D/E/F/` (exactly 7 slashes; C is usually `<independent>-<dependent>`,
    /// for example `STAGE-FLOW`). Refused, never cleaned or truncated.
    pub pathname: &'a [u8],
    pub data: PdArrays<'a>,
    /// `numberCurves`; the values hold `number_curves * number_ordinates` entries.
    pub number_curves: usize,
    /// `unitsIndependent` (X units).
    pub units_independent: &'a [u8],
    /// `typeIndependent` (X type: `Linear`, `Log`, `Probability`, ...).
    pub type_independent: &'a [u8],
    /// `unitsDependent` (Y units).
    pub units_dependent: &'a [u8],
    /// `typeDependent` (Y type).
    pub type_dependent: &'a [u8],
    /// One label per curve, or none. Labels are curve names, never the X/Y types.
    pub labels: Option<&'a [Vec<u8>]>,
    /// `boolIndependentIsXaxis`; HEC's default is 1 (`zstructPdNew()` line 61).
    pub independent_is_x_axis: i32,
    /// `xprecision`, `yprecision`: decimal places 0..=9, or both −1 (unset, HEC's
    /// default, `zstructPdNew()` lines 59-60).
    pub x_precision: i32,
    pub y_precision: i32,
    /// Record time-zone name (DSS7 only).
    pub time_zone: Option<&'a [u8]>,
}

impl<'a> PdInput<'a> {
    pub fn new(
        pathname: &'a [u8],
        data: PdArrays<'a>,
        number_curves: usize,
        units_independent: &'a [u8],
        type_independent: &'a [u8],
        units_dependent: &'a [u8],
        type_dependent: &'a [u8],
    ) -> Self {
        PdInput {
            pathname,
            data,
            number_curves,
            units_independent,
            type_independent,
            units_dependent,
            type_dependent,
            labels: None,
            independent_is_x_axis: 1,
            x_precision: -1,
            y_precision: -1,
            time_zone: None,
        }
    }

    /// The four unit and type strings with their names (for messages).
    pub(crate) fn strings(&self) -> [(&'static str, &'a [u8]); 4] {
        [
            ("units_independent", self.units_independent),
            ("type_independent", self.type_independent),
            ("units_dependent", self.units_dependent),
            ("type_dependent", self.type_dependent),
        ]
    }

    /// `xprecision * 10 + yprecision`, or −1 when unset (`zpdStore()` lines 743-748).
    pub(crate) fn precision_word(&self) -> i32 {
        if self.x_precision < 0 {
            -1
        } else {
            self.x_precision * 10 + self.y_precision
        }
    }
}

fn bad(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

/// Version-independent checks of a paired-data input: sizes, precision, axis flag and
/// label and string bytes. Version-specific limits are checked by each writer.
pub(crate) fn check_common(input: &PdInput) -> Result<(), WriteError> {
    let nord = input.data.number_ordinates();
    let ncurve = input.number_curves;
    // zpdStore() lines 749-754 (numberValues <= 0: NO_DATA_GIVEN); zspdi6.f lines 364-366.
    if nord == 0 {
        return Err(bad("no ordinates"));
    }
    if ncurve == 0 {
        return Err(bad("number_curves must be at least 1"));
    }
    let n = nord
        .checked_mul(ncurve)
        .ok_or_else(|| bad("number_curves * number_ordinates overflows"))?;
    if input.data.number_values() != n {
        return Err(bad(format!(
            "{} values for {ncurve} curves of {nord} ordinates (expected {n}, curve-major)",
            input.data.number_values()
        )));
    }
    // HEC counts values in 32-bit ints: (numberCurves + 1) * numberOrdinates * valueSize
    // (zpdStore() lines 795-815) must fit an int.
    let width = if input.data.is_double() { 2 } else { 1 };
    let words = (n as u128 + nord as u128) * width;
    if words > (i32::MAX as u128) / 2 {
        return Err(bad(format!(
            "{n} values are more than a paired-data record holds"
        )));
    }
    let (x, y) = (input.x_precision, input.y_precision);
    let ok = (x == -1 && y == -1) || ((0..=9).contains(&x) && (0..=9).contains(&y));
    if !ok {
        return Err(bad(format!(
            "x_precision and y_precision must both be -1 (unset) or both 0..=9 (got {x}, {y})"
        )));
    }
    if let Some(labels) = input.labels {
        if labels.len() != ncurve {
            return Err(bad(format!(
                "{} labels for {ncurve} curves (give one label per curve, or none)",
                labels.len()
            )));
        }
        for (i, l) in labels.iter().enumerate() {
            if let Some(&c) = l.iter().find(|&&c| !(32..=126).contains(&c)) {
                return Err(bad(format!(
                    "label {i} byte 0x{c:02x} is outside printable ASCII"
                )));
            }
        }
    }
    for (what, s) in input.strings() {
        if let Some(&c) = s.iter().find(|&&c| !(32..=126).contains(&c)) {
            return Err(bad(format!(
                "{what} byte 0x{c:02x} is outside printable ASCII"
            )));
        }
    }
    if let Some(tz) = input.time_zone {
        if let Some(&c) = tz.iter().find(|&&c| !(32..=126).contains(&c)) {
            return Err(bad(format!(
                "time_zone byte 0x{c:02x} is outside printable ASCII"
            )));
        }
    }
    Ok(())
}

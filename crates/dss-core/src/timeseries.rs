//! Version-neutral time-series model shared by the DSS6 and DSS7 readers.
//!
//! The DSS7 reader and the DSS6 reader decode their own on-disk
//! layouts and map them into these types. This file holds no format knowledge; keep
//! changes additive because both readers depend on it.
//!
//! Values keep the stored bits. HEC's missing markers (−901.0, −902.0, −FLT_MAX) are
//! not converted, so a raw read can be compared bit for bit with `hecdss`; use
//! [`is_missing`] (or the Python layer) to map them to NaN.

/// Stored value array. DSS keeps float and double records apart (types 100/110 vs
/// 105/115); the stored width is preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum TsValues {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl TsValues {
    pub fn len(&self) -> usize {
        match self {
            TsValues::F32(v) => v.len(),
            TsValues::F64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Values widened to f64, stored bits otherwise unchanged (missing markers kept).
    pub fn to_f64(&self) -> Vec<f64> {
        match self {
            TsValues::F32(v) => v.iter().map(|&x| f64::from(x)).collect(),
            TsValues::F64(v) => v.clone(),
        }
    }
}

/// Regular or irregular series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsKind {
    /// Regular interval in seconds (calendar intervals such as 1Month use their
    /// nominal length here; `times` carries the real stamps).
    Regular { interval_seconds: i64 },
    /// Irregular series; `block` is the E-part block span (IR-Day, IR-Month, ...).
    Irregular { block: IrregularBlock },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrregularBlock {
    Day,
    Month,
    Year,
    Decade,
    Century,
}

/// One decoded time-series record (a single block) or a stitched dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSeries {
    /// Pathname of the record read, as stored (for a stitched dataset: the first block).
    pub pathname: Vec<u8>,
    /// Record type code (100, 105, 110, 115, ...).
    pub record_type: i32,
    pub kind: TsKind,
    /// One stamp per value: whole seconds since 1899-12-31 00:00 (see `time`).
    /// HEC's 24:00 stamps are the same instant as the next day's 00:00.
    pub times: Vec<i64>,
    pub values: TsValues,
    /// Per-value quality flags when the record stores them.
    pub quality: Option<Vec<i32>>,
    pub units: String,
    /// HEC data type string (`INST-VAL`, `PER-AVER`, `PER-CUM`, ...).
    pub data_type: String,
    /// Time zone name or offset as stored; never applied.
    pub time_zone: Option<String>,
}

impl TimeSeries {
    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }
}

/// True for HEC's missing-value markers: −901.0 (missing), −902.0 (not computed), and
/// −FLT_MAX (UNDEFINED; `heclib_c/src/headers/zdssVals.h`), in either width, and NaN.
pub fn is_missing(v: f64) -> bool {
    v.is_nan() || v == -901.0 || v == -902.0 || v == -f64::from(f32::MAX)
}

/// HEC's own missing test, `zisMissingFloat` / `zisMissingDouble`
/// (`heclib_c/src/Utilities/zmissingFlag.c` lines 115-145, hec-dss fork tag `7-IU-15`):
/// −FLT_MAX (`zmissingFlagFloat` / `zmissingFlagDouble`, `UNDEFINED_FLOAT = -FLT_MAX`
/// and `UNDEFINED_DOUBLE = -(double)FLT_MAX`, `headers/heclib.h` lines 21-22), −901.0
/// and −902.0. NaN is **not** missing: every comparison with NaN is false, so HEC keeps
/// a stored NaN as a value. This is the test `Internal/ztsTrim.c` uses (lines 87-116)
/// and the one trims standing in for `ztsTrim` must use; [`is_missing`] (which also
/// matches NaN) stays the marker-to-NaN mapping.
///
/// `v` is the stored value widened to f64. For a float the widening is exact, so the
/// result equals `zisMissingFloat` on the float; for a double it equals
/// `zisMissingDouble` (whose flag is the float −FLT_MAX widened, not −DBL_MAX).
pub fn is_hec_missing(v: f64) -> bool {
    v == -901.0 || v == -902.0 || v == -f64::from(f32::MAX)
}

/// The time convention of pattern records: the first value of a regular pattern is
/// at 01JAN3000 00:00, then one value per interval. The stored offset is never applied.
/// This is the time array HEC Monolith returns (`HecDss.get(path, false)`).
pub const PATTERN_TIME_CONVENTION: &str = "first-value-at-3000-01-01T00:00";

/// Where a pattern record's stored time offset comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternOffsetSource {
    /// DSS6 internal header word 0, minutes. Unreliable: `heclib_f/src/zsrtpa6.f` writes
    /// the uninitialized local `IOFSET`, and `zrrtsi.f` line 183 resets it to 0 on read.
    Dss6InternalHeaderWord0Minutes,
    /// DSS7 `INT_HEAD_timeOffset` (seconds), written by `SemiPublic/ztsStorePattern.c`
    /// lines 137-140.
    Dss7IntHeadTimeOffset,
}

impl PatternOffsetSource {
    /// The name the Python layer reports as `pattern_stored_offset_source`.
    pub fn as_str(self) -> &'static str {
        match self {
            PatternOffsetSource::Dss6InternalHeaderWord0Minutes => {
                "dss6_internal_header_word0_minutes"
            }
            PatternOffsetSource::Dss7IntHeadTimeOffset => "dss7_int_head_time_offset",
        }
    }
}

/// Time-origin facts of a pattern record (types 101/106 regular, 111/116 irregular; D part
/// starting `TS-`). Reported only; the reader's times follow [`PATTERN_TIME_CONVENTION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternOrigin {
    /// Raw stored offset in seconds, not applied. `None` when the record has no such word.
    pub stored_offset_seconds: Option<i64>,
    pub stored_offset_source: PatternOffsetSource,
    /// HEC seconds (since 1899-12-31 00:00) of the first value as heclib C
    /// `Public/ztsRetrievePattern.c` reports it: 01Jan3000 00:00 plus one interval, moved
    /// to the stored offset when it is nonzero (lines 360-372). This is the start
    /// pydsstools and Monolith `startTime` show. `None` for irregular patterns, whose
    /// stored times are returned as they are.
    pub heclib_first_seconds: Option<i64>,
}

impl PatternOrigin {
    /// Always [`PATTERN_TIME_CONVENTION`].
    pub fn time_convention(&self) -> &'static str {
        PATTERN_TIME_CONVENTION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_markers() {
        assert!(is_missing(-901.0));
        assert!(is_missing(-902.0));
        assert!(is_missing(f64::from(-f32::MAX)));
        assert!(is_missing(f64::NAN));
        assert!(!is_missing(0.0));
        assert!(!is_missing(-900.0));
        assert!(!is_missing(-f64::MAX));
    }

    #[test]
    fn hec_missing_markers_keep_nan() {
        assert!(is_hec_missing(-901.0));
        assert!(is_hec_missing(-902.0));
        assert!(is_hec_missing(f64::from(-f32::MAX)));
        assert!(!is_hec_missing(f64::NAN));
        assert!(!is_hec_missing(f64::from(f32::NAN)));
        assert!(!is_hec_missing(0.0));
        assert!(!is_hec_missing(-900.0));
        assert!(!is_hec_missing(-f64::MAX));
    }
}

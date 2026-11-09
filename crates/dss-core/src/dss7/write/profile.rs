//! Regular-interval profile writer input.
//!
//! A profile is a time x depth matrix: one row of `depths.len()` values per time, stored
//! row by row (`Public/ztsStore.c` lines 154-176: `floatProfileValues[profileDepthsNumber]
//! [numberValues]`). The record type follows the arrays given: `f32` makes RTS-PROFILE
//! (102) and `f64` RTD-PROFILE (107) (`Internal/ztsStoreReg7.c:ztsStoreReg7()` lines
//! 183-206); values are never converted to the other width. DSS6 has no profiles
//! (`Public/ztsStore.c` lines 340-347), so there is no DSS6 writer.

/// Depths and values in the stored width.
#[derive(Debug, Clone, Copy)]
pub enum ProfileArrays<'a> {
    /// RTS-PROFILE (102).
    F32 {
        depths: &'a [f32],
        /// Row-major: `n_times * depths.len()` values.
        values: &'a [f32],
    },
    /// RTD-PROFILE (107).
    F64 {
        depths: &'a [f64],
        values: &'a [f64],
    },
}

impl ProfileArrays<'_> {
    pub fn number_depths(&self) -> usize {
        match self {
            ProfileArrays::F32 { depths, .. } => depths.len(),
            ProfileArrays::F64 { depths, .. } => depths.len(),
        }
    }

    pub fn number_values(&self) -> usize {
        match self {
            ProfileArrays::F32 { values, .. } => values.len(),
            ProfileArrays::F64 { values, .. } => values.len(),
        }
    }
}

/// One regular-interval profile series to store (HEC's storage flag 0).
#[derive(Debug, Clone)]
pub struct ProfileInput<'a> {
    /// `/A/B/C/D/E/F/`; D may be empty or a date (it is replaced per block).
    pub pathname: &'a [u8],
    /// Time of the first row: whole seconds since 1899-12-31 00:00, as
    /// [`super::RtsInput::start`].
    pub start: i64,
    pub data: ProfileArrays<'a>,
    /// One quality word per time, or none.
    pub quality: Option<&'a [i32]>,
    /// `unitsProfileDepths`.
    pub depth_units: &'a [u8],
    /// `unitsProfileValues`.
    pub value_units: &'a [u8],
    /// HEC data type (`INST-VAL`, ...).
    pub data_type: &'a [u8],
    pub time_zone: Option<&'a [u8]>,
    /// Decimal places (`INT_HEAD_precision`); -1 = unset.
    pub precision: i32,
}

impl<'a> ProfileInput<'a> {
    pub fn new(
        pathname: &'a [u8],
        start: i64,
        data: ProfileArrays<'a>,
        depth_units: &'a [u8],
        value_units: &'a [u8],
        data_type: &'a [u8],
    ) -> Self {
        ProfileInput {
            pathname,
            start,
            data,
            quality: None,
            depth_units,
            value_units,
            data_type,
            time_zone: None,
            precision: -1,
        }
    }
}

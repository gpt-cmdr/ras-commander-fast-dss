//! Location writer input, shared by the DSS7 writer
//! ([`super::write_location`]) and the DSS6 writer
//! (`crate::dss6::write::write_location6`). Format: `docs/format/dss7.md` §11.
//!
//! Field meanings: `Public/zlocationStore.c` comment block, `headers/zStructLocation.h`.
//! Codes are not validated, as heclib does not validate them.

use super::WriteError;

/// A location to store. A field left `None` keeps the stored value when
/// [`keep_existing`](Self::keep_existing) is true and there is one, and otherwise takes
/// HEC's unset value (0, no time zone, no supplemental text).
#[derive(Debug, Clone, Default)]
pub struct LocationInput<'a> {
    /// Any pathname of the location. DSS7: only A and B are used
    /// (`zlocationPath()`). DSS6: selects the time-series dataset.
    pub pathname: &'a [u8],
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub z: Option<f64>,
    pub coordinate_system: Option<i32>,
    pub coordinate_id: Option<i32>,
    pub horizontal_units: Option<i32>,
    pub horizontal_datum: Option<i32>,
    pub vertical_units: Option<i32>,
    pub vertical_datum: Option<i32>,
    /// DSS7 only. An empty name is not stored (`zlocationStore()` line 218).
    pub time_zone: Option<&'a [u8]>,
    /// DSS7: values2. DSS6: the record's user header. `Some(b"")` is stored text.
    pub supplemental: Option<&'a [u8]>,
    /// Keep what the record holds for fields left `None`, so a partial update loses
    /// nothing.
    pub keep_existing: bool,
}

/// Every location field after merging, as stored.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct LocationFields {
    pub xyz: [f64; 3],
    /// coordinateSystem, coordinateID, horizontalUnits, horizontalDatum, verticalUnits,
    /// verticalDatum.
    pub codes: [i32; 6],
    pub time_zone: Option<Vec<u8>>,
    pub supplemental: Option<Vec<u8>>,
}

impl LocationFields {
    /// The input's fields over `base`.
    pub(crate) fn merged(input: &LocationInput, base: LocationFields) -> LocationFields {
        let LocationFields {
            xyz,
            codes,
            time_zone,
            supplemental,
        } = base;
        LocationFields {
            xyz: [
                input.x.unwrap_or(xyz[0]),
                input.y.unwrap_or(xyz[1]),
                input.z.unwrap_or(xyz[2]),
            ],
            codes: [
                input.coordinate_system.unwrap_or(codes[0]),
                input.coordinate_id.unwrap_or(codes[1]),
                input.horizontal_units.unwrap_or(codes[2]),
                input.horizontal_datum.unwrap_or(codes[3]),
                input.vertical_units.unwrap_or(codes[4]),
                input.vertical_datum.unwrap_or(codes[5]),
            ],
            time_zone: input.time_zone.map(<[u8]>::to_vec).or(time_zone),
            supplemental: input.supplemental.map(<[u8]>::to_vec).or(supplemental),
        }
    }
}

/// Checks both versions share: C strings cannot hold a NUL (`strlen()` in
/// `zlocationStore()` lines 217 and 229; `stringToUserHeader()` line 340).
pub(crate) fn check_common(input: &LocationInput) -> Result<(), WriteError> {
    for (what, s) in [
        ("time_zone", input.time_zone),
        ("supplemental", input.supplemental),
    ] {
        if s.is_some_and(|b| b.contains(&0)) {
            return Err(WriteError::BadInput(format!(
                "{what} contains a NUL byte; heclib stores C strings"
            )));
        }
    }
    Ok(())
}

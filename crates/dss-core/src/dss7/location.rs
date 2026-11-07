//! DSS7 location records (type 20) and associated-location lookup (packet P15).
//!
//! Port of `heclib_c/src/Public/zlocationRetrieve.c:zlocationRetrieve()`; layout written
//! by `Public/zlocationStore.c:zlocationStore()` (dss7.md §11):
//!
//! | Item | Where | Source |
//! |---|---|---|
//! | x, y, z | values1 ints 0–5, three doubles | `zlocationRetrieve()` `convertDataType(..., 6, 6)` |
//! | coordinateSystem, coordinateID, horizontalUnits, horizontalDatum, verticalUnits, verticalDatum | values1 ints 6–11 (only when `values1Number >= 12`) | `zlocationRetrieve()` |
//! | time-zone name | values1 ints 12.. (`values1Number > 12`), bytes to the first NUL | `zlocationRetrieve()`, `zlocationStore()` (at most 30 ints in total) |
//! | supplemental text | values2, `values2Number * 4` bytes to the first NUL | `zlocationRetrieve()` |
//!
//! `values1Number < 6` is HEC's `STATUS_NOT_OKAY` (reported here as
//! [`LocationError::Corrupt`]). The associated location of any pathname is the record
//! `/A/B/Location Info////` ([`crate::location::location_pathname`]); HEC's `ztsRetrieve`
//! and `zpdRetrieve` call `zlocationRetrieve()` with the data pathname, and the data record
//! need not exist for the lookup.

use std::fmt;
use std::io::{Read, Seek};

use super::{Dss7Error, Dss7File, RecordRef};
use crate::location::{location_pathname, Location, LocationSource, LOCATION};

/// Largest values1/values2 area read for a location (int32 units). `zlocationStore()`
/// writes at most 30 values1 ints; supplemental text is free-length.
const MAX_LOCATION_INTS: i64 = 1 << 24;

/// Errors from the DSS7 location API.
#[derive(Debug)]
pub enum LocationError {
    Dss7(Dss7Error),
    /// No live record with this pathname.
    NotFound {
        pathname: Vec<u8>,
    },
    /// The pathname cannot form a location pathname (fewer than three `/`).
    BadPathname {
        pathname: Vec<u8>,
    },
    /// The record is not a location record (type 20).
    UnsupportedRecordType {
        pathname: Vec<u8>,
        record_type: i32,
    },
    /// Stored sizes are inconsistent (`values1Number < 6`, area outside the file, ...).
    Corrupt {
        pathname: Vec<u8>,
        info_address: i64,
        what: &'static str,
    },
}

impl fmt::Display for LocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = |b: &[u8]| -> String { b.iter().map(|&c| c as char).collect() };
        match self {
            LocationError::Dss7(e) => write!(f, "{e}"),
            LocationError::NotFound { pathname } => {
                write!(f, "no location record {}", p(pathname))
            }
            LocationError::BadPathname { pathname } => {
                write!(f, "pathname {} has no A and B parts", p(pathname))
            }
            LocationError::UnsupportedRecordType {
                pathname,
                record_type,
            } => write!(
                f,
                "record {} has type {record_type}, which is not a location record (20)",
                p(pathname)
            ),
            LocationError::Corrupt {
                pathname,
                info_address,
                what,
            } => write!(
                f,
                "location record {} (info at word {info_address}) is corrupt: {what}",
                p(pathname)
            ),
        }
    }
}

impl std::error::Error for LocationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LocationError::Dss7(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Dss7Error> for LocationError {
    fn from(e: Dss7Error) -> Self {
        LocationError::Dss7(e)
    }
}

/// A DSS7 location record with its raw areas.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss7Location {
    pub location: Location,
    /// values1 int32s as stored (at most 30 by `zlocationStore()`; all kept).
    pub values1: Vec<i32>,
    /// `kinfoValues2Number` as stored.
    pub values2_ints: i64,
    /// Program that wrote the record and its last-write time (ms since 1970).
    pub program: String,
    pub last_write_ms: i64,
}

/// Bytes of a NUL-terminated string in `b` (the whole slice when no NUL).
fn c_str(b: &[u8]) -> &[u8] {
    let e = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..e]
}

/// Decode `zlocationRetrieve()` from the record's values1 and values2 bytes.
/// `values1` holds `values1_number` ints. Errors name the inconsistency: fewer than 6
/// ints stored (HEC's `STATUS_NOT_OKAY`), or fewer bytes than the stored count.
pub(crate) fn decode(
    pathname: &[u8],
    values1: &[u8],
    values1_number: usize,
    values2: &[u8],
) -> Result<(Location, Vec<i32>), &'static str> {
    if values1_number < 6 {
        return Err("values1 holds fewer than 6 ints (HEC: STATUS_NOT_OKAY)");
    }
    if values1.len() < values1_number * 4 {
        return Err("values1 bytes shorter than its stored int count");
    }
    let ints: Vec<i32> = values1
        .chunks_exact(4)
        .take(values1_number)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let dbl = |i: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&values1[i * 8..i * 8 + 8]);
        f64::from_le_bytes(a)
    };
    let descriptors_stored = values1_number >= 12;
    let d = |i: usize| if descriptors_stored { ints[6 + i] } else { 0 };
    // Time zone: "if (values1Number > 12)", from int 12 (count after the descriptors),
    // nwords * 4 bytes; charInt(..., zeroEnd=1), then C string semantics.
    let time_zone_name = if values1_number > 12 {
        Some(
            c_str(&values1[12 * 4..values1_number * 4])
                .iter()
                .map(|&c| c as char)
                .collect(),
        )
    } else {
        None
    };
    let supplemental = if values2.is_empty() {
        None
    } else {
        Some(c_str(values2).to_vec())
    };
    let loc = Location {
        pathname: pathname.to_vec(),
        source: LocationSource::Dss7Record,
        x: dbl(0),
        y: dbl(1),
        z: dbl(2),
        descriptors_stored,
        coordinate_system_code: d(0),
        coordinate_id: d(1),
        horizontal_units_code: d(2),
        horizontal_datum_code: d(3),
        vertical_units_code: d(4),
        vertical_datum_code: d(5),
        time_zone_name,
        supplemental,
    };
    Ok((loc, ints))
}

impl<R: Read + Seek> Dss7File<R> {
    fn loc_corrupt(rec: &RecordRef, what: &'static str) -> LocationError {
        LocationError::Corrupt {
            pathname: rec.pathname.clone(),
            info_address: rec.info_address,
            what,
        }
    }

    fn loc_area(
        &mut self,
        rec: &RecordRef,
        address: i64,
        n: i64,
    ) -> Result<Vec<u8>, LocationError> {
        if !(0..=MAX_LOCATION_INTS).contains(&n) {
            return Err(Self::loc_corrupt(rec, "data area length out of range"));
        }
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut b = self.read_words(address, (n + 1) / 2).map_err(|e| match e {
            Dss7Error::ReadOutOfBounds { .. } => {
                Self::loc_corrupt(rec, "data area outside the file")
            }
            other => LocationError::Dss7(other),
        })?;
        b.truncate(n as usize * 4);
        Ok(b)
    }

    /// Read the location record (type 20) stored under `pathname`.
    pub fn read_location(&mut self, pathname: &[u8]) -> Result<Dss7Location, LocationError> {
        let rec = self
            .lookup(pathname)?
            .ok_or_else(|| LocationError::NotFound {
                pathname: pathname.to_vec(),
            })?;
        self.read_location_record(&rec)
    }

    /// Read a location record found by [`Dss7File::lookup`] or [`Dss7File::record_at`].
    pub fn read_location_record(&mut self, rec: &RecordRef) -> Result<Dss7Location, LocationError> {
        let info = rec.info.clone();
        if info.record_type != LOCATION {
            return Err(LocationError::UnsupportedRecordType {
                pathname: rec.pathname.clone(),
                record_type: info.record_type,
            });
        }
        let n1 = info.values1.number_ints;
        let v1 = self.loc_area(rec, info.values1.address, n1)?;
        let v2 = self.loc_area(rec, info.values2.address, info.values2.number_ints)?;
        let (location, values1) = decode(&rec.pathname, &v1, n1 as usize, &v2)
            .map_err(|what| Self::loc_corrupt(rec, what))?;
        Ok(Dss7Location {
            location,
            values1,
            values2_ints: info.values2.number_ints,
            program: info.program.clone(),
            last_write_ms: info.last_write_ms,
        })
    }

    /// The location associated with any pathname: the record `/A/B/Location Info////`
    /// (`zlocationPath()`), as `ztsRetrieve`/`zpdRetrieve` read it. `Ok(None)` when no
    /// such record exists. The data record itself is not looked up.
    pub fn read_associated_location(
        &mut self,
        pathname: &[u8],
    ) -> Result<Option<Dss7Location>, LocationError> {
        let lp = location_pathname(pathname).ok_or_else(|| LocationError::BadPathname {
            pathname: pathname.to_vec(),
        })?;
        match self.lookup(&lp)? {
            Some(rec) => self.read_location_record(&rec).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values1(xyz: [f64; 3], desc: &[i32], tz: &[u8]) -> (Vec<u8>, usize) {
        let mut b = Vec::new();
        for v in xyz {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for d in desc {
            b.extend_from_slice(&d.to_le_bytes());
        }
        b.extend_from_slice(tz);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        let n = b.len() / 4;
        (b, n)
    }

    #[test]
    fn full_record_decodes() {
        let (v1, n) = values1(
            [-97.5, 30.25, 610.5],
            &[1, 0, 3, 1, 1, 0],
            b"America/Chicago",
        );
        let v2 = b"VerticalDatum:NGVD29;Gage:1;\0\0\0\0".to_vec();
        let (l, ints) = decode(b"/A/B/Location Info////", &v1, n, &v2).unwrap();
        assert_eq!(ints.len(), n);
        assert_eq!((l.x, l.y, l.z), (-97.5, 30.25, 610.5));
        assert!(l.descriptors_stored);
        assert_eq!(l.coordinate_system_code, 1);
        assert_eq!(l.horizontal_units_code, 3);
        assert_eq!(l.vertical_datum_code, 0);
        assert!(l.vertical_datum().is_unknown());
        assert_eq!(l.time_zone_name.as_deref(), Some("America/Chicago"));
        assert_eq!(
            l.supplemental_text().as_deref(),
            Some("VerticalDatum:NGVD29;Gage:1;")
        );
        assert_eq!(l.supplemental_vertical_datum().as_deref(), Some("NGVD29"));
    }

    #[test]
    fn short_records_follow_hec() {
        // Coordinates only: descriptors stay 0, no time zone.
        let (v1, n) = values1([1.0, 2.0, 3.0], &[], b"");
        let (l, _) = decode(b"/A/B/Location Info////", &v1, n, &[]).unwrap();
        assert!(!l.descriptors_stored);
        assert_eq!(l.vertical_units_code, 0);
        assert!(l.time_zone_name.is_none() && l.supplemental.is_none());
        // Exactly 12 ints: no time zone.
        let (v1, n) = values1([1.0, 2.0, 3.0], &[2, 15, 1, 2, 2, 1], b"");
        let (l, _) = decode(b"/A/B/Location Info////", &v1, n, &[]).unwrap();
        assert!(l.time_zone_name.is_none());
        assert_eq!(l.vertical_datum_code, 1);
        // Fewer than 6 ints: STATUS_NOT_OKAY.
        assert!(decode(b"/A/B/Location Info////", &v1[..20], 5, &[]).is_err());
        // Declared count longer than the bytes: no panic, refused.
        assert_eq!(
            decode(b"/A/B/Location Info////", &v1[..24], 12, &[]),
            Err("values1 bytes shorter than its stored int count")
        );
        // Supplemental without NUL keeps all stored bytes.
        let (l, _) = decode(b"/A/B/Location Info////", &v1, n, b"abcd").unwrap();
        assert_eq!(l.supplemental.as_deref(), Some(&b"abcd"[..]));
    }
}

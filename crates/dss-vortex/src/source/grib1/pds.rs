//! Section 1, the product definition: what a record is and when it applies.
//!
//! Sources, NetCDF-Java 5.5.3: `Grib1SectionProductDefinition` (octets, `getReferenceDate`,
//! `getDecimalScale`), `Grib1ParamTime` (time range indicators), `GribNumbers.int2`. WMO FM 92
//! GRIB edition 1, section 1 and code tables 4 and 5.

use super::scan::decode_error;
use crate::error::VortexError;
use dss_core::time::ymd_to_julian;

/// Seconds from the epoch of `dss_core::time` julian day 0.
pub type Seconds = i64;

/// `GribNumbers.UNDEFINED`, which `int2` returns for all ones.
pub(super) const UNDEFINED: i32 = -9999;

/// `GribNumbers.int2`: sign and magnitude, all ones undefined.
pub(super) fn int2(a: u8, b: u8) -> i32 {
    if a == 0xff && b == 0xff {
        return UNDEFINED;
    }
    let a = i32::from(a);
    (1 - ((a & 128) >> 6)) * ((a & 127) << 8 | i32::from(b))
}

/// What section 1 says about a record, as the naming and time rules need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductDefinition {
    /// Octet 4: the parameter table version.
    pub table_version: u8,
    /// Octet 5.
    pub center: u8,
    /// Octet 26.
    pub subcenter: u8,
    /// Octet 6: the generating process.
    pub process: u8,
    /// Octet 7: the grid id (255 when the GDS defines it).
    pub grid_id: u8,
    /// Octet 9: code table 2.
    pub parameter: u8,
    /// Octet 10: code table 3.
    pub level_type: u8,
    /// Octets 11-12, verbatim.
    pub level: [u8; 2],
    /// Octets 13-17 and 25: the reference time.
    pub reference: Seconds,
    /// Octet 18: code table 4.
    pub time_unit: u8,
    pub p1: u8,
    pub p2: u8,
    /// Octet 21: code table 5.
    pub time_range: u8,
    /// Octets 22-23 and 24.
    pub included: u16,
    pub missing_from_statistic: u8,
    /// Octets 27-28, sign and magnitude; [`UNDEFINED`] for all ones.
    pub decimal_scale: i32,
}

/// `Grib1SectionProductDefinition.getReferenceDate`, then `CalendarDate.of`, which throws on a
/// date the calendar does not hold; that is a decode error here.
fn reference(section: &[u8]) -> Result<Seconds, VortexError> {
    // getReferenceCentury() - 1, with 0 read as the 20th century.
    let mut century = i64::from(section[24]) - 1;
    if century == -1 {
        century = 20;
    }
    let year = century * 100 + i64::from(section[12]);
    let month = u32::from(section[13]);
    let day = u32::from(section[14]);
    let hour = i64::from(section[15]);
    let minute = i64::from(section[16]);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(decode_error(format!("reference month {month}"))),
    };
    if !(1..=last).contains(&day) || year < 1 || hour > 23 || minute > 59 {
        return Err(decode_error(format!(
            "reference date {year}-{month}-{day} {hour}:{minute}"
        )));
    }
    Ok(ymd_to_julian(year, month, day) * 86_400 + hour * 3600 + minute * 60)
}

/// Read section 1. Its length was checked by the scan to be at least 28 octets.
pub fn read(section: &[u8]) -> Result<ProductDefinition, VortexError> {
    if section.len() < 28 {
        return Err(decode_error(format!(
            "PDS of {} octets, under 28",
            section.len()
        )));
    }
    let o = |n: usize| section[n - 1];
    Ok(ProductDefinition {
        table_version: o(4),
        center: o(5),
        process: o(6),
        grid_id: o(7),
        parameter: o(9),
        level_type: o(10),
        level: [o(11), o(12)],
        reference: reference(section)?,
        time_unit: o(18),
        p1: o(19),
        p2: o(20),
        time_range: o(21),
        included: u16::from_be_bytes([o(22), o(23)]),
        missing_from_statistic: o(24),
        subcenter: o(26),
        decimal_scale: int2(o(27), o(28)),
    })
}

/// Seconds in a code table 4 unit, for the units a sampled product uses. `Grib1ParamTime` and
/// `GribUtils.getCalendarPeriod` take the others too; with no sample there is no reference for
/// their names, so they are refused.
pub(super) fn unit_seconds(unit: u8) -> Result<i64, VortexError> {
    match unit {
        1 => Ok(3600),
        other => Err(VortexError::Refused {
            what: "GRIB1 time".to_string(),
            reason: format!("time unit {other} (code table 4); Stage IV uses 1, hour"),
        }),
    }
}

/// The record's interval, `Grib1ParamTime` case 4: an accumulation from reference + P1 to
/// reference + P2. Other time range indicators are refused (see the module of `grib1`).
pub(super) fn accumulation(pd: &ProductDefinition) -> Result<(Seconds, Seconds), VortexError> {
    if pd.time_range != 4 {
        return Err(VortexError::Refused {
            what: "GRIB1 time".to_string(),
            reason: format!(
                "time range indicator {} (code table 5); this reader takes 4, accumulation, the one \
                 Stage IV uses",
                pd.time_range
            ),
        });
    }
    let unit = unit_seconds(pd.time_unit)?;
    if pd.p2 <= pd.p1 {
        return Err(VortexError::Refused {
            what: "GRIB1 time".to_string(),
            reason: format!(
                "accumulation from P1 = {} to P2 = {} has no length",
                pd.p1, pd.p2
            ),
        });
    }
    Ok((
        pd.reference + i64::from(pd.p1) * unit,
        pd.reference + i64::from(pd.p2) * unit,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int2_is_sign_and_magnitude() {
        assert_eq!(int2(0, 1), 1);
        assert_eq!(int2(0x80, 2), -2);
        assert_eq!(int2(0xff, 0xff), UNDEFINED);
    }
}

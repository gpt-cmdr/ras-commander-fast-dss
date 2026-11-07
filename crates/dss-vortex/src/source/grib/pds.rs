//! Sections 1 and 4: what a record is and when it applies.
//!
//! Sources, NetCDF-Java 5.5.3: `Grib2Pds` (templates 0, 1, 8, 11), `Grib2Tables.getForecastTimeInterval`
//! and `getForecastTime`, `Grib2SectionIdentification`.

use super::numbers::{int4, UNDEFINED};
use crate::error::VortexError;
use dss_core::time::ymd_to_julian;

/// A UTC time, seconds from the epoch of `dss_core::time` julian day 0.
pub type Seconds = i64;

fn decode_error(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: "GRIB2",
        message: message.into(),
    }
}

fn octet(section: &[u8], n: usize) -> Result<u8, VortexError> {
    section
        .get(n - 1)
        .copied()
        .ok_or_else(|| decode_error(format!("section ends before octet {n}")))
}

fn int4_at(section: &[u8], n: usize) -> Result<i32, VortexError> {
    Ok(int4([
        octet(section, n)?,
        octet(section, n + 1)?,
        octet(section, n + 2)?,
        octet(section, n + 3)?,
    ]))
}

/// `CalendarDate.of`, for a date the calendar accepts.
fn date(section: &[u8], n: usize) -> Result<Option<Seconds>, VortexError> {
    let year = (i32::from(octet(section, n)?) << 8) | i32::from(octet(section, n + 1)?);
    let month = u32::from(octet(section, n + 2)?);
    let mut day = u32::from(octet(section, n + 3)?);
    let mut hour = i64::from(octet(section, n + 4)?);
    let minute = i64::from(octet(section, n + 5)?);
    let second = i64::from(octet(section, n + 6)?);
    if year == 0 && month == 0 && day == 0 && hour == 0 && minute == 0 && second == 0 {
        return Ok(None);
    }
    // Grib2Pds.calcTime: "href.t00z.prob.f36.grib2".
    if hour > 23 {
        day += (hour / 24) as u32;
        hour %= 24;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(decode_error(format!("month {month}"))),
    };
    if !(1..=last).contains(&day) || year < 1 || minute > 59 || second > 59 {
        return Err(decode_error(format!(
            "date {year}-{month}-{day} {hour}:{minute}:{second}"
        )));
    }
    let julian = ymd_to_julian(i64::from(year), month, day);
    Ok(Some(julian * 86_400 + hour * 3600 + minute * 60 + second))
}

/// Seconds in a GRIB time unit (`Grib2Utils.getCalendarPeriod`). Months and years have no fixed
/// length, and no sampled product uses them, so they are refused.
fn unit_seconds(unit: u8) -> Result<i64, VortexError> {
    Ok(match unit {
        0 => 60,
        1 => 3600,
        2 => 86_400,
        10 => 3 * 3600,
        11 => 6 * 3600,
        12 => 12 * 3600,
        13 => 1,
        other => {
            return Err(VortexError::Refused {
                what: "GRIB2 time".to_string(),
                reason: format!("time unit {other}"),
            })
        }
    })
}

/// One time range specification (`Grib2Pds.readTimeIntervals`).
fn interval(section: &[u8], first_range: usize, count: u8, unit: u8) -> Result<Stat, VortexError> {
    let mut range: i64 = 0;
    let mut range_unit: Option<u8> = None;
    let mut statistic = None;
    for i in 0..usize::from(count) {
        let base = first_range + i * 12;
        let stat = octet(section, base)?;
        let this_unit = octet(section, base + 2)?;
        let length = int4_at(section, base + 3)?;
        let increment_unit = octet(section, base + 7)?;
        let increment = int4_at(section, base + 8)?;
        // Grib2Pds.getStatType reads the first range only. Names carry that one, so a record that
        // goes on to another statistic would read as the first. No sampled product has that.
        if i == 0 {
            statistic = Some(stat);
        } else if statistic != Some(stat) {
            return Err(VortexError::Refused {
                what: "GRIB2 time".to_string(),
                reason: "time ranges with different statistical processes".to_string(),
            });
        }
        if this_unit == 255 {
            continue;
        }
        match range_unit {
            None => range_unit = Some(this_unit),
            Some(first) => {
                if this_unit != first
                    || (increment_unit != first && increment_unit != 255 && increment != 0)
                {
                    return Err(VortexError::Refused {
                        what: "GRIB2 time".to_string(),
                        reason: "time ranges with different units".to_string(),
                    });
                }
            }
        }
        range += i64::from(length);
        if increment_unit != 255 {
            range += i64::from(increment);
        }
    }
    let range_unit = range_unit.ok_or_else(|| decode_error("no time range"))?;
    // getForecastTimeInterval: the range in the PDS unit, truncated to int (`(int) fac2`).
    let want = unit_seconds(unit)?;
    let have = unit_seconds(range_unit)?;
    let whole_units = (range.saturating_mul(have)) / want;
    Ok(Stat {
        statistic: statistic.ok_or_else(|| decode_error("no time range"))?,
        length_seconds: whole_units.saturating_mul(want),
    })
}

struct Stat {
    statistic: u8,
    length_seconds: i64,
}

/// What section 4 says about a record, as the naming and time rules need it.
#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    /// Section 0 octet 7.
    pub discipline: u8,
    pub center: u16,
    pub subcenter: u16,
    pub template: u16,
    pub category: u8,
    pub number: u8,
    pub generating_type: u8,
    pub level_type1: u8,
    pub level_type2: u8,
    /// Octets 24-28, verbatim: the identity of the level value.
    pub level1: [u8; 5],
    pub level2: [u8; 5],
    /// Set for an ensemble member template.
    pub ensemble_member: Option<u8>,
    /// Code table 4.10, for statistically processed templates.
    pub statistic: Option<u8>,
    /// Inclusive start and end, seconds. Equal for an instant.
    pub start: Seconds,
    pub end: Seconds,
    /// `Grib2Pds.getTimeUnit`, with the interval's unit in seconds, for the interval's name.
    pub time_unit_seconds: i64,
}

impl Product {
    pub fn is_layer(&self) -> bool {
        // Grib2Tables.isLayer.
        !(self.level_type2 == 255 || self.level_type2 == 0)
    }
}

/// Read sections 1 and 4. Templates with no sampled product are refused.
pub fn read_product(
    discipline: u8,
    section1: &[u8],
    section4: &[u8],
) -> Result<Product, VortexError> {
    let center = u16::from_be_bytes([octet(section1, 6)?, octet(section1, 7)?]);
    let subcenter = u16::from_be_bytes([octet(section1, 8)?, octet(section1, 9)?]);
    let reference = date(section1, 13)?.ok_or_else(|| decode_error("no reference time"))?;

    let template = u16::from_be_bytes([octet(section4, 8)?, octet(section4, 9)?]);
    let (ensemble, end_octet) = match template {
        0 => (false, None),
        1 => (true, None),
        8 => (false, Some(35)),
        11 => (true, Some(38)),
        other => {
            return Err(VortexError::Refused {
                what: "GRIB2 product".to_string(),
                reason: format!("product template 4.{other} has no sampled product"),
            })
        }
    };
    let category = octet(section4, 10)?;
    let number = octet(section4, 11)?;
    let generating_type = octet(section4, 12)?;
    let unit = octet(section4, 18)?;
    let forecast = int4_at(section4, 19)?;
    let level_type1 = octet(section4, 23)?;
    let level_type2 = octet(section4, 29)?;
    let mut level1 = [0; 5];
    let mut level2 = [0; 5];
    for i in 0..5 {
        level1[i] = octet(section4, 24 + i)?;
        level2[i] = octet(section4, 30 + i)?;
    }
    let unit_length = unit_seconds(unit)?;
    let ensemble_member = if ensemble {
        Some(octet(section4, 37)?)
    } else {
        None
    };
    let (start, end, statistic) = match end_octet {
        None => {
            if forecast == UNDEFINED {
                return Err(decode_error("no forecast time"));
            }
            // Grib2Tables.getForecastDate: reference + forecast * unit.
            let t = reference.saturating_add(i64::from(forecast).saturating_mul(unit_length));
            (t, t, None)
        }
        Some(at) => {
            let count = octet(section4, at + 7)?;
            let stat = interval(section4, at + 12, count, unit)?;
            // TimeCoordIntvDateValue(period, EI), or (reference, period) when EI is all zero.
            match date(section4, at)? {
                Some(end) => (
                    end.saturating_sub(stat.length_seconds),
                    end,
                    Some(stat.statistic),
                ),
                None => (
                    reference,
                    reference.saturating_add(stat.length_seconds),
                    Some(stat.statistic),
                ),
            }
        }
    };
    Ok(Product {
        discipline,
        center,
        subcenter,
        template,
        category,
        number,
        generating_type,
        level_type1,
        level_type2,
        level1,
        level2,
        ensemble_member,
        statistic,
        start,
        end,
        time_unit_seconds: unit_length,
    })
}

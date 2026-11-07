//! HEC time base shared by the DSS6 and DSS7 readers.
//!
//! HEC julian day 1 is 1900-01-01 (`heclib_c/src/Utilities/julianToYearMonthDay.c`;
//! day 0 is 1899-12-31). Times in this crate are whole seconds since 1899-12-31 00:00,
//! so `julian * 86_400 + seconds_of_day`. HEC's 24:00 convention (day `j`, 86400 s) is the
//! same instant as day `j + 1`, 0 s; [`hec_seconds`] accepts it and [`split`] can produce
//! it on request. No time zone is ever applied.

/// Seconds per day.
pub const DAY: i64 = 86_400;

/// Seconds since 1899-12-31 00:00 for a HEC julian day and seconds past midnight.
/// `seconds` may be 86400 (24:00 of `julian`).
pub fn hec_seconds(julian: i64, seconds: i64) -> i64 {
    julian * DAY + seconds
}

/// Split seconds since the HEC epoch into (julian day, seconds of day).
/// With `end_of_day`, midnight is returned as 24:00 of the previous day, as HEC
/// displays end-of-period stamps.
pub fn split(t: i64, end_of_day: bool) -> (i64, i64) {
    let (mut j, mut s) = (t.div_euclid(DAY), t.rem_euclid(DAY));
    if end_of_day && s == 0 {
        j -= 1;
        s = DAY;
    }
    (j, s)
}

/// Civil date (year, month, day) for a HEC julian day (proleptic Gregorian).
pub fn julian_to_ymd(julian: i64) -> (i64, u32, u32) {
    // Days since 1970-01-01: HEC julian 25568 is 1970-01-01.
    civil_from_days(julian - 25_568)
}

/// HEC julian day for a civil date (proleptic Gregorian).
pub fn ymd_to_julian(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, month, day) + 25_568
}

// Howard Hinnant's civil-date algorithms (public domain), on days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(julian_to_ymd(1), (1900, 1, 1));
        assert_eq!(julian_to_ymd(0), (1899, 12, 31));
        assert_eq!(ymd_to_julian(1970, 1, 1), 25_568);
        assert_eq!(ymd_to_julian(2000, 1, 1), 36_525);
        assert_eq!(julian_to_ymd(ymd_to_julian(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(julian_to_ymd(-1), (1899, 12, 30));
    }

    #[test]
    fn twenty_four_hundred_round_trips() {
        let t = hec_seconds(100, DAY);
        assert_eq!(t, hec_seconds(101, 0));
        assert_eq!(split(t, true), (100, DAY));
        assert_eq!(split(t, false), (101, 0));
        assert_eq!(split(hec_seconds(5, 3600), true), (5, 3600));
    }
}

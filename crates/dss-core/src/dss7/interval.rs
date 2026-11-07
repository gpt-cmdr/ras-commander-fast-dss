//! HEC calendar, interval and block arithmetic used by DSS7 time-series reads
//! (`docs/format/dss7.md` §7.1–§7.3).
//!
//! Each function is a port of the `heclib` C function named in its comment. HEC works in
//! C `int` (julian day, seconds of day); these ports use `i64` so hostile input can never
//! overflow or panic, and keep HEC's conventions:
//!
//! - julian day 1 = 1900-01-01 (`headers/heclibDate.h`);
//! - a time is `(julian, seconds)` with seconds in `1..=86400`: midnight is 24:00 of the
//!   previous day (`Utilities/DateTime/cleanTime.c:cleanTime()`);
//! - calendar intervals (Tri-Month, Semi-Month, 1Month, 1Year) carry nominal second
//!   counts (`headers/standardIntervals.h`) and are stepped on the calendar.

/// `UNDEFINED_TIME` (`headers/heclibDate.h`).
pub const UNDEFINED_TIME: i64 = -2_147_483_647;

// `enum INTERVAL_SECONDS`, `headers/standardIntervals.h`.
pub const SECS_IN_1_YEAR: i64 = 31_536_000;
pub const SECS_IN_1_MONTH: i64 = 2_592_000;
pub const SECS_IN_SEMI_MONTH: i64 = 1_296_000;
pub const SECS_IN_TRI_MONTH: i64 = 864_000;
pub const SECS_IN_1_WEEK: i64 = 604_800;
pub const SECS_IN_1_DAY: i64 = 86_400;
pub const SECS_IN_6_HOURS: i64 = 21_600;
pub const SECS_IN_4_HOURS: i64 = 14_400;
pub const SECS_IN_30_MINUTES: i64 = 1_800;
pub const SECS_IN_15_MINUTES: i64 = 900;
pub const SECS_IN_1_MINUTE: i64 = 60;
// `enum INTERVAL_MINUTES`.
const MINS_IN_1_YEAR: i64 = 525_600;
const MINS_IN_1_MONTH: i64 = 43_200;
const MINS_IN_SEMI_MONTH: i64 = 21_600;
const MINS_IN_TRI_MONTH: i64 = 14_400;
const MINS_IN_1_DAY: i64 = 1_440;

/// Time granularities (`headers/hecdssInternal.h`: `SECOND_GRANULARITY` … `DAY_GRANULARITY`).
pub const SECOND_GRANULARITY: i64 = 1;
pub const MINUTE_GRANULARITY: i64 = 60;
pub const HOUR_GRANULARITY: i64 = 3_600;
pub const DAY_GRANULARITY: i64 = 86_400;

/// Block sizes (`enum BLOCK_SIZE`, `headers/standardIntervals.h`).
pub const BLOCK_1_DAY: i64 = 1;
pub const BLOCK_1_MONTH: i64 = 2;
pub const BLOCK_1_YEAR: i64 = 3;
pub const BLOCK_1_DECADE: i64 = 4;
pub const BLOCK_1_CENTURY: i64 = 5;

/// Julians outside this range are rejected before calendar arithmetic (about ±270,000
/// years); HEC's own `int` arithmetic is meaningless far before that.
pub const JULIAN_LIMIT: i64 = 100_000_000;

/// DSS7 E-part names and interval codes, in table order (`eParts7` /
/// `secondsInInterval7`, `headers/standardIntervals.h`). Irregular codes are the negative
/// block size.
pub const EPARTS7: [(&str, i64); 39] = [
    ("1Year", SECS_IN_1_YEAR),
    ("1Month", SECS_IN_1_MONTH),
    ("Semi-Month", SECS_IN_SEMI_MONTH),
    ("Tri-Month", SECS_IN_TRI_MONTH),
    ("1Week", SECS_IN_1_WEEK),
    ("1Day", SECS_IN_1_DAY),
    ("12Hour", 43_200),
    ("8Hour", 28_800),
    ("6Hour", 21_600),
    ("4Hour", 14_400),
    ("3Hour", 10_800),
    ("2Hour", 7_200),
    ("1Hour", 3_600),
    ("30Minute", 1_800),
    ("20Minute", 1_200),
    ("15Minute", 900),
    ("12Minute", 720),
    ("10Minute", 600),
    ("6Minute", 360),
    ("5Minute", 300),
    ("4Minute", 240),
    ("3Minute", 180),
    ("2Minute", 120),
    ("1Minute", 60),
    ("30Second", 30),
    ("20Second", 20),
    ("15Second", 15),
    ("10Second", 10),
    ("6Second", 6),
    ("5Second", 5),
    ("4Second", 4),
    ("3Second", 3),
    ("2Second", 2),
    ("1Second", 1),
    ("IR-Century", -BLOCK_1_CENTURY),
    ("IR-Decade", -BLOCK_1_DECADE),
    ("IR-Year", -BLOCK_1_YEAR),
    ("IR-Month", -BLOCK_1_MONTH),
    ("IR-Day", -BLOCK_1_DAY),
];

/// A DSS7 E part resolved by [`standard_interval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInterval {
    /// Seconds (regular, > 0) or the negative irregular block size (−1 IR-Day … −5
    /// IR-Century). Pseudo-regular (`~`) E parts map to an irregular block.
    pub code: i64,
    /// Canonical DSS7 name (`1Hour`, `IR-Month`, ...), without any `~`.
    pub name: &'static str,
    /// The E part started with `~` (pseudo-regular, stored as irregular).
    pub pseudo_regular: bool,
}

impl StandardInterval {
    pub fn is_regular(&self) -> bool {
        self.code > 0
    }
}

/// `SemiPublic/ztsGetStandardInterval.c:ztsGetStandardInterval()` for DSS version 7 with
/// `EPART_TO_SECONDS`: the first table entry whose first `min(len(name), len(E))` bytes
/// equal the E part, case-insensitively (`zstringCompare`). A leading `~` selects the
/// pseudo-regular block for the named interval.
pub fn standard_interval(epart: &[u8]) -> Option<StandardInterval> {
    let (pseudo, e) = match epart.first() {
        Some(b'~') => (true, &epart[1..]),
        _ => (false, epart),
    };
    if e.is_empty() {
        return None;
    }
    for &(name, secs) in EPARTS7.iter() {
        let n = name.len().min(e.len());
        if name.as_bytes()[..n].eq_ignore_ascii_case(&e[..n]) {
            let code = if pseudo {
                // DSS7 branch of the pseudo-regular block table.
                if secs < SECS_IN_30_MINUTES {
                    -BLOCK_1_DAY
                } else if secs < SECS_IN_6_HOURS {
                    -BLOCK_1_MONTH
                } else if secs < SECS_IN_1_WEEK {
                    -BLOCK_1_YEAR
                } else if secs < SECS_IN_1_MONTH {
                    -BLOCK_1_DECADE
                } else {
                    -BLOCK_1_CENTURY
                }
            } else {
                secs
            };
            return Some(StandardInterval {
                code,
                name,
                pseudo_regular: pseudo,
            });
        }
    }
    None
}

/// `Utilities/DateTime/isLeapYear.c:isLeapYear()` (Gregorian rule).
pub fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// `Utilities/DateTime/yearMonthDayToJulian.c:yearMonthDayToJulian()`, including its
/// month normalization (month 13 is January of the next year, month 0 is December of the
/// previous year) and its day arithmetic (day 0 is the last day of the previous month).
pub fn ymd_to_julian(year: i64, month: i64, day: i64) -> i64 {
    let (mut y, mut m) = (year, month);
    if !(1..=12).contains(&m) {
        let iy = m / 12; // C truncation
        m -= iy * 12;
        y += iy;
        if m < 1 {
            y -= 1;
            m += 12;
        }
    }
    let y = y.clamp(-JULIAN_LIMIT / 365, JULIAN_LIMIT / 365);
    let d = day.clamp(-JULIAN_LIMIT, JULIAN_LIMIT);
    crate::time::ymd_to_julian(y, m as u32, 1) + d - 1
}

/// `Utilities/DateTime/julianToYearMonthDay.c:julianToYearMonthDay()` (proleptic
/// Gregorian, equal to HEC's search for positive years).
pub fn julian_to_ymd(julian: i64) -> (i64, i64, i64) {
    let (y, m, d) = crate::time::julian_to_ymd(julian.clamp(-JULIAN_LIMIT, JULIAN_LIMIT));
    (y, i64::from(m), i64::from(d))
}

/// `Utilities/DateTime/cleanTime.c:cleanTime()`: normalize `(julian, time)` so that `time`
/// lies in `1..=dayLength` of `granularity` units. Returns false (time undefined) for
/// `UNDEFINED_TIME` and for `(0, 0)`, which HEC turns into `UNDEFINED_TIME`.
pub fn clean_time(julian: &mut i64, itime: &mut i64, granularity: i64) -> bool {
    if *julian == UNDEFINED_TIME {
        return false;
    }
    if *julian == 0 && *itime == 0 {
        *julian = UNDEFINED_TIME;
        *itime = -1;
        return false;
    }
    let g = if granularity < 1 {
        MINUTE_GRANULARITY
    } else {
        granularity
    };
    let day_len = if g == MINUTE_GRANULARITY {
        MINS_IN_1_DAY
    } else if g == SECOND_GRANULARITY {
        SECS_IN_1_DAY
    } else {
        (SECS_IN_1_DAY / g).max(1)
    };
    let lt = *itime;
    if lt > 0 && lt <= day_len {
        return true;
    }
    let days = lt / day_len;
    if days > 0 {
        *itime = lt - days * day_len;
        *julian = julian.saturating_add(days);
    } else {
        *itime = lt - days * day_len + day_len;
        *julian = julian.saturating_add(days - 1);
    }
    if *itime == 0 {
        *itime = day_len;
        *julian -= 1;
    }
    true
}

/// `Utilities/DateTime/dayOfWeek.c:dayOfWeek()` (C `%`).
fn day_of_week(julian: i64) -> i64 {
    (julian % 7) + 1
}

/// `Public/ztsOffset.c:ztsOffset()`: offset in seconds of `(julian, seconds)` into its
/// standard interval. `None` for an interval HEC rejects.
pub fn ts_offset(interval: i64, julian: i64, seconds: i64) -> Option<i64> {
    if interval < 1 {
        return None;
    }
    let (mut jul, mut secs) = (julian, seconds);
    clean_time(&mut jul, &mut secs, SECOND_GRANULARITY);
    if interval <= SECS_IN_1_DAY {
        return Some(secs - (secs / interval) * interval);
    }
    if interval == SECS_IN_1_WEEK {
        let day = day_of_week(jul);
        let mut off = (day - 1) * SECS_IN_1_DAY + secs;
        if off == SECS_IN_1_WEEK {
            off = 0;
        }
        return Some(off);
    }
    let (year, month, mut day) = julian_to_ymd(jul);
    if interval <= SECS_IN_1_MONTH {
        if month != 2 {
            if day == 30 || day == 31 {
                return Some(0);
            }
        } else if day >= 28 {
            return Some(0);
        }
        if interval == SECS_IN_TRI_MONTH {
            if day == 9 || day == 10 || day == 19 || day == 20 {
                return Some(0);
            }
            if day > 20 {
                day -= 20;
            } else if day > 10 {
                day -= 10;
            }
            jul = ymd_to_julian(year, month, day);
        } else if interval == SECS_IN_SEMI_MONTH {
            if day == 14 || day == 15 {
                return Some(0);
            }
            if day > 15 {
                day -= 15;
            }
            jul = ymd_to_julian(year, month, day);
        } else if interval != SECS_IN_1_MONTH {
            return None;
        }
        let beg = ymd_to_julian(year, month, 1);
        Some((jul - beg) * SECS_IN_1_DAY + secs)
    } else if interval == SECS_IN_1_YEAR {
        if month == 12 && day == 31 {
            return Some(0);
        }
        let beg = ymd_to_julian(year, 1, 1);
        let mut leap = 0;
        if is_leap_year(year) && month > 2 {
            leap = if year > 0 { 1 } else { -1 };
        }
        Some((jul - beg - leap) * SECS_IN_1_DAY + secs)
    } else {
        None
    }
}

/// `Public/ztsOffsetAdjustToStandard.c:ztsOffsetAdjustToStandard()`: move `(julian,
/// seconds)` forward to the end of its standard interval. Returns the offset that was
/// removed (`None` for an interval HEC rejects; the time is then unchanged).
pub fn offset_adjust_to_standard(
    interval: i64,
    julian: &mut i64,
    seconds: &mut i64,
) -> Option<i64> {
    if interval < 1 {
        return None;
    }
    clean_time(julian, seconds, SECOND_GRANULARITY);
    let off = ts_offset(interval, *julian, *seconds).unwrap_or(-1);
    if interval <= SECS_IN_1_WEEK {
        if off > 0 {
            *seconds += interval - off;
            clean_time(julian, seconds, SECOND_GRANULARITY);
        }
        return Some(off);
    }
    let (year, mut month, mut day) = julian_to_ymd(*julian);
    if interval <= SECS_IN_1_MONTH {
        if interval == SECS_IN_TRI_MONTH {
            day = if day <= 10 {
                10
            } else if day <= 20 {
                20
            } else {
                28
            };
        } else if interval == SECS_IN_SEMI_MONTH {
            day = if day <= 15 { 15 } else { 28 };
        } else if interval == SECS_IN_1_MONTH {
            day = 28;
        } else {
            return None;
        }
        if day >= 25 {
            month += 1;
            *julian = ymd_to_julian(year, month, 1) - 1;
        } else {
            *julian = ymd_to_julian(year, month, day);
        }
    } else if interval == SECS_IN_1_YEAR {
        *julian = ymd_to_julian(year, 12, 31);
    } else {
        return None;
    }
    *seconds = SECS_IN_1_DAY;
    Some(off)
}

/// `Public/ztsOffsetAdjustToOffset.c:ztsOffsetAdjustToOffset()`: move a time to the
/// stamp that carries `offset` within its standard interval.
pub fn offset_adjust_to_offset(offset: i64, interval: i64, julian: &mut i64, seconds: &mut i64) {
    if interval < 1 {
        return;
    }
    offset_adjust_to_standard(interval, julian, seconds);
    if offset == 0 {
        return;
    }
    let (j, s) = increment_time(interval, -1, *julian, *seconds);
    *julian = j;
    *seconds = s.saturating_add(offset);
    clean_time(julian, seconds, SECOND_GRANULARITY);
    if interval == SECS_IN_1_YEAR {
        let (year, month, _) = julian_to_ymd(*julian);
        if is_leap_year(year) && month > 2 {
            if year > 0 {
                *julian += 1;
            } else {
                *julian -= 1;
            }
        }
    }
}

/// `Utilities/DateTime/incrementTime.c:incrementTime()`: step `n` intervals from
/// `(julian, seconds)`; calendar intervals step on the calendar.
pub fn increment_time(interval: i64, n: i64, julian: i64, seconds: i64) -> (i64, i64) {
    if interval < 1 {
        return (julian, seconds);
    }
    let (mut je, mut se) = (julian, seconds);
    if interval < SECS_IN_TRI_MONTH {
        let ls = interval.saturating_mul(n).saturating_add(se);
        if !(-SECS_IN_1_DAY..=SECS_IN_1_DAY).contains(&ls) {
            let mut day = ls / SECS_IN_1_DAY;
            let mut rem = ls - day * SECS_IN_1_DAY;
            if rem < 1 {
                day -= 1;
                rem += SECS_IN_1_DAY;
            }
            je = je.saturating_add(day);
            se = rem;
        } else {
            se = ls;
        }
        clean_time(&mut je, &mut se, SECOND_GRANULARITY);
        return (je, se);
    }
    let imins = interval / SECS_IN_1_MINUTE;
    let off = offset_adjust_to_standard(interval, &mut je, &mut se).unwrap_or(-1);
    let (mut year, mut month, mut day) = julian_to_ymd(je);
    let n = n.clamp(-JULIAN_LIMIT, JULIAN_LIMIT);
    if imins == MINS_IN_TRI_MONTH {
        let im = n / 3;
        let rem = n - im * 3;
        month += im;
        day += rem * 10;
        if day < 1 {
            month -= 1;
            day += 30;
        }
        if day > 31 {
            month += 1;
            day -= 30;
        }
        if month < 1 {
            year -= 1;
            month += 12;
        }
        day = if day > 25 {
            28
        } else if day > 15 {
            20
        } else {
            10
        };
    } else if imins == MINS_IN_SEMI_MONTH {
        let im = n / 2;
        let rem = n - im * 2;
        month += im;
        day += rem * 15;
        if day < 1 {
            month -= 1;
            day += 30;
        }
        if day > 31 {
            month += 1;
            day -= 30;
        }
        if month < 1 {
            year -= 1;
            month += 12;
        }
        day = if day > 25 { 28 } else { 15 };
    } else if (imins - MINS_IN_1_MONTH).abs() <= 2 * MINS_IN_1_DAY {
        month += n;
        day = 28;
    } else if i64::from(imins > MINS_IN_1_YEAR) <= MINS_IN_1_DAY {
        // HEC writes `abs(intervalMinutes > MINS_IN_1_YEAR) <= MINS_IN_1_DAY`, which is
        // always true; every remaining calendar interval is treated as a year.
        year += n;
        month = 12;
        day = 31;
    }
    je = ymd_to_julian(year, month, day);
    if off != 0 {
        offset_adjust_to_offset(off, interval, &mut je, &mut se);
    } else {
        offset_adjust_to_standard(interval, &mut je, &mut se);
    }
    (je, se)
}

/// `Utilities/DateTime/numberPeriods.c:numberPeriods()`: whole intervals from start to end.
pub fn number_periods(interval: i64, j1: i64, s1: i64, j2: i64, s2: i64) -> i64 {
    if interval < 1 {
        return -1;
    }
    let (mut j1, mut s1, mut j2, mut s2) = (j1, s1, j2, s2);
    clean_time(&mut j1, &mut s1, SECOND_GRANULARITY);
    clean_time(&mut j2, &mut s2, SECOND_GRANULARITY);
    let linear = |interval: i64| ((j2 - j1) * SECS_IN_1_DAY + (s2 - s1)) / interval;
    if interval < SECS_IN_TRI_MONTH {
        return linear(interval);
    }
    let imins = interval / SECS_IN_1_MINUTE;
    let (y1, m1, d1) = julian_to_ymd(j1);
    let (y2, m2, d2) = julian_to_ymd(j2);
    // HEC compares the interval in *minutes* with SECS_IN_4_HOURS / SECS_IN_6_HOURS,
    // i.e. Tri-Month (14,400 min) and Semi-Month (21,600 min).
    if imins == SECS_IN_4_HOURS {
        (y2 - y1) * 36 + (m2 - m1) * 3 + (d2 - d1) / 8
    } else if imins == SECS_IN_6_HOURS {
        (y2 - y1) * 24 + (m2 - m1) * 2 + (d2 - d1) / 13
    } else if imins > 40_000 && imins < 45_000 {
        (y2 - y1) * 12 + (m2 - m1) + (d2 - d1) / 27
    } else if imins > 520_000 && imins < 530_000 {
        // As written in HEC: `(y2-y1) + ((m2-m1) + ((d2-d1)/28)/12)`.
        (y2 - y1) + ((m2 - m1) + ((d2 - d1) / 28) / 12)
    } else {
        linear(interval)
    }
}

/// `Internal/ztsRegGetBlockStart.c:ztsRegGetBlockStart()`: block start julian and block
/// size for a regular interval (dss7.md §7.2).
pub fn reg_block_start(julian: i64, interval: i64) -> (i64, i64) {
    if interval < SECS_IN_15_MINUTES {
        return (julian, BLOCK_1_DAY);
    }
    let (mut year, mut month, _) = julian_to_ymd(julian);
    let size;
    if interval < SECS_IN_1_DAY {
        size = BLOCK_1_MONTH;
    } else if interval < SECS_IN_1_WEEK {
        month = 1;
        size = BLOCK_1_YEAR;
    } else if interval < SECS_IN_1_YEAR {
        month = 1;
        year = (year / 10) * 10;
        size = BLOCK_1_DECADE;
    } else {
        month = 1;
        year = (year / 100) * 100;
        size = BLOCK_1_CENTURY;
    }
    (ymd_to_julian(year, month, 1), size)
}

/// `Internal/ztsIrregGetBlockStart.c:ztsIrregGetBlockStart()` (dss7.md §7.3).
pub fn irreg_block_start(julian: i64, block_size: i64) -> Option<i64> {
    let (mut year, mut month, mut day) = julian_to_ymd(julian);
    match block_size {
        BLOCK_1_DAY => {}
        BLOCK_1_MONTH => day = 1,
        BLOCK_1_YEAR => {
            day = 1;
            month = 1;
        }
        BLOCK_1_DECADE => {
            day = 1;
            month = 1;
            year = (year / 10) * 10;
        }
        BLOCK_1_CENTURY => {
            day = 1;
            month = 1;
            year = (year / 100) * 100;
        }
        _ => return None,
    }
    Some(ymd_to_julian(year, month, day))
}

/// `Internal/ztsIncrementBlock.c:ztsIncrementBlock()`: next (or, for a negative size,
/// previous) block start.
pub fn increment_block(julian: i64, block_size: i64) -> i64 {
    let (step, size) = if block_size < 0 {
        (-1, -block_size)
    } else {
        (1, block_size)
    };
    let t = SECS_IN_1_DAY;
    match size {
        BLOCK_1_DAY => julian + step,
        BLOCK_1_MONTH => increment_time(SECS_IN_1_MONTH, step, julian, t).0,
        BLOCK_1_YEAR => increment_time(SECS_IN_1_YEAR, step, julian, t).0,
        BLOCK_1_DECADE => increment_time(SECS_IN_1_YEAR, step * 10, julian, t).0,
        BLOCK_1_CENTURY => increment_time(SECS_IN_1_YEAR, step * 100, julian, t).0,
        _ => 0,
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// D part for a block start: `julianToDate(julian, 4, ...)` (`ddMmmyyyy`,
/// `Utilities/DateTime/yearMonthDayToDate.c` style 4; the year is 4 digits for
/// −999..9999 and `%d` otherwise).
pub fn julian_to_d_part(julian: i64) -> String {
    let (y, m, d) = julian_to_ymd(julian);
    let mon = MONTHS[((m - 1).clamp(0, 11)) as usize];
    if (0..10_000).contains(&y) {
        format!("{d:02}{mon}{y:04}")
    } else {
        format!("{d:02}{mon}{y}")
    }
}

/// Parse a DSS date such as `01Jan2021` (`Utilities/DateTime/dateToJulian.c:dateToJulian()`
/// for the `ddMmmyyyy` form used by DSS7 D parts; case-insensitive; a 2-digit year in a
/// 7-character date gets a century as in `addCentury`).
pub fn parse_date(s: &[u8]) -> Option<i64> {
    let s: Vec<u8> = s
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let dpos = s.iter().position(|b| !b.is_ascii_digit())?;
    if dpos == 0 || dpos > 2 || s.len() < dpos + 3 {
        return None;
    }
    let day: i64 = std::str::from_utf8(&s[..dpos]).ok()?.parse().ok()?;
    let mon = &s[dpos..dpos + 3];
    let month = MONTHS
        .iter()
        .position(|m| m.as_bytes().eq_ignore_ascii_case(mon))? as i64
        + 1;
    let ys = &s[dpos + 3..];
    if ys.is_empty() || ys.len() > 6 || !ys.iter().all(|b| b.is_ascii_digit() || *b == b'-') {
        return None;
    }
    let mut year: i64 = std::str::from_utf8(ys).ok()?.parse().ok()?;
    if year < 100 && s.len() == 7 {
        // Utilities/DateTime/addCentury.c: years below 50 are 20xx (HEC's default pivot).
        year += if year < 50 { 2000 } else { 1900 };
    }
    let day = if day == 0 { 1 } else { day };
    Some(ymd_to_julian(year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_intervals() {
        assert_eq!(standard_interval(b"1Hour").unwrap().code, 3600);
        assert_eq!(standard_interval(b"1HOUR").unwrap().name, "1Hour");
        assert_eq!(standard_interval(b"15Minute").unwrap().code, 900);
        assert_eq!(standard_interval(b"1Minute").unwrap().code, 60);
        assert_eq!(standard_interval(b"IR-Month").unwrap().code, -2);
        assert_eq!(standard_interval(b"IR-Day").unwrap().code, -1);
        assert_eq!(standard_interval(b"IR-Decade").unwrap().code, -4);
        assert_eq!(standard_interval(b"~1Day").unwrap().code, -3);
        assert!(standard_interval(b"xyz").is_none());
        assert!(standard_interval(b"").is_none());
    }

    #[test]
    fn clean_time_conventions() {
        let (mut j, mut s) = (100, 0);
        assert!(clean_time(&mut j, &mut s, 1));
        assert_eq!((j, s), (99, 86_400));
        let (mut j, mut s) = (100, 86_401);
        clean_time(&mut j, &mut s, 1);
        assert_eq!((j, s), (101, 1));
    }

    #[test]
    fn blocks() {
        let j = ymd_to_julian(2018, 9, 8);
        assert_eq!(reg_block_start(j, 60), (j, BLOCK_1_DAY));
        assert_eq!(reg_block_start(j, 3600).0, ymd_to_julian(2018, 9, 1));
        assert_eq!(reg_block_start(j, 86_400).0, ymd_to_julian(2018, 1, 1));
        assert_eq!(
            reg_block_start(j, SECS_IN_1_MONTH).0,
            ymd_to_julian(2010, 1, 1)
        );
        assert_eq!(
            reg_block_start(j, SECS_IN_1_YEAR).0,
            ymd_to_julian(2000, 1, 1)
        );
        assert_eq!(
            increment_block(ymd_to_julian(2018, 1, 31), BLOCK_1_MONTH),
            ymd_to_julian(2018, 2, 28)
        );
        assert_eq!(
            increment_block(ymd_to_julian(2018, 9, 1), BLOCK_1_MONTH),
            ymd_to_julian(2018, 10, 1)
        );
        assert_eq!(
            increment_block(ymd_to_julian(2018, 1, 1), BLOCK_1_YEAR),
            ymd_to_julian(2019, 1, 1)
        );
        assert_eq!(
            increment_block(ymd_to_julian(2010, 1, 1), BLOCK_1_DECADE),
            ymd_to_julian(2020, 1, 1)
        );
        assert_eq!(
            increment_block(ymd_to_julian(2010, 1, 1), -BLOCK_1_DECADE),
            ymd_to_julian(2000, 1, 1)
        );
    }

    #[test]
    fn calendar_steps() {
        // Monthly values end on the last day of the month at 24:00.
        let (j, s) = increment_time(SECS_IN_1_MONTH, 1, ymd_to_julian(2018, 1, 31), 86_400);
        assert_eq!((j, s), (ymd_to_julian(2018, 2, 28), 86_400));
        let (j, _) = increment_time(SECS_IN_1_YEAR, 1, ymd_to_julian(2018, 12, 31), 86_400);
        assert_eq!(j, ymd_to_julian(2019, 12, 31));
        let n = number_periods(
            SECS_IN_1_MONTH,
            ymd_to_julian(2017, 12, 31),
            86_400,
            ymd_to_julian(2018, 12, 31),
            86_400,
        );
        assert_eq!(n, 12);
        assert_eq!(number_periods(3600, 10, 86_400, 11, 86_400), 24);
    }

    #[test]
    fn dates() {
        let j = parse_date(b"08Sep2018").unwrap();
        assert_eq!(j, 43_350);
        assert_eq!(julian_to_d_part(j), "08Sep2018");
        assert_eq!(parse_date(b"01JAN1877"), Some(ymd_to_julian(1877, 1, 1)));
        assert!(parse_date(b"TS-PATTERN").is_none());
        assert_eq!(ymd_to_julian(2018, 13, 1), ymd_to_julian(2019, 1, 1));
        assert_eq!(ymd_to_julian(2018, 0, 1), ymd_to_julian(2017, 12, 1));
    }
}

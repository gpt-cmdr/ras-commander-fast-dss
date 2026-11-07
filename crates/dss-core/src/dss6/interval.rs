//! DSS6 time-series calendar arithmetic: intervals, blocks, value positions.
//!
//! Every routine here is a port of a `heclib_f` Fortran routine and keeps its integer
//! arithmetic (Fortran integer division truncates toward zero; `MOD` takes the sign of
//! the dividend). Times are HEC julian days (1900-01-01 = 1, `IYMDJL`) and minutes past
//! midnight in `1..=1440`: midnight is 24:00 of the previous day (`DATCLN`).
//!
//! Sources (all under `heclib_f/src/`):
//! - `zgintl6.f:ZGINTL6`: E part to interval minutes and values per block.
//! - `zbegdt6.f:ZBEGDT6`: regular block for an interval; `zincbk6.f:ZINCBK6`: next block.
//! - `zirbeg6.f:ZIRBEG6`: irregular block from an `IR-*` or `~interval` E part.
//! - `Support/inctim.f:INCTIM`, `Support/nopers.f:NOPERS`: stepping and counting periods,
//!   including the calendar intervals (tri-month, semi-month, month, year).
//! - `zofset6.f:ZOFSET6`: time offset of a stamp from the standard interval boundary.
//! - `Support/iymdjl.f:IYMDJL`, `Support/jliymd.f:JLIYMD`, `Support/idaywk.f:IDAYWK`,
//!   `Support/datcln.f:DATCLN`, `Support/datcll.f:DATCLL`.
//! - `Support/ymddat.f:YMDDAT` style 104 (`01JAN1951`) for the stored D part.

use crate::time::{julian_to_ymd, ymd_to_julian};
use crate::IrregularBlock;

/// Minutes per day.
pub const DAY_MINUTES: i64 = 1440;

/// `zgintl6` regular intervals: (minutes, E part, values per block `NVALS`). Lookups
/// compare the first 4 characters only.
pub const REGULAR_INTERVALS: [(i32, &str, i32); 24] = [
    (525_600, "1YEAR", 100),
    (43_200, "1MON", 120),
    (21_600, "SEMI-MONTH", 240),
    (14_400, "TRI-MONTH", 360),
    (10_080, "1WEEK", 522),
    (1_440, "1DAY", 366),
    (720, "12HOUR", 62),
    (480, "8HOUR", 93),
    (360, "6HOUR", 124),
    (240, "4HOUR", 186),
    (180, "3HOUR", 248),
    (120, "2HOUR", 372),
    (60, "1HOUR", 744),
    (30, "30MIN", 1488),
    (20, "20MIN", 2232),
    (15, "15MIN", 2976),
    (12, "12MIN", 120),
    (10, "10MIN", 144),
    (6, "6MIN", 240),
    (5, "5MIN", 288),
    (4, "4MIN", 360),
    (3, "3MIN", 480),
    (2, "2MIN", 720),
    (1, "1MIN", 1440),
];

/// Block span of a time-series record. Codes 1..5 are `IBLOCK` in `zbegdt6`/`zirbeg6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockSpan {
    Day,
    Month,
    Year,
    Decade,
    Century,
}

impl BlockSpan {
    pub fn to_irregular(self) -> IrregularBlock {
        match self {
            BlockSpan::Day => IrregularBlock::Day,
            BlockSpan::Month => IrregularBlock::Month,
            BlockSpan::Year => IrregularBlock::Year,
            BlockSpan::Decade => IrregularBlock::Decade,
            BlockSpan::Century => IrregularBlock::Century,
        }
    }
}

/// A decoded E part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EPart {
    /// Regular interval (`zgintl6` `ISTAT = 0`).
    Regular { minutes: i32, block_values: i32 },
    /// `IR-*` irregular data (`zirbeg6`, 6-character match).
    Irregular { block: BlockSpan },
    /// `~<interval>` pseudo-regular data; stored as irregular (`zirbeg6` lines 26-44).
    Pseudo { minutes: i32, block: BlockSpan },
}

fn upper_prefix<const N: usize>(s: &[u8]) -> [u8; N] {
    // Fortran CHARACTER*N assignment: truncate or blank-pad.
    let mut out = [b' '; N];
    for (o, c) in out.iter_mut().zip(s) {
        *o = c.to_ascii_uppercase();
    }
    out
}

/// Regular interval for an E part (`zgintl6`, `ISTAT = 1`; first 4 characters, upper
/// case). `None` for irregular or unknown E parts.
pub fn regular_interval(e: &[u8]) -> Option<(i32, i32)> {
    let key: [u8; 4] = upper_prefix(e);
    if &key[..3] == b"IR-" {
        return None;
    }
    REGULAR_INTERVALS
        .iter()
        .find(|(_, name, _)| upper_prefix::<4>(name.as_bytes()) == key)
        .map(|&(m, _, n)| (m, n))
}

/// Irregular block for an E part: `IR-DAY`, `IR-MONTH`, `IR-YEAR`, `IR-DECADE`,
/// `IR-CENTURY` matched on 6 characters (`zirbeg6.f` lines 46-55).
pub fn irregular_block(e: &[u8]) -> Option<BlockSpan> {
    let key: [u8; 6] = upper_prefix(e);
    [
        (b"IR-DAY", BlockSpan::Day),
        (b"IR-MON", BlockSpan::Month),
        (b"IR-YEA", BlockSpan::Year),
        (b"IR-DEC", BlockSpan::Decade),
        (b"IR-CEN", BlockSpan::Century),
    ]
    .iter()
    .find(|(n, _)| **n == key)
    .map(|&(_, b)| b)
}

/// Block for pseudo-regular data of `minutes` (`zirbeg6.f` lines 34-44).
pub fn pseudo_block(minutes: i32) -> BlockSpan {
    match minutes {
        m if m < 15 => BlockSpan::Day,
        m if m < 1440 => BlockSpan::Month,
        m if m < 10_080 => BlockSpan::Year,
        m if m < 50_000 => BlockSpan::Decade,
        _ => BlockSpan::Century,
    }
}

/// Classify an E part.
pub fn parse_e_part(e: &[u8]) -> Option<EPart> {
    if e.first() == Some(&b'~') {
        let (minutes, _) = regular_interval(&e[1..])?;
        return Some(EPart::Pseudo {
            minutes,
            block: pseudo_block(minutes),
        });
    }
    if let Some((minutes, block_values)) = regular_interval(e) {
        return Some(EPart::Regular {
            minutes,
            block_values,
        });
    }
    irregular_block(e).map(|block| EPart::Irregular { block })
}

/// KVERNO of `6-I` (`60000 + 100*ichar('I')`, `zopen6Int.f` line 250).
pub const KVERNO_6I: i32 = 67_300;

/// Regular block for an interval in a file of version number `kverno`. `zbegdt6.f`
/// lines 26-32 put 15- and 30-minute data (15 <= interval < 60) in daily blocks for
/// files before `6-I` (`IF (IVERNO.LT.67300) GO TO 100`). That line is commented out in
/// HEC's current source ("1990 time frame - don't use"), but HEC Monolith 3.3.27 still
/// reads such files with daily blocks (verified on a 6-EA corpus file, dss6.md §12 O21),
/// so the reader follows it.
pub fn regular_block_for(minutes: i32, kverno: i32) -> BlockSpan {
    if (15..60).contains(&minutes) && kverno < KVERNO_6I {
        BlockSpan::Day
    } else {
        regular_block(minutes)
    }
}

/// Regular block for an interval (`zbegdt6.f:ZBEGDT6` lines 11-57). The 15/30-minute
/// daily-block branch is commented out in HEC's source ("1990 time frame - don't use"),
/// so every interval of 15 minutes or more below a day uses monthly blocks.
pub fn regular_block(minutes: i32) -> BlockSpan {
    match minutes {
        m if m < 15 => BlockSpan::Day,
        m if m < 1440 => BlockSpan::Month,
        m if m < 10_080 => BlockSpan::Year,
        m if m < 525_600 => BlockSpan::Decade,
        _ => BlockSpan::Century,
    }
}

/// `IYMDJL`: julian day of a date. Months 13 / 0 wrap once and days past the end of the
/// month carry over, as in the Fortran (`Support/iymdjl.f` lines 20-48). Years below
/// 100 are not given a century here (stored D parts always have 4 digits).
pub fn iymdjl(year: i64, month: i64, day: i64) -> i64 {
    let (mut y, mut m) = (year, month);
    if m > 12 {
        y += 1;
        m -= 12;
    }
    if m < 1 {
        y -= 1;
        m += 12;
    }
    let m = m.clamp(1, 12) as u32;
    ymd_to_julian(y, m, 1) + day - 1
}

/// `JLIYMD`: (year, month, day) of a julian day.
pub fn jliymd(julian: i64) -> (i64, i64, i64) {
    let (y, m, d) = julian_to_ymd(julian);
    (y, i64::from(m), i64::from(d))
}

/// `IDAYWK`: 1 = Sunday (`Support/idaywk.f`: `MOD(JUL,7)+1`).
pub fn idaywk(julian: i64) -> i64 {
    julian.rem_euclid(7) + 1
}

/// `DATCLN`/`DATCLL`: normalize so the time is in `1..=1440`.
pub fn datcln(julian: i64, minutes: i64) -> (i64, i64) {
    let t = julian * DAY_MINUTES + minutes;
    let (mut j, mut m) = (t.div_euclid(DAY_MINUTES), t.rem_euclid(DAY_MINUTES));
    if m == 0 {
        j -= 1;
        m = DAY_MINUTES;
    }
    (j, m)
}

/// Absolute minutes since julian day 0 00:00.
pub fn abs_minutes(julian: i64, minutes: i64) -> i64 {
    julian * DAY_MINUTES + minutes
}

/// `INCTIM(INTERVAL, 0, NPERIODS, JULS, ISTIME, JULE, IETIME)`
/// (`Support/inctim.f` lines 124-297). Calendar intervals step by logical periods (end
/// of month stays end of month); all others are plain minute arithmetic. The result is
/// normalized to `1..=1440` minutes.
pub fn inctim(interval: i64, nperiods: i64, juls: i64, istime: i64) -> (i64, i64) {
    let nperyear: i64;
    let mut ndayper: i64 = 0;
    if interval == 14_400 {
        nperyear = 36;
        ndayper = 10;
    } else if interval == 21_600 {
        nperyear = 24;
        ndayper = 15;
    } else if (40_000..45_000).contains(&interval) {
        nperyear = 12;
    } else if interval > 520_000 && interval < 530_000 {
        nperyear = 1;
    } else {
        nperyear = 0;
    }

    if nperyear == 0 {
        let total = abs_minutes(juls, istime) + nperiods.saturating_mul(interval);
        let (j, m) = (total.div_euclid(DAY_MINUTES), total.rem_euclid(DAY_MINUTES));
        // inctim.f lines 288-292: never 0000, but 2400 of the previous day.
        return if m == 0 { (j - 1, DAY_MINUTES) } else { (j, m) };
    }

    let ietime = istime;
    let (mut iyear, mut imonth, mut iday) = jliymd(juls);
    // inctim.f lines 194-207: is the start the end of a month?
    let (_, imon2, _) = jliymd(juls + 2);
    let lendmo = imon2 != imonth;
    let mut iofset = 0;
    if !lendmo && nperyear >= 24 {
        iofset = iday % ndayper;
    }
    let jyear = nperiods / nperyear;
    iyear += jyear;
    let mut nleft = 0;
    if nperyear >= 12 {
        let jmonth = (12 * nperiods) / nperyear - jyear * 12;
        imonth += jmonth;
        if nperyear > 12 {
            nleft = nperiods - (jmonth * nperyear) / 12 - jyear * nperyear;
            if nleft < 0 {
                nleft += nperyear / 12;
                imonth -= 1;
            }
        }
    }
    while imonth > 12 {
        iyear += 1;
        imonth -= 12;
    }
    while imonth < 1 {
        iyear -= 1;
        imonth += 12;
    }
    let mut jule;
    if nperyear <= 12 || nleft == 0 {
        jule = if lendmo {
            iymdjl(iyear, imonth + 1, 1) - 1
        } else {
            iymdjl(iyear, imonth, iday)
        };
    } else {
        // inctim.f lines 263-282 (semi- and tri-monthly remainder periods).
        for i in 1..=nleft.min(3) {
            let mut ndaymon = iymdjl(iyear, imonth + 1, 1) - iymdjl(iyear, imonth, 1);
            if ndaymon > 30 {
                ndaymon = 30;
            }
            iday += ndayper;
            if iday >= ndaymon {
                imonth += 1;
                jule = iymdjl(iyear, imonth, 1) - 1 + iofset;
                if iday > 32 && iofset == 0 {
                    jule += ndayper;
                }
                let (y, m, d) = jliymd(jule);
                iyear = y;
                imonth = m;
                iday = d;
                if iday >= ndaymon && i != nleft {
                    iday = 0;
                    imonth += 1;
                }
            }
        }
        jule = iymdjl(iyear, imonth, iday);
    }
    if ietime == 0 {
        (jule - 1, DAY_MINUTES)
    } else {
        (jule, ietime)
    }
}

/// `NOPERS(INTERVAL, 0, JULS, ISTIME, JULE, IETIME)` (`Support/nopers.f` lines 33-86).
pub fn nopers(interval: i64, juls: i64, istime: i64, jule: i64, ietime: i64) -> i64 {
    if interval > 14_000 {
        let (sy, sm, sd) = jliymd(juls);
        let (ey, em, ed) = jliymd(jule);
        if interval == 14_400 {
            return (ey - sy) * 36 + (em - sm) * 3 + (ed - sd) / 8;
        } else if interval == 21_600 {
            return (ey - sy) * 24 + (em - sm) * 2 + (ed - sd) / 13;
        } else if (40_000..45_000).contains(&interval) {
            return (ey - sy) * 12 + (em - sm) + (ed - sd) / 27;
        } else if interval > 520_000 && interval < 530_000 {
            return (ey - sy) + ((em - sm) + (ed - sd) / 28) / 12;
        }
    }
    // Fortran integer division truncates toward zero.
    ((jule - juls) * DAY_MINUTES + (ietime - istime)) / interval
}

/// `ZOFSET6` with `IFLAG = 0`: offset in minutes of a stamp from the standard boundary
/// of `interval` (`zofset6.f` lines 25-87). `None` for an interval it cannot handle.
pub fn time_offset(julian: i64, minutes: i64, interval: i64) -> Option<i64> {
    let (jul, jtime) = datcln(julian, minutes);
    let mut jofset: i64 = -1;
    if interval <= 1440 {
        jofset = jtime - (jtime / interval) * interval;
    } else if interval == 10_080 {
        let kday = idaywk(jul);
        jofset = (kday - 1) * 1440 + jtime;
        if jofset == 10_080 {
            jofset = 0;
        }
    } else {
        let (iyear, _imon, mut iday) = jliymd(jul);
        let (_jy, jmon, jday) = jliymd(jul + 1);
        if jday == 1 && jtime == 1440 {
            jofset = 0;
        } else if interval == 14_400 {
            if iday >= 30 {
                iday = 9;
            }
            jofset = ((iday % 10) - 1) * 1440 + jtime;
        } else if interval == 21_600 {
            if iday >= 30 {
                iday = 14;
            }
            jofset = ((iday % 15) - 1) * 1440 + jtime;
        } else if interval == 43_200 {
            jofset = (iday - 1) * 1440 + jtime;
            if jofset > 43_200 {
                jofset = 43_200 - (1440 - jtime);
            }
        }
        if interval == 525_600 {
            if jday == 1 && jmon == 1 && jtime == 1440 {
                jofset = 0;
            } else {
                jofset = (jul - iymdjl(iyear, 1, 1)) * 1440 + jtime;
            }
        }
    }
    (jofset >= 0).then_some(jofset)
}

/// `ZOFSET6` with `IFLAG = 2`: the actual stamp of a value whose standard (end of
/// period) stamp is `(julian, minutes)` and whose record offset is `offset` minutes:
/// one period back, then forward by the offset (`zofset6.f` lines 142-154).
pub fn apply_offset(julian: i64, minutes: i64, interval: i64, offset: i64) -> (i64, i64) {
    if offset == 0 {
        return (julian, minutes);
    }
    let (j, t) = inctim(interval, -1, julian, minutes);
    datcln(j, t + offset)
}

/// Start of the regular or irregular block that holds `julian`, as (year, month, day)
/// (`zbegdt6.f` lines 15-59; `zirbeg6.f` lines 24-90).
pub fn block_start_ymd(julian: i64, span: BlockSpan) -> (i64, i64, i64) {
    let (y, m, d) = jliymd(julian);
    match span {
        BlockSpan::Day => (y, m, d),
        BlockSpan::Month => (y, m, 1),
        BlockSpan::Year => (y, 1, 1),
        // Fortran integer division (truncates toward zero for negative years, which HEC
        // never writes).
        BlockSpan::Decade => ((y / 10) * 10, 1, 1),
        BlockSpan::Century => ((y / 100) * 100, 1, 1),
    }
}

/// Next block start (`zincbk6.f:ZINCBK6` with a positive `IBLOCK`).
pub fn next_block_ymd(span: BlockSpan, ymd: (i64, i64, i64)) -> (i64, i64, i64) {
    let (mut y, mut m, d) = ymd;
    match span {
        BlockSpan::Day => {
            let (y2, m2, d2) = jliymd(iymdjl(y, m, d) + 1);
            return (y2, m2, d2);
        }
        BlockSpan::Month => {
            m += 1;
            if m > 12 {
                y += 1;
                m -= 12;
            }
        }
        BlockSpan::Year => y += 1,
        BlockSpan::Decade => y += 10,
        BlockSpan::Century => y += 100,
    }
    (y, m, d)
}

/// Julian day of a block start; for weekly data the start used for value positions is
/// moved back to the Sunday on or before it, so values fall on Saturday 24:00
/// (`zrrtsi.f:ZRRTSI6` lines 275-282).
pub fn position_base(interval: i64, ymd: (i64, i64, i64)) -> i64 {
    let jul = iymdjl(ymd.0, ymd.1, ymd.2);
    if interval == 10_080 {
        jul - idaywk(jul) + 1
    } else {
        jul
    }
}

const MONTHS: [&[u8; 3]; 12] = [
    b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV", b"DEC",
];

/// The stored D part for a block start: `YMDDAT` style 104, e.g. `01JAN1951`
/// (`zrrtsi.f` line 261; `zritsi6.f` line 368 `JULDAT(JUL, 104, ...)`).
pub fn d_part(julian: i64) -> Vec<u8> {
    let (y, m, d) = jliymd(julian);
    let mut out = format!("{d:02}").into_bytes();
    out.extend_from_slice(MONTHS[(m - 1) as usize]);
    out.extend_from_slice(format!("{y:04}").as_bytes());
    out
}

/// Parse a D part date `DDMMMYYYY` (also `DMMMYYYY`), case-insensitive, as `DATJUL`
/// reads the style-104 dates HEC stores. Returns the julian day.
pub fn parse_d_part(d: &[u8]) -> Option<i64> {
    let s: Vec<u8> = d
        .iter()
        .copied()
        .filter(|c| *c != b' ')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let mpos = s.iter().position(|c| c.is_ascii_alphabetic())?;
    if mpos == 0 || mpos > 2 || s.len() < mpos + 3 + 4 {
        return None;
    }
    let day: i64 = std::str::from_utf8(&s[..mpos]).ok()?.parse().ok()?;
    let mon = &s[mpos..mpos + 3];
    let month = MONTHS.iter().position(|m| &m[..] == mon)? as i64 + 1;
    let ys = &s[mpos + 3..];
    if ys.len() != 4 || !ys.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: i64 = std::str::from_utf8(ys).ok()?.parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    Some(iymdjl(year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e_parts() {
        assert_eq!(
            parse_e_part(b"1HOUR"),
            Some(EPart::Regular {
                minutes: 60,
                block_values: 744
            })
        );
        assert_eq!(
            parse_e_part(b"15min"),
            Some(EPart::Regular {
                minutes: 15,
                block_values: 2976
            })
        );
        assert_eq!(
            parse_e_part(b"IR-DAY"),
            Some(EPart::Irregular {
                block: BlockSpan::Day
            })
        );
        assert_eq!(
            parse_e_part(b"IR-DECADE"),
            Some(EPart::Irregular {
                block: BlockSpan::Decade
            })
        );
        assert_eq!(
            parse_e_part(b"~1DAY"),
            Some(EPart::Pseudo {
                minutes: 1440,
                block: BlockSpan::Year
            })
        );
        assert_eq!(parse_e_part(b"FOO"), None);
        assert_eq!(regular_block(15), BlockSpan::Month);
        assert_eq!(regular_block(10), BlockSpan::Day);
        assert_eq!(regular_block(1440), BlockSpan::Year);
        assert_eq!(regular_block(10_080), BlockSpan::Decade);
    }

    #[test]
    fn d_parts() {
        let j = iymdjl(1951, 4, 1);
        assert_eq!(d_part(j), b"01APR1951".to_vec());
        assert_eq!(parse_d_part(b"01APR1951"), Some(j));
        assert_eq!(parse_d_part(b"1apr1951"), Some(j));
        assert_eq!(parse_d_part(b"01APR51"), None);
        assert_eq!(parse_d_part(b"TS-PATTERN"), None);
        assert_eq!(parse_d_part(b""), None);
    }

    #[test]
    fn inctim_regular_and_calendar() {
        let j = iymdjl(1951, 1, 1);
        // First 1HOUR value of a January block: 01JAN 01:00; last is 31JAN 24:00.
        assert_eq!(inctim(60, 1, j, 0), (j, 60));
        assert_eq!(inctim(60, 744, j, 0), (j + 30, 1440));
        // Monthly: end of each month.
        assert_eq!(inctim(43_200, 1, j, 0), (j + 30, 1440));
        assert_eq!(inctim(43_200, 2, j, 0), (iymdjl(1951, 2, 28), 1440));
        // From an end-of-month stamp, stay at the end of the month.
        let feb28 = iymdjl(1951, 2, 28);
        assert_eq!(inctim(43_200, 1, feb28, 1440), (iymdjl(1951, 3, 31), 1440));
        assert_eq!(inctim(43_200, -1, feb28, 1440), (j + 30, 1440));
        // Semi-monthly: 15th then end of month.
        assert_eq!(inctim(21_600, 1, j, 0), (iymdjl(1951, 1, 15), 1440));
        assert_eq!(inctim(21_600, 2, j, 0), (j + 30, 1440));
        // Yearly.
        assert_eq!(inctim(525_600, 1, j, 0), (iymdjl(1951, 12, 31), 1440));
        // Negative minute arithmetic.
        assert_eq!(inctim(1440, -1, j, 1440), (j - 1, 1440));
    }

    #[test]
    fn nopers_matches_inctim() {
        let j = iymdjl(1951, 1, 1);
        for intl in [1i64, 5, 15, 60, 1440, 10_080] {
            for n in [0i64, 1, 5, 100] {
                let (je, te) = inctim(intl, n, j, 0);
                assert_eq!(nopers(intl, j, 0, je, te), n, "intl {intl} n {n}");
            }
        }
        for n in [0i64, 1, 5, 13] {
            let (je, te) = inctim(43_200, n, j, 0);
            assert_eq!(nopers(43_200, j, 0, je, te), n);
        }
    }

    #[test]
    fn offsets() {
        let j = iymdjl(2000, 6, 10);
        assert_eq!(time_offset(j, 480, 1440), Some(480));
        assert_eq!(time_offset(j, 1440, 1440), Some(0));
        assert_eq!(time_offset(j, 90, 60), Some(30));
        // Daily value at standard 24:00 with an 8-hour offset is at 08:00 that day.
        assert_eq!(apply_offset(j, 1440, 1440, 480), (j, 480));
        assert_eq!(apply_offset(j, 120, 60, 0), (j, 120));
    }

    #[test]
    fn blocks() {
        let j = iymdjl(1957, 7, 14);
        assert_eq!(block_start_ymd(j, BlockSpan::Decade), (1950, 1, 1));
        assert_eq!(block_start_ymd(j, BlockSpan::Month), (1957, 7, 1));
        assert_eq!(
            next_block_ymd(BlockSpan::Month, (1957, 12, 1)),
            (1958, 1, 1)
        );
        assert_eq!(next_block_ymd(BlockSpan::Day, (1957, 12, 31)), (1958, 1, 1));
        // Weekly positions start on a Sunday.
        let base = position_base(10_080, (1990, 1, 1));
        assert_eq!(idaywk(base), 1);
        assert!(base <= iymdjl(1990, 1, 1));
    }
}

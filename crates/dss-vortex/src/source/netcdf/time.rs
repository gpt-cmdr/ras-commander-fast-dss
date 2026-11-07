//! Time coordinates and windows.
//!
//! Sources: NetCDF-Java 5.5.3 `CalendarDateUnit`, `CalendarDateFormatter.parseIsoTimeString`,
//! `CalendarDate.add`, `CalendarPeriod.fromUnitString`; Vortex `TimeConverter`,
//! `GridDatasetReader.SpecialFileType` and `adjustTimeForSpecialFile`.

use super::refused;
use super::values::java_round;
use crate::error::VortexError;

/// Calendars this reader takes. `Calendar.getChronology` also has noleap, all_leap and 360-day,
/// which no sample uses, and the mixed Gregorian-Julian calendar differs from ISO only before 1582.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Calendar {
    Iso,
    Julian,
}

pub(super) fn calendar(name: Option<&str>) -> Result<Calendar, VortexError> {
    let Some(name) = name else {
        return Ok(Calendar::Iso);
    };
    let is = |x: &str| name.eq_ignore_ascii_case(x);
    if is("gregorian") || is("standard") || is("proleptic_gregorian") || is("ISO8601") || is("none")
    {
        return Ok(Calendar::Iso);
    }
    if is("julian") {
        return Ok(Calendar::Julian);
    }
    if is("noleap")
        || is("365_day")
        || is("all_leap")
        || is("366_day")
        || is("uniform30day")
        || is("360_day")
    {
        return Err(refused(
            "NetCDF time axis",
            format!("calendar {name} has no sample here"),
        ));
    }
    // Calendar.get returns null, so CalendarDateUnit.of takes the default.
    Ok(Calendar::Iso)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Field {
    Millisec,
    Second,
    Minute,
    Hour,
    Day,
    Month,
    Year,
}

/// `CalendarPeriod.fromUnitString`.
fn field(unit: &str) -> Option<Field> {
    let unit = unit.trim().to_lowercase();
    if unit == "s" {
        return Some(Field::Second);
    }
    if unit == "ms" {
        return Some(Field::Millisec);
    }
    let unit = unit.strip_suffix('s').unwrap_or(&unit);
    Some(match unit {
        "second" | "sec" => Field::Second,
        "millisecond" | "millisec" | "msec" => Field::Millisec,
        "minute" | "min" => Field::Minute,
        "hour" | "hr" | "h" => Field::Hour,
        "day" | "d" => Field::Day,
        "month" | "mon" => Field::Month,
        "year" | "yr" => Field::Year,
        _ => return None,
    })
}

/// `CalendarDate.MILLISECS_IN_*`.
fn millis_in(field: Field) -> f64 {
    const YEAR: f64 = 3.15569259747E10;
    match field {
        Field::Millisec => 1.0,
        Field::Second => 1000.0,
        Field::Minute => 60_000.0,
        Field::Hour => 3_600_000.0,
        Field::Day => 86_400_000.0,
        Field::Month => YEAR / 12.0,
        Field::Year => YEAR,
    }
}

/// `SimpleUnit.isDateUnit`, as far as coordinate rules need it: a time unit since a date.
pub(super) fn is_date_unit(unit: &str) -> bool {
    let lower = unit.trim().to_lowercase();
    let Some((head, tail)) = lower.split_once("since") else {
        return false;
    };
    field(head).is_some() && !tail.trim().is_empty()
}

/// Largest magnitude of an instant, in milliseconds. It leaves room for a window rule's hours and
/// days, and for the difference of two instants.
const INSTANT_BOUND: u64 = 1 << 62;

/// A `CalendarDateUnit` that is not a calendar field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct DateUnit {
    field: Field,
    /// Milliseconds since 1970 in the calendar's own day count.
    base: i64,
    calendar: Calendar,
}

fn parse_error(unit: &str, why: &str) -> VortexError {
    refused("NetCDF time units", format!("{unit:?}: {why}"))
}

/// Days from 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a day count from 1970-01-01, proleptic Gregorian.
#[cfg(test)]
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The same for the proleptic Julian calendar: day count from its 1970-01-01.
fn days_from_julian(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(4);
    let yoe = y - era * 4;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + doy;
    era * 1461 + doe - 719_470
}

fn julian_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_470;
    let era = z.div_euclid(1461);
    let doe = z.rem_euclid(1461);
    let yoe = (doe - doe / 1460).min(1460) / 365;
    let yoe = yoe.min(3);
    let y = yoe + era * 4;
    let doy = doe - 365 * yoe;
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) || text.len() > 9 {
        return None;
    }
    text.parse().ok()
}

fn month_length(calendar: Calendar, y: i64, m: i64) -> i64 {
    let leap = match calendar {
        Calendar::Iso => y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0),
        Calendar::Julian => y.rem_euclid(4) == 0,
    };
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    }
}

impl DateUnit {
    /// `CalendarDateUnit`'s constructor and `parseIsoTimeString`. A base date before 1583 in the
    /// default calendar is refused: there the mixed calendar and ISO differ, and no sample has one.
    pub fn parse(calendar: Calendar, unit: &str) -> Result<Self, VortexError> {
        let lower = unit.trim().to_lowercase();
        if lower.starts_with("calendar ") {
            return Err(parse_error(
                unit,
                "calendar-field units have no sample here",
            ));
        }
        let (head, tail) = lower
            .split_once("since")
            .ok_or_else(|| parse_error(unit, "not a date unit"))?;
        // udunitPatternString: (\w*)\s*since. A head holding anything else fails the match.
        let head = head.trim_end();
        if !head.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(parse_error(unit, "unit is not one word"));
        }
        let field = field(head).ok_or_else(|| parse_error(unit, "unknown time unit"))?;
        if matches!(field, Field::Month | Field::Year) {
            return Err(parse_error(
                unit,
                "month and year units have no sample here",
            ));
        }
        let iso = tail.trim();
        // isodatePatternString: ([\+\-?\d]+)([ t]([\.\:?\d]*)([ \+\-]\S*)?z?)?$
        let date_end = iso
            .find(|c: char| !(c == '+' || c == '-' || c == '?' || c.is_ascii_digit()))
            .unwrap_or(iso.len());
        let (date, rest) = iso.split_at(date_end);
        if date.is_empty() {
            return Err(parse_error(unit, "no date"));
        }
        let (mut time, mut zone) = ("", "");
        if !rest.is_empty() {
            let Some(after) = rest.strip_prefix([' ', 't']) else {
                return Err(parse_error(unit, "date is not followed by a time"));
            };
            let time_end = after
                .find(|c: char| !(c == '.' || c == ':' || c == '?' || c.is_ascii_digit()))
                .unwrap_or(after.len());
            time = &after[..time_end];
            let tail = &after[time_end..];
            let body = tail
                .strip_suffix('z')
                .filter(|t| !t.is_empty() || tail == "z");
            // ([ \+\-]\S*)?z? with \S* greedy: a trailing z stays in the zone group when one exists.
            zone = match tail.chars().next() {
                None => "",
                Some(' ' | '+' | '-') => {
                    if tail[1..].chars().any(char::is_whitespace) {
                        return Err(parse_error(unit, "text after the zone"));
                    }
                    tail
                }
                Some('z') if body == Some("") => "",
                Some(_) => return Err(parse_error(unit, "text after the time")),
            };
        }
        let negative = date.starts_with('-');
        let date_text = date.strip_prefix(['-', '+']).unwrap_or(date);
        let (mut year, mut month, mut day) = (0, 1, 1);
        if date_text.contains('-') {
            let mut parts = date_text.split('-').filter(|p| !p.is_empty());
            let bad = || parse_error(unit, "bad date");
            if let Some(p) = parts.next() {
                year = digits(p).ok_or_else(bad)?;
            }
            if let Some(p) = parts.next() {
                month = digits(p).ok_or_else(bad)?;
            }
            if let Some(p) = parts.next() {
                day = digits(p).ok_or_else(bad)?;
            }
        } else {
            let n = date_text.len();
            let bad = || parse_error(unit, "bad date");
            if n % 2 != 0 {
                return Err(bad());
            }
            if n > 3 {
                year = digits(&date_text[..4]).ok_or_else(bad)?;
            }
            if n > 5 {
                month = digits(&date_text[4..6]).ok_or_else(bad)?;
            }
            if n > 7 {
                day = digits(&date_text[6..8]).ok_or_else(bad)?;
            }
        }
        if negative {
            year = -year;
        }
        let (mut hour, mut minute) = (0, 0);
        let mut second = 0.0;
        if !time.is_empty() {
            let bad = || parse_error(unit, "bad time");
            if time.contains(':') {
                let mut parts = time.split(':').filter(|p| !p.is_empty());
                if let Some(p) = parts.next() {
                    hour = digits(p).ok_or_else(bad)?;
                }
                if let Some(p) = parts.next() {
                    minute = digits(p).ok_or_else(bad)?;
                }
                if let Some(p) = parts.next() {
                    second = p.parse::<f64>().map_err(|_| bad())?;
                }
            } else {
                let whole = time.split('.').next().unwrap_or("").len();
                if whole == 1 {
                    hour = digits(time).ok_or_else(bad)?;
                } else if whole % 2 != 0 {
                    return Err(bad());
                } else {
                    if time.len() > 1 {
                        hour = digits(time.get(..2).ok_or_else(bad)?).ok_or_else(bad)?;
                    }
                    if time.len() > 3 {
                        minute = digits(time.get(2..4).ok_or_else(bad)?).ok_or_else(bad)?;
                    }
                    if time.len() > 5 {
                        second = time[4..].parse::<f64>().map_err(|_| bad())?;
                    }
                }
            }
        }
        if !(1..=12).contains(&month)
            || !(1..=month_length(calendar, year, month)).contains(&day)
            || !(0..=23).contains(&hour)
            || !(0..=59).contains(&minute)
        {
            return Err(parse_error(unit, "date field out of range"));
        }
        // Joda's own range is wider. This keeps the millisecond arithmetic below in range.
        if year.abs() > 200_000 {
            return Err(parse_error(unit, "year out of range"));
        }
        if calendar == Calendar::Iso && year < 1583 {
            return Err(parse_error(
                unit,
                "a base date before 1583 depends on the mixed calendar; no sample has one",
            ));
        }
        let days = match calendar {
            Calendar::Iso => days_from_civil(year, month, day),
            Calendar::Julian => days_from_julian(year, month, day),
        };
        let out_of_range = || parse_error(unit, "the date is out of range");
        // In range: the year is at most 200,000, the other fields were checked above.
        let base = days * 86_400_000 + hour * 3_600_000 + minute * 60_000;
        // dt.plus((long) (1000 * second)). A Java long cast saturates, as `as` does, and Joda's
        // addition throws on overflow. NetCDF-Java does not range-check the seconds field.
        let mut base = base
            .checked_add((1000.0 * second) as i64)
            .ok_or_else(out_of_range)?;
        let zone = zone.trim();
        if !zone.is_empty()
            && !zone.eq_ignore_ascii_case("z")
            && !zone.eq_ignore_ascii_case("utc")
            && !zone.eq_ignore_ascii_case("gmt")
        {
            let (zone_negative, text) = match zone.strip_prefix('-') {
                Some(t) => (true, t),
                None => (false, zone.strip_prefix('+').unwrap_or(zone)),
            };
            let bad = || parse_error(unit, "bad time zone");
            let (h, m) = match text.find(':') {
                Some(at) if at > 0 => (
                    digits(&text[..at]).ok_or_else(bad)?,
                    digits(&text[at + 1..]).ok_or_else(bad)?,
                ),
                _ => {
                    if text.len() > 2 {
                        (
                            digits(text.get(..2).ok_or_else(bad)?).ok_or_else(bad)?,
                            digits(text.get(2..).ok_or_else(bad)?).ok_or_else(bad)?,
                        )
                    } else {
                        (digits(text).ok_or_else(bad)?, 0)
                    }
                }
            };
            if h > 23 || m > 59 {
                return Err(bad());
            }
            // DateTimeZone.forOffsetHoursMinutes: the minutes take the hours' sign.
            let offset = (h * 60 + m) * 60_000;
            base = base
                .checked_sub(if zone_negative { -offset } else { offset })
                .ok_or_else(out_of_range)?;
        }
        if base.unsigned_abs() >= INSTANT_BOUND {
            return Err(out_of_range());
        }
        Ok(DateUnit {
            field,
            base,
            calendar,
        })
    }

    /// `CalendarDate.add(double, field)` then `TimeConverter.toZonedDateTime`. Epoch milliseconds
    /// as Vortex reports them. For the Julian calendar the reported instant has the Julian date's
    /// labels, because `fromJulian` parses the label.
    pub fn instant(&self, value: f64) -> Result<i64, VortexError> {
        if !value.is_finite() {
            return Err(refused("NetCDF time coordinate", format!("value {value}")));
        }
        let delta = java_round(value * millis_in(self.field));
        let millis = self
            .base
            .checked_add(delta)
            .filter(|m| m.unsigned_abs() < INSTANT_BOUND)
            .ok_or_else(|| {
                refused(
                    "NetCDF time coordinate",
                    format!("value {value} is out of range"),
                )
            })?;
        if self.calendar == Calendar::Iso {
            return Ok(millis);
        }
        let days = millis.div_euclid(86_400_000);
        let (y, m, d) = julian_from_days(days);
        // A Julian date with no Gregorian label: fromJulian falls back to the instant after a
        // warning. No sample reaches it.
        if d > month_length(Calendar::Iso, y, m) {
            return Err(refused(
                "NetCDF time coordinate",
                "a Julian date with no Gregorian label",
            ));
        }
        Ok(days_from_civil(y, m, d) * 86_400_000 + millis.rem_euclid(86_400_000))
    }
}

/// `GridDatasetReader.SpecialFileType`, in its order, for a lower-cased file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Special {
    AbrscGauge,
    AbrscRadar,
    MrmsIdp,
    Gpm,
    AorcApcp,
    AorcTmp,
    UaSwe,
    NldasApcp,
    Cmorph,
    Gefs,
    LivnehPrecip,
    HrrrWrfsfcf,
    Gfs,
    Undefined,
}

impl Special {
    /// The rule's name in `GridDatasetReader.SpecialFileType`, or `None` when no rule applies.
    pub(super) fn name(self) -> Option<&'static str> {
        Some(match self {
            Special::AbrscGauge => "ABRSC_GAUGE",
            Special::AbrscRadar => "ABRSC_RADAR",
            Special::MrmsIdp => "MRMS_IDP",
            Special::Gpm => "GPM",
            Special::AorcApcp => "AORC_APCP",
            Special::AorcTmp => "AORC_TMP",
            Special::UaSwe => "UA_SWE",
            Special::NldasApcp => "NLDAS_APCP",
            Special::Cmorph => "CMORPH",
            Special::Gefs => "GEFS",
            Special::LivnehPrecip => "LIVNEH_PRECIP",
            Special::HrrrWrfsfcf => "HRRR_WRFSFCF",
            Special::Gfs => "GFS",
            Special::Undefined => return None,
        })
    }
}

/// Full match of literals joined by `.*`, where `.` in a literal matches any character.
fn glob(name: &[char], parts: &[&str]) -> bool {
    fn at(name: &[char], i: usize, part: &[char]) -> bool {
        name.len() >= i + part.len()
            && part
                .iter()
                .enumerate()
                .all(|(k, c)| *c == '.' || name[i + k] == *c)
    }
    let Some((first, rest)) = parts.split_first() else {
        return false;
    };
    let first: Vec<char> = first.chars().collect();
    if !at(name, 0, &first) {
        return false;
    }
    let mut position = first.len();
    for (i, part) in rest.iter().enumerate() {
        let part: Vec<char> = part.chars().collect();
        if i + 1 == rest.len() {
            return name.len() >= position + part.len() && at(name, name.len() - part.len(), &part);
        }
        match (position..=name.len()).find(|p| at(name, *p, &part)) {
            Some(p) => position = p + part.len(),
            None => return false,
        }
    }
    position == name.len()
}

pub(super) fn special_file(file_name: &str, variable: &str) -> Special {
    if file_name.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']) {
        return Special::Undefined;
    }
    let lower = file_name.to_lowercase();
    let name: Vec<char> = lower.chars().collect();
    let is_exact = |digits_at: &[usize], len: usize, literal: &[(usize, char)]| {
        name.len() == len
            && digits_at.iter().all(|i| name[*i].is_ascii_digit())
            && literal.iter().all(|(i, c)| name[*i] == *c)
    };
    if glob(&name, &["", "gaugecorr", "qpe", "01h", "grib2"]) {
        return Special::AbrscGauge;
    }
    if glob(&name, &["", "radaronly", "qpe", "01h", "grib2"]) {
        return Special::AbrscRadar;
    }
    if glob(&name, &["", "multisensor", "qpe", "01h", "grib2"]) {
        return Special::MrmsIdp;
    }
    // The escaped dots of hhr\.ms\.mrg are literal.
    if lower.contains("hhr.ms.mrg")
        && lower
            .find("hhr.ms.mrg")
            .is_some_and(|at| lower[at + 10..].contains("hdf"))
    {
        return Special::Gpm;
    }
    if glob(&name, &["", "aorc", "apcp", "nc4", ""]) {
        return Special::AorcApcp;
    }
    if glob(&name, &["", "aorc", "tmp", "nc4", ""]) {
        return Special::AorcTmp;
    }
    // [0-9]{2}.nc
    if is_exact(&[0, 1], 5, &[(3, 'n'), (4, 'c')]) {
        return Special::UaSwe;
    }
    if variable == "APCP" && glob(&name, &["nldas_fora0125_h.a", ""]) {
        return Special::NldasApcp;
    }
    if glob(&name, &["", "cmorph", "h", "ly", ""]) {
        return Special::Cmorph;
    }
    // ge.*\.pgrb2.*\.0p.*\.f.*\..*, with literal dots.
    if let Some(after) = lower.strip_prefix("ge") {
        let mut rest = after;
        let mut found = true;
        for part in [".pgrb2", ".0p", ".f", "."] {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            return Special::Gefs;
        }
    }
    // prec.\d{4}.nc
    if name.len() == 12
        && name[..4] == ['p', 'r', 'e', 'c']
        && name[5..9].iter().all(char::is_ascii_digit)
        && name[10] == 'n'
        && name[11] == 'c'
    {
        return Special::LivnehPrecip;
    }
    if glob(&name, &["hrrr", "wrfsfcf", ""]) {
        return Special::HrrrWrfsfcf;
    }
    if variable == "Precipitation_rate_surface"
        && name.len() == 6
        && name[..3] == ['g', 'f', 's']
        && name[4] == 'n'
        && name[5] == 'c'
    {
        return Special::Gfs;
    }
    Special::Undefined
}

/// `adjustTimeForSpecialFile`. `start`, `end`, `instant` and `initial` are in milliseconds.
pub(super) fn adjust(
    special: Special,
    start: i64,
    end: i64,
    instant: i64,
    initial: i64,
) -> (i64, i64) {
    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;
    let adjusted_start = match special {
        Special::AbrscGauge | Special::AbrscRadar | Special::MrmsIdp | Special::NldasApcp => {
            start - HOUR
        }
        Special::Gpm | Special::AorcTmp | Special::LivnehPrecip => instant,
        Special::AorcApcp => instant - HOUR,
        Special::UaSwe => initial,
        Special::Gfs => start + 90 * 60_000,
        _ => start,
    };
    let adjusted_end = match special {
        Special::HrrrWrfsfcf | Special::Cmorph => adjusted_start + HOUR,
        Special::Gpm => adjusted_start + 30 * 60_000,
        Special::AorcApcp | Special::AorcTmp => instant,
        Special::UaSwe | Special::LivnehPrecip => adjusted_start + DAY,
        Special::Gefs => adjusted_start + 3 * HOUR,
        Special::Gfs => end + 90 * 60_000,
        _ => end,
    };
    (adjusted_start, adjusted_end)
}

/// `DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm:ss z")`, on the zones a file would state: UTC.
pub(super) fn parse_attribute_date(text: &str) -> Option<i64> {
    let (date, zone) = text.trim().rsplit_once(' ')?;
    if !matches!(zone, "UTC" | "GMT" | "Z") {
        return None;
    }
    let (day, clock) = date.split_once(' ')?;
    let mut d = day.split('-');
    let (y, m, dd) = (d.next()?, d.next()?, d.next()?);
    let mut c = clock.split(':');
    let (h, mi, s) = (c.next()?, c.next()?, c.next()?);
    if y.len() != 4
        || m.len() != 2
        || dd.len() != 2
        || h.len() != 2
        || mi.len() != 2
        || s.len() != 2
    {
        return None;
    }
    let (y, m, dd, h, mi, s) = (
        digits(y)?,
        digits(m)?,
        digits(dd)?,
        digits(h)?,
        digits(mi)?,
        digits(s)?,
    );
    if !(1..=12).contains(&m)
        || !(1..=month_length(Calendar::Iso, y, m)).contains(&dd)
        || h > 23
        || mi > 59
        || s > 59
    {
        return None;
    }
    Some(days_from_civil(y, m, dd) * 86_400_000 + (h * 3600 + mi * 60 + s) * 1000)
}

/// `NetcdfDataReader.parseTimeCellMethod`: the method applied to the time coordinate.
pub fn parse_time_cell_method(cell_methods: &str, time_axis: Option<&str>) -> String {
    if cell_methods.trim().is_empty() {
        return String::new();
    }
    // CELL_METHODS_QUALIFIER, \([^)]*\): a parenthesis with no closer is left in place.
    let mut stripped = String::new();
    let mut rest = cell_methods;
    while let Some(open) = rest.find('(') {
        match rest[open..].find(')') {
            Some(close) => {
                stripped.push_str(&rest[..open]);
                stripped.push(' ');
                rest = &rest[open + close + 1..];
            }
            None => break,
        }
    }
    stripped.push_str(rest);
    if !stripped.contains(':') {
        return stripped.trim_matches(is_java_trimmed).to_string();
    }
    // (?:^|\s)(?:time|name)\s*:\s*([A-Za-z_]+), case-insensitive, first match.
    let names: Vec<&str> = match time_axis {
        Some(name) if !name.trim().is_empty() && !name.eq_ignore_ascii_case("time") => {
            vec!["time", name]
        }
        _ => vec!["time"],
    };
    let chars: Vec<char> = stripped.chars().collect();
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r');
    for start in 0..=chars.len() {
        if start > 0 && !is_space(chars[start - 1]) {
            continue;
        }
        for name in &names {
            let want: Vec<char> = name.chars().collect();
            let end = start + want.len();
            if end > chars.len()
                || !chars[start..end]
                    .iter()
                    .zip(&want)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                continue;
            }
            let mut i = end;
            while i < chars.len() && is_space(chars[i]) {
                i += 1;
            }
            if chars.get(i) != Some(&':') {
                continue;
            }
            i += 1;
            while i < chars.len() && is_space(chars[i]) {
                i += 1;
            }
            let from = i;
            while i < chars.len() && (chars[i].is_ascii_alphabetic() || chars[i] == '_') {
                i += 1;
            }
            if i > from {
                return chars[from..i].iter().collect();
            }
        }
    }
    String::new()
}

/// `String.trim`: everything up to and including the space.
fn is_java_trimmed(c: char) -> bool {
    c <= ' '
}

/// `VortexDataType.fromString`, as (dss string) or nothing for UNDEFINED.
pub(super) fn declared_type(method: &str) -> Option<DeclaredType> {
    let is = |x: &str| method.eq_ignore_ascii_case(x);
    if is("INST-VAL") || is("point") {
        Some(DeclaredType::Instantaneous)
    } else if is("PER-CUM") || is("sum") {
        Some(DeclaredType::Accumulation)
    } else if is("PER-AVER") || is("mean") {
        Some(DeclaredType::Average)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeclaredType {
    Instantaneous,
    Accumulation,
    Average,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(unit: &str, value: f64) -> i64 {
        DateUnit::parse(Calendar::Iso, unit)
            .unwrap()
            .instant(value)
            .unwrap()
    }

    #[test]
    fn units_seen_in_samples_parse() {
        const H: i64 = 3_600_000;
        assert_eq!(at("seconds since 1970-01-01 00:00:00.0 0:00", 3600.0), H);
        assert_eq!(at("seconds since 1970-01-01 00:00:00 UTC", 1.0), 1000);
        assert_eq!(at("Hour since 1970-01-02T00:00:00Z", 1.0), 25 * H);
        assert_eq!(at("hours since 1970-1-1 00:00:0.0", 2.0), 2 * H);
        assert_eq!(at("days since 1970-01-01", 1.5), 36 * H);
        assert_eq!(at("days since 1970-01-01 00:00:00Z", 1.0), 24 * H);
        assert_eq!(at("hours since 1970-01-01 01:00:00 +01:00", 0.0), 0);
        assert_eq!(at("hours since 1970-01-01 00:00:00 -05", 0.0), 5 * H);
    }

    #[test]
    fn calendars_and_units_outside_the_samples_are_refused() {
        assert!(calendar(Some("noleap")).is_err());
        assert!(DateUnit::parse(Calendar::Iso, "months since 2000-01-01").is_err());
        assert!(DateUnit::parse(Calendar::Iso, "hours since 1500-01-01").is_err());
        assert!(DateUnit::parse(Calendar::Iso, "hours since 2000-13-01").is_err());
        assert!(DateUnit::parse(Calendar::Iso, "calendar months since 2000-01-01").is_err());
        assert!(DateUnit::parse(Calendar::Iso, "furlongs since 2000-01-01").is_err());
        assert!(DateUnit::parse(Calendar::Iso, "hours since 999999999-01-01").is_err());
        let far = DateUnit::parse(Calendar::Iso, "days since 2000-01-01").unwrap();
        assert!(far.instant(1e300).is_err());
        assert!(far.instant(f64::NAN).is_err());
    }

    #[test]
    fn julian_dates_keep_their_labels() {
        let unit = DateUnit::parse(Calendar::Julian, "days since 1980-01-06").unwrap();
        // 1980-01-07 in the Julian calendar is labelled 1980-01-07.
        assert_eq!(
            unit.instant(1.0).unwrap(),
            days_from_civil(1980, 1, 7) * 86_400_000
        );
    }

    #[test]
    fn julian_day_counts_round_trip() {
        for z in [-800_000, -1, 0, 1, 3652, 18_262, 400_000] {
            let (y, m, d) = julian_from_days(z);
            assert_eq!(days_from_julian(y, m, d), z, "{y}-{m}-{d}");
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn cell_methods_read_as_vortex_reads_them() {
        let f = |c: &str| parse_time_cell_method(c, None);
        assert_eq!(f("time: sum"), "sum");
        assert_eq!(f("area: mean time: maximum (interval: 1 hr)"), "maximum");
        assert_eq!(f("time : mean"), "mean");
        assert_eq!(f("TIME:sum"), "sum");
        assert_eq!(f("mean"), "mean");
        assert_eq!(f(" sum (x) "), "sum");
        assert_eq!(f("area: mean"), "");
        assert_eq!(f("sometime: sum"), "");
        assert_eq!(f(""), "");
        assert_eq!(parse_time_cell_method("t1: sum", Some("t1")), "sum");
        assert_eq!(parse_time_cell_method("precip: mean", Some("time")), "");
    }

    #[test]
    fn file_rules_follow_the_regexes() {
        let f = |n: &str| special_file(n, "x");
        assert_eq!(f("AORC_APCP_MARFC_1984010100.nc4"), Special::AorcApcp);
        assert_eq!(f("AORC_TMP_MARFC_1984010100.nc4"), Special::AorcTmp);
        assert_eq!(f("01.nc"), Special::UaSwe);
        assert_eq!(f("001.nc"), Special::Undefined);
        assert_eq!(f("prec.2017.nc"), Special::LivnehPrecip);
        assert_eq!(f("precip.2017.nc"), Special::Undefined);
        assert_eq!(
            f("CMORPH_V1.0_ADJ_0.25deg-HLY_2017010101.nc"),
            Special::Cmorph
        );
        assert_eq!(
            f("3B-HHR-L.MS.MRG.3IMERG.20250701-S000000-E002959.0000.V07B.HDF5"),
            Special::Undefined
        );
        assert_eq!(
            f("3B-HHR.MS.MRG.3IMERG.20170103-S110000-E112959.0660.V06B.HDF5.SUB.hdf5"),
            Special::Gpm
        );
        assert_eq!(special_file("gfs.nc", "Temperature"), Special::Undefined);
        assert_eq!(
            special_file("gfs.nc", "Precipitation_rate_surface"),
            Special::Gfs
        );
        assert_eq!(
            special_file("NLDAS_FORA0125_H.A19820101.0000.002.grb.SUB.nc4", "APCP"),
            Special::NldasApcp
        );
        assert_eq!(
            special_file("NLDAS_FORA0125_H.A19820101.0000.002.grb.SUB.nc4", "TMP"),
            Special::Undefined
        );
    }

    #[test]
    fn attribute_dates_need_their_format() {
        assert_eq!(
            parse_attribute_date("1970-01-02 00:00:00 UTC"),
            Some(86_400_000)
        );
        assert_eq!(parse_attribute_date("1970-01-02 00:00:00 PST"), None);
        assert_eq!(parse_attribute_date("1970-1-2 00:00:00 UTC"), None);
    }
}

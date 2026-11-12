//! Names, units and time windows from the file name, in the order of Vortex 1.1.0
//! `io/AscDataReader.getDtos`, and data types as in `VortexGrid.inferDataType`.
//!
//! Vortex matches some digits against the whole path. This reads the file name only, so a PRISM
//! name with no date is refused where Vortex would pick digits from a folder name. Where Java's
//! parser throws, this refuses.

use crate::error::VortexError;
use crate::{DataType, FrameTime};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Described {
    pub short_name: String,
    pub description: Option<String>,
    pub units: String,
    pub time: FrameTime,
    pub data_type: DataType,
}

/// Whether `text` matches groups of alternatives in order, as a regular expression with `.*`
/// between them and at the end. With `anchored`, the first group starts the text.
pub(crate) fn in_order(text: &str, groups: &[&[&str]], anchored: bool) -> bool {
    let Some((first, rest)) = groups.split_first() else {
        return true;
    };
    first.iter().any(|alt| {
        if anchored {
            return text
                .strip_prefix(alt)
                .is_some_and(|tail| in_order(tail, rest, false));
        }
        text.match_indices(alt)
            .any(|(at, _)| in_order(&text[at + alt.len()..], rest, false))
    })
}

pub(crate) const STATUS: &[&str] = &["stable", "provisional", "early"];

/// `prism.*<var>.*(stable|provisional|early).*`, with whether its daily and monthly forms match.
/// Vortex's daily form for `tmin` alone is `...d2.*(d1|d2).*`.
fn prism_var(name: &str, var: &str) -> Option<(bool, bool)> {
    if !in_order(name, &[&["prism"], &[var], STATUS], true) {
        return None;
    }
    let daily = if var == "tmin" {
        in_order(
            name,
            &[&["prism"], &[var], STATUS, &["d2"], &["d1", "d2"]],
            true,
        )
    } else {
        in_order(name, &[&["prism"], &[var], STATUS, &["d1", "d2"]], true)
    };
    let monthly = in_order(name, &[&["prism"], &[var], STATUS, &["m3"]], true);
    Some((daily, monthly))
}

/// `PrismVariable.parse`, as its `toString`.
fn prism_variable(name: &str) -> &'static str {
    let mut base = name.to_string();
    for ext in [".zip", ".bil", ".asc", ".tif", ".tiff"] {
        base = base.replace(ext, "");
    }
    let code = base.split('_').nth(1).map(str::to_ascii_lowercase);
    match code.as_deref() {
        Some("ppt") => "precipitation",
        Some("tmin") => "minimum temperature",
        Some("tmax") => "maximum temperature",
        Some("tmean") => "mean temperature",
        Some("tdmean") => "mean dewpoint",
        Some("vpdmin") => "minimum vapor pressure deficit",
        Some("vpdmax") => "maximum vapor pressure deficit",
        _ => "unknown variable",
    }
}

/// The last match of a left-to-right `\d{count}` search.
pub(crate) fn last_digits(text: &str, count: usize) -> Option<&str> {
    let b = text.as_bytes();
    let (mut at, mut found) = (0, None);
    while at + count <= b.len() {
        if b[at..at + count].iter().all(u8::is_ascii_digit) {
            found = text.get(at..at + count);
            at += count;
        } else {
            at += 1;
        }
    }
    found
}

/// The length of the first `(\d{8}|\d{6}|\d{4})(?:_|\.|$)` in the text.
fn prism_date_length(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    for at in 0..b.len() {
        for count in [8, 6, 4] {
            let Some(digits) = b.get(at..at + count) else {
                continue;
            };
            if digits.iter().all(u8::is_ascii_digit)
                && matches!(b.get(at + count), None | Some(b'_') | Some(b'.'))
            {
                return Some(count);
            }
        }
    }
    None
}

pub(crate) fn refuse(name: &str, why: &str) -> VortexError {
    VortexError::Refused {
        what: format!("time in file name {name}"),
        reason: why.to_string(),
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn month_length(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1899-12-31, which is `dss_core::grid_write::parse_grid_time`'s julian day.
pub(crate) fn julian(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    // Days since 1970-01-01, plus the 25,568 from 1899-12-31 to it.
    era * 146_097 + doe - 719_468 + 25_568
}

/// The latest year [`time_of`] takes. File names state four digits; a header field (SNODAS) is any
/// integer, and a year past this one would overflow the julian arithmetic.
pub(crate) const MAX_YEAR: i64 = 9999;

/// `DateTimeFormatter`'s default SMART resolver: a day from 1 to 31 past the month's end becomes
/// the month's last day. A year of era 0 is rejected, as is any field outside its calendar range,
/// so the arithmetic below cannot overflow for any input.
pub(crate) fn time_of(
    name: &str,
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
) -> Result<(i64, i64), VortexError> {
    if !(1..=MAX_YEAR).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
    {
        return Err(refuse(name, "a date or time field is out of range"));
    }
    Ok((
        julian(year, month, day.min(month_length(year, month))),
        hour * 3600 + minute * 60,
    ))
}

/// ASCII digits to a number; the caller has checked them.
pub(crate) fn number(text: &str) -> i64 {
    text.bytes()
        .fold(0, |n, b| n * 10 + i64::from(b.wrapping_sub(b'0')))
}

/// `\d{4}[_-]\d{2}[_-]\d{2}[t_-]\d{4}` matches, non-overlapping, parsed by Vortex's formatter
/// `yyyy['_']['-']MM['_']['-']dd['t']['_']['-']Hmm`.
fn dated(name: &str) -> Result<Vec<(i64, i64)>, VortexError> {
    let b = name.as_bytes();
    let digit = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let (mut at, mut out) = (0, Vec::new());
    while at + 15 <= b.len() {
        let hit = digit(at..at + 4)
            && matches!(b[at + 4], b'_' | b'-')
            && digit(at + 5..at + 7)
            && matches!(b[at + 7], b'_' | b'-')
            && digit(at + 8..at + 10)
            && matches!(b[at + 10], b't' | b'_' | b'-')
            && digit(at + 11..at + 15);
        if !hit {
            at += 1;
            continue;
        }
        let field = |r: std::ops::Range<usize>| number(name.get(r).unwrap_or(""));
        out.push(time_of(
            name,
            field(at..at + 4),
            field(at + 5..at + 7),
            field(at + 8..at + 10),
            field(at + 11..at + 13),
            field(at + 13..at + 15),
        )?);
        at += 15;
    }
    Ok(out)
}

pub(crate) fn add_seconds(t: (i64, i64), seconds: i64) -> (i64, i64) {
    let total = t.0 * 86_400 + t.1 + seconds;
    (total.div_euclid(86_400), total.rem_euclid(86_400))
}

/// The frame's metadata, from the file name alone.
pub(crate) fn describe(file_name: &str) -> Result<Described, VortexError> {
    let name = file_name.to_lowercase();
    let (mut daily, mut monthly, mut normal, mut qpf) = (false, false, false, false);
    let mut description: Option<String> = None;
    let frequency: &[&[&str]] = &[&["yr"], &["ha", "ma", "da"]];

    let short_name = if let Some((d, m)) = prism_var(&name, "ppt") {
        (daily, monthly) = (d, m);
        "precipitation"
    } else if in_order(&name, &[&["prism"], &["ppt"], &["normal"]], true) {
        normal = true;
        "precipitation"
    } else if let Some((d, m)) = prism_var(&name, "tmean") {
        (daily, monthly) = (d, m);
        "mean temperature"
    } else if let Some((d, m)) = prism_var(&name, "tmin") {
        (daily, monthly) = (d, m);
        "minimum temperature"
    } else if let Some((d, m)) = prism_var(&name, "tmax") {
        (daily, monthly) = (d, m);
        "maximum temperature"
    } else if let Some((d, m)) = prism_var(&name, "tdmean") {
        (daily, monthly) = (d, m);
        "mean dewpoint temperature"
    } else if let Some((d, m)) = prism_var(&name, "vpdmin") {
        (daily, monthly) = (d, m);
        "minimum vapor pressure deficit"
    } else if let Some((d, m)) = prism_var(&name, "vpdmax") {
        (daily, monthly) = (d, m);
        "maximum vapor pressure deficit"
    } else if in_order(&name, &[&["prism_"], &["15s", "30s", "25m"]], true) {
        let base = name.replace(".tif", "").replace(".tiff", "");
        match prism_date_length(&base) {
            Some(8) => daily = true,
            Some(6) => monthly = true,
            _ => {}
        }
        prism_variable(&name)
    } else if in_order(&name, &[&["qpf"], &["1hr"]], true) {
        qpf = true;
        "precipitation"
    } else if in_order(&name, frequency, false) {
        // FilenameUtil.removeExtension(name, true).
        let stem = name.rfind('.').map_or(name.as_str(), |dot| &name[..dot]);
        description = Some(stem.to_string());
        "precipitation-frequency"
    } else if name.starts_with("windspeed") {
        "windspeed"
    } else {
        ""
    };
    if description.is_none() && !short_name.is_empty() {
        description = Some(short_name.to_string());
    }

    let time = if daily {
        let text = last_digits(&name, 8).ok_or_else(|| refuse(&name, "no eight-digit date"))?;
        let date = time_of(
            &name,
            number(&text[..4]),
            number(&text[4..6]),
            number(&text[6..8]),
            0,
            0,
        )?;
        // minusHours(12), and the next day's minusHours(12) for the end.
        FrameTime::Period {
            start: add_seconds(date, -43_200),
            end: add_seconds(date, 43_200),
        }
    } else if normal {
        FrameTime::Period {
            start: (julian(1981, 1, 1), 0),
            end: (julian(2010, 12, 31), 0),
        }
    } else if monthly {
        let text = last_digits(&name, 6).ok_or_else(|| refuse(&name, "no six-digit date"))?;
        let (year, month) = (number(&text[..4]), number(&text[4..6]));
        let start = time_of(&name, year, month, 1, 0, 0)?;
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        FrameTime::Period {
            start,
            end: (julian(next_year, next_month, 1), 0),
        }
    } else if qpf {
        // filenameSansExt.substring(length - 8), "yyMMddHH"; Java's yy is 20yy.
        let stem = name.rfind('.').map_or(name.as_str(), |dot| &name[..dot]);
        let text = stem
            .len()
            .checked_sub(8)
            .and_then(|at| stem.get(at..))
            .filter(|t| t.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| refuse(&name, "no yyMMddHH at the end of the name"))?;
        let end = time_of(
            &name,
            2000 + number(&text[..2]),
            number(&text[2..4]),
            number(&text[4..6]),
            number(&text[6..8]),
            0,
        )?;
        FrameTime::Period {
            start: add_seconds(end, -3_600),
            end,
        }
    } else {
        match dated(&name)?.as_slice() {
            [start, end] if start == end => FrameTime::Instant(*start),
            [start, end] => FrameTime::Period {
                start: *start,
                end: *end,
            },
            [only] => FrameTime::Instant(*only),
            _ => FrameTime::Invariant,
        }
    };

    let units = if name.contains("ppt") || name.contains("qpf") {
        "mm"
    } else if ["tmean", "tmin", "tmax", "tdmean"]
        .iter()
        .any(|v| name.contains(v))
    {
        "Degrees C"
    } else if name.contains("vpdmin") || name.contains("vpdmax") {
        "hPa"
    } else if in_order(&name, frequency, false) {
        "1/1000 in"
    } else {
        ""
    };

    // VortexVariable.fromName is PRECIPITATION for the one name below among those this produces;
    // the data type is compared against Vortex's.
    let data_type = match time {
        FrameTime::Period { .. } if short_name == "precipitation" => DataType::PerCum,
        FrameTime::Period { .. } => DataType::PerAver,
        _ => DataType::InstVal,
    };
    Ok(Described {
        short_name: short_name.to_string(),
        description,
        units: units.to_string(),
        time,
        data_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn julian_is_the_core_writers_day() {
        for (text, y, m, d) in [
            (&b"01JAN2000:0000"[..], 2000, 1, 1),
            (&b"29FEB2024:0000"[..], 2024, 2, 29),
            (&b"31DEC1899:0000"[..], 1899, 12, 31),
            (&b"15JUN2026:0000"[..], 2026, 6, 15),
        ] {
            let (day, _) = dss_core::grid_write::parse_grid_time(text).unwrap();
            assert_eq!(day, julian(y, m, d));
        }
    }

    #[test]
    fn smart_resolver_clamps_the_day_and_rejects_the_month() {
        let d = describe("2023-02-31T0100.tif").unwrap();
        assert_eq!(d.time, FrameTime::Instant((julian(2023, 2, 28), 3600)));
        assert!(describe("2023-13-01T0100.tif").is_err());
        assert!(describe("2023-01-01T2400.tif").is_err());
    }

    #[test]
    fn a_three_date_name_has_no_time() {
        let d = describe("2000-01-01t0000_2000-01-01t0100_2000-01-01t0200.tif").unwrap();
        assert_eq!(d.time, FrameTime::Invariant);
    }
}

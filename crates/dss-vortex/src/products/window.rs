//! A caller's statement of how long a GRIB2 record's accumulation is (packet P46c).
//!
//! MRMS GRIB2 states an instant: template 4.0 with no time range, so Section 4 cannot say what period a
//! QPE covers. Its name can, in the MRMS table (`RadarOnly_QPE_15M`, `MultiSensor_QPE_24H_Pass2`). Vortex
//! gives those names an instant unless a file-name rule gives one hour, so 15M, and 03H to 72H, come out
//! as INST-VAL with a D part only. This rule is deliberately not Vortex's: the reference is the period.
//! The record ends at the file's time, starts the stated duration earlier, and is PER-CUM.
//!
//! The statement is checked against everything that also states a duration, and any disagreement is
//! refused. The cells are not touched: no resampling, no differencing, no rate conversion.
//!
//! **What states a duration.** A variable's name comes from the GRIB2 tables, so all of it counts
//! ([`durations_in`]). A file's name is free text, so only its QPE stem counts ([`stem_durations`]):
//! `24H_RadarOnly_QPE_01H_x.grib2` states one hour, and `storm_24H.grib2` states nothing. This is the same
//! rule as `_stem_seconds` in ras-commander's `precip/_native_vortex.py`; keep the two together.
//!
//! **What is a depth.** `VortexVariable.fromName` calls any name holding `qpe` precipitation, so MRMS's
//! `FLASH_QPE_ARI24H`, a recurrence interval in years, and `FLASH_QPE_FFG01H`, a ratio, arrive here as
//! precipitation. [`not_a_rain_depth`] says otherwise, from the source's own units and name.

use super::{TimeWindow, Variable};
use crate::error::VortexError;
use crate::units::{self, Unit};

/// The MRMS QPE durations, from `MrmsLocalTables.java` by way of `mrms_table.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accumulation {
    M15,
    H01,
    H03,
    H06,
    H12,
    H24,
    H48,
    H72,
}

impl Accumulation {
    pub const ALL: [Accumulation; 8] = [
        Accumulation::M15,
        Accumulation::H01,
        Accumulation::H03,
        Accumulation::H06,
        Accumulation::H12,
        Accumulation::H24,
        Accumulation::H48,
        Accumulation::H72,
    ];

    pub fn seconds(self) -> i64 {
        match self {
            Accumulation::M15 => 15 * 60,
            Accumulation::H01 => 3600,
            Accumulation::H03 => 3 * 3600,
            Accumulation::H06 => 6 * 3600,
            Accumulation::H12 => 12 * 3600,
            Accumulation::H24 => 24 * 3600,
            Accumulation::H48 => 48 * 3600,
            Accumulation::H72 => 72 * 3600,
        }
    }

    /// The name MRMS gives the duration, which `window_rule="accumulation:<name>"` takes.
    pub fn name(self) -> &'static str {
        match self {
            Accumulation::M15 => "15M",
            Accumulation::H01 => "01H",
            Accumulation::H03 => "03H",
            Accumulation::H06 => "06H",
            Accumulation::H12 => "12H",
            Accumulation::H24 => "24H",
            Accumulation::H48 => "48H",
            Accumulation::H72 => "72H",
        }
    }

    /// Exactly one of [`Self::name`]'s values.
    pub fn parse(text: &str) -> Option<Accumulation> {
        Self::ALL.into_iter().find(|a| a.name() == text)
    }
}

/// Word units may stand apart from their number (`6_Hour`, `24-hr`); a single letter may not, since
/// `2 m` and `3-h` are not how these names state a duration. Longest first.
const WORD_UNITS: &[(&str, i64)] = &[
    ("minutes", 60),
    ("minute", 60),
    ("hours", 3600),
    ("mins", 60),
    ("days", 86_400),
    ("hour", 3600),
    ("hrs", 3600),
    ("min", 60),
    ("day", 86_400),
    ("hr", 3600),
    ("mn", 60),
];
const LETTER_UNITS: &[(&str, i64)] = &[("h", 3600), ("m", 60)];

fn unit_at(rest: &[u8], units: &[(&str, i64)]) -> Option<(usize, i64)> {
    units.iter().find_map(|(unit, seconds)| {
        let after = rest.strip_prefix(unit.as_bytes())?;
        // `25mm` and `1km` are not minutes; `24H2` is not a duration.
        (!after.first().is_some_and(u8::is_ascii_alphanumeric)).then_some((unit.len(), *seconds))
    })
}

/// Every duration a name states, as `(the text, seconds)`, in order. Text before the number may be
/// anything but a digit, so `QPE24H` counts, as the gauge-corrected names write it. A false hit can only
/// cause a refusal that names the text; a missed one would give a record the wrong stamps.
pub fn durations_in(name: &str) -> Vec<(String, i64)> {
    let lower = name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // A longer run is a date or time, and no accumulation here is near it.
        let Some(count) = lower[start..i]
            .parse::<i64>()
            .ok()
            .filter(|_| i - start <= 4)
        else {
            continue;
        };
        let rest = &bytes[i..];
        let joined = unit_at(rest, LETTER_UNITS).or_else(|| unit_at(rest, WORD_UNITS));
        let spaced = || match rest.first() {
            Some(b'_' | b'-' | b' ') => {
                unit_at(&rest[1..], WORD_UNITS).map(|(len, seconds)| (len + 1, seconds))
            }
            _ => None,
        };
        if let Some((len, seconds)) = joined.or_else(spaced) {
            found.push((name[start..i + len].to_string(), count * seconds));
            i += len;
        }
    }
    found
}

const QPE_FAMILIES: [&str; 5] = [
    "radaronly",
    "gaugecorr",
    "gaugeonly",
    "multisensor",
    "mountainmapper",
];

fn after_separator(rest: &[u8]) -> &[u8] {
    match rest.first() {
        Some(b'_' | b'-') => &rest[1..],
        _ => rest,
    }
}

// `[_\-]?QPE[_\-]?(\d{1,2})([HM])(?![A-Za-z0-9])` at the start of `rest`, lower case: (length, seconds).
fn qpe_stem_at(rest: &[u8]) -> Option<(usize, i64)> {
    let at_qpe = after_separator(rest);
    let after_qpe = at_qpe.strip_prefix(b"qpe")?;
    let digits = after_separator(after_qpe);
    let count = digits.iter().take_while(|b| b.is_ascii_digit()).count();
    if !(1..=2).contains(&count) {
        return None;
    }
    let unit = match digits.get(count)? {
        b'h' => 3600,
        b'm' => 60,
        _ => return None,
    };
    if digits.get(count + 1).is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    let value: i64 = digits[..count]
        .iter()
        .fold(0, |v, b| v * 10 + i64::from(b - b'0'));
    Some((rest.len() - digits.len() + count + 1, value * unit))
}

/// Each accumulation tied to an MRMS QPE stem in a file name, as `(the text, seconds)`. A stem naming its
/// family outranks a bare `QPE_24H`, and anything else is a label. The same as ras-commander's
/// `_stem_seconds`: `(?:RadarOnly|GaugeCorr|GaugeOnly|MultiSensor|MountainMapper)[_\-]?QPE[_\-]?(\d{1,2})([HM])`,
/// else `(?<![A-Za-z0-9])QPE[_\-]?(\d{1,2})([HM])`, each `(?![A-Za-z0-9])`, ignoring case.
pub fn stem_durations(name: &str) -> Vec<(String, i64)> {
    let lower = name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let stem = QPE_FAMILIES.iter().find_map(|family| {
            let after = bytes[i..].strip_prefix(family.as_bytes())?;
            qpe_stem_at(after).map(|(len, seconds)| (family.len() + len, seconds))
        });
        match stem {
            Some((len, seconds)) => {
                found.push((name[i..i + len].to_string(), seconds));
                i += len;
            }
            None => i += 1,
        }
    }
    if !found.is_empty() {
        return found;
    }
    let mut i = 0;
    while i < bytes.len() {
        let starts = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        match qpe_stem_at(&bytes[i..]).filter(|_| starts && bytes[i] == b'q') {
            Some((len, seconds)) => {
                found.push((name[i..i + len].to_string(), seconds));
                i += len;
            }
            None => i += 1,
        }
    }
    found
}

/// A unit a depth can be in. Rates, ratios, years and flows are not. Strings are `units::parse_unit`'s.
pub fn is_depth_unit(unit: Unit) -> bool {
    matches!(
        unit,
        Unit::Millimetre | Unit::Metre | Unit::Other("IN") | Unit::Other("FT")
    )
}

/// Why a source is not a rain depth, from its own name and units, or `None` if it is. Vortex calls all of
/// these precipitation (`VortexVariable.fromName`), which is what a parity route keeps.
pub fn not_a_rain_depth(variable: &str, source_units: &str) -> Option<String> {
    const VORTEX_ROUTE: &str =
        "To keep the record Vortex writes, state window_rule \"vortex\" with \
                                data_type \"INST-VAL\"";
    let lower = variable.to_ascii_lowercase();
    // A word that ends in "rate": `PrecipRate` and `Precip_rate`, and not `Integrated`.
    let names_a_rate = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.ends_with("rate"));
    let what = if lower.contains("flash") {
        Some("a FLASH product")
    } else if lower.contains("precipitable") {
        Some("precipitable water")
    } else if names_a_rate {
        Some("a rate")
    } else {
        None
    };
    if let Some(what) = what {
        return Some(format!(
            "{variable} is {what}, not a rain depth. {VORTEX_ROUTE}"
        ));
    }
    match units::parse_unit(Some(source_units)) {
        Some(unit) if is_depth_unit(unit) => None,
        _ => Some(format!(
            "{variable} is in {source_units:?}, so it is not a rain depth. {VORTEX_ROUTE}"
        )),
    }
}

fn refuse_disagreement(
    what: &str,
    name: &str,
    wanted: Accumulation,
    stated: Vec<(String, i64)>,
    since: bool,
) -> Result<bool, VortexError> {
    if since {
        return Err(VortexError::Refused {
            what: format!("accumulation {}", wanted.name()),
            reason: format!(
                "the {what} {name} states an accumulation since a time of day, whose length changes \
                 with the hour"
            ),
        });
    }
    if let Some((text, _)) = stated.iter().find(|(_, s)| *s != wanted.seconds()) {
        return Err(VortexError::Refused {
            what: format!("accumulation {}", wanted.name()),
            reason: format!(
                "the {what} {name} states {text}, and {} was asked for. Nothing was stamped: one \
                 of them is wrong, and the cells cannot say which",
                wanted.name()
            ),
        });
    }
    Ok(!stated.is_empty())
}

/// The default route: a file name that gives a one-hour window, on a variable stating another duration.
pub(super) fn refuse_one_hour_name_disagreement(
    file_name: &str,
    variable_name: &str,
) -> Result<(), VortexError> {
    const ONE_HOUR: i64 = 3600;
    let stated = durations_in(variable_name);
    match stated.iter().find(|(_, seconds)| *seconds != ONE_HOUR) {
        Some((text, _)) => Err(VortexError::Refused {
            what: format!("time window of {file_name}"),
            reason: format!(
                "the variable {variable_name} states {text}, and the file name would give a one-hour \
                 window (01H). One of them is wrong, and the cells cannot say which. Observed behaviour \
                 we did not implement: Vortex stamps the record as one hour. Correct the file's name, or \
                 pass WindowRule::Vortex for Vortex's window"
            ),
        }),
        None => Ok(()),
    }
}

/// Check a name pair against the statement, and say what else states it. Refuses on any disagreement.
pub fn stated_by(
    file_name: &str,
    variable_name: &str,
    wanted: Accumulation,
) -> Result<&'static str, VortexError> {
    let by_variable = refuse_disagreement(
        "variable",
        variable_name,
        wanted,
        durations_in(variable_name),
        variable_name.to_ascii_lowercase().contains("since"),
    )?;
    let file = file_name.to_ascii_lowercase();
    let by_file = refuse_disagreement(
        "file name",
        file_name,
        wanted,
        stem_durations(file_name),
        ["qpe_since", "qpe-since", "qpesince"]
            .iter()
            .any(|stem| file.contains(stem)),
    )?;
    Ok(match (by_variable, by_file) {
        (true, true) => "variable+file_name",
        (true, false) => "variable",
        (false, true) => "file_name",
        (false, false) => "caller",
    })
}

/// The window of one step under the statement. Code table 4.10's 1 is an accumulation; a record with
/// any other statistic is not a depth over its interval. A record that states its own interval must
/// state the same one, and then its own stamps stand.
pub(super) fn accumulation_window(
    file_name: &str,
    variable: &Variable,
    step: usize,
    wanted: Accumulation,
) -> Result<TimeWindow, VortexError> {
    let refuse = |reason: String| VortexError::Refused {
        what: format!("accumulation {} of {}", wanted.name(), variable.name),
        reason,
    };
    stated_by(file_name, &variable.name, wanted)?;
    let current = variable
        .steps
        .get(step)
        .ok_or_else(|| refuse(format!("step {step}")))?;
    if !matches!(variable.statistic, None | Some(1)) {
        return Err(refuse(format!(
            "the record states statistic {:?} of code table 4.10, which is not an accumulation",
            variable.statistic
        )));
    }
    if variable.interval {
        let own = current.end - current.start;
        if own != wanted.seconds() {
            return Err(refuse(format!(
                "the record states its own interval of {own} s, and {} s was asked for",
                wanted.seconds()
            )));
        }
        return Ok(TimeWindow {
            start: current.start,
            end: current.end,
        });
    }
    Ok(TimeWindow {
        start: current.end - wanted.seconds(),
        end: current.end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(name: &str) -> Vec<i64> {
        durations_in(name).into_iter().map(|(_, s)| s).collect()
    }

    #[test]
    fn real_names_state_only_their_duration() {
        for (name, want) in [
            (
                "MRMS_MultiSensor_QPE_24H_Pass2_00.00_20240920-120000.grib2",
                vec![86_400],
            ),
            (
                "MRMS_RadarOnly_QPE_15M_00.00_20250115-060000.grib2",
                vec![900],
            ),
            ("RadarOnly_QPE_01H_altitude_above_msl", vec![3600]),
            (
                "MultiSensor_QPE_72H_Pass1_altitude_above_msl",
                vec![259_200],
            ),
            ("GaugeCorrQPE03H_altitude_above_msl", vec![10_800]),
            (
                "Total_precipitation_surface_layer_6_Hour_Accumulation",
                vec![21_600],
            ),
            ("st4_conus.2026103012.01h.grb2", vec![3600]),
            ("rain-24-hr.grib2", vec![86_400]),
            ("rain_1day.grib2", vec![86_400]),
            // None of these is a duration.
            (
                "Total_precipitation_surface_Mixed_intervals_Accumulation",
                vec![],
            ),
            ("VAR209-6-250_FROM_161-0--1_altitude_above_msl", vec![]),
            ("hrrr.t06z.wrfsfcf06.grib2", vec![]),
            ("rain_25mm_1km.grib2", vec![]),
            ("x_00.00_20240920-120000.grib2", vec![]),
            ("x.grib2", vec![]),
            ("", vec![]),
        ] {
            assert_eq!(seconds(name), want, "{name}");
        }
        // Every name MRMS uses, in both spellings.
        for a in Accumulation::ALL {
            assert_eq!(seconds(a.name()), vec![a.seconds()]);
            assert_eq!(
                seconds(&format!("x_QPE{}.grib2", a.name())),
                vec![a.seconds()]
            );
        }
    }

    /// ras-commander's `test_accumulation_is_tied_to_the_qpe_stem`, row for row, then edges of the regex.
    #[test]
    fn stems_match_ras_commanders_rule() {
        for (name, want) in [
            ("24H_RadarOnly_QPE_01H_legitimate.grib2", vec![3600]),
            (
                "MRMS_MultiSensor_QPE_24H_Pass2_00.00_20240920-120000.grib2",
                vec![86_400],
            ),
            ("RadarOnly_QPE_15M_altitude_above_msl", vec![900]),
            ("GaugeCorrQPE03H_altitude_above_msl", vec![10_800]),
            ("GaugeCorr-QPE-06H", vec![21_600]),
            ("radaronly_qpe_12h", vec![43_200]),
            ("QPE_24H_RadarOnly_QPE_01H.grib2", vec![3600]),
            ("QPE_72H_label.grib2", vec![259_200]),
            ("hrrr_24H.t12z.wrfsfcf01.grib2", vec![]),
            ("FLASH_QPE_ARI01H_00.00.grib2", vec![]),
            // A letter before a bare QPE, a third digit, and a letter after.
            ("xQPE_24H.grib2", vec![]),
            ("QPE_124H.grib2", vec![]),
            ("QPE_24Hours.grib2", vec![]),
            ("QPE__24H.grib2", vec![]),
            ("24H.grib2", vec![]),
            ("", vec![]),
            (
                "RadarOnly_QPE_01H_MultiSensor_QPE_24H.grib2",
                vec![3600, 86_400],
            ),
        ] {
            let got: Vec<i64> = stem_durations(name).into_iter().map(|(_, s)| s).collect();
            assert_eq!(got, want, "{name}");
        }
    }

    /// Every row of MRMS's table. A name's own table entry decides, so a new row must be looked at.
    #[test]
    fn of_every_mrms_row_only_qpe_depths_count_as_rain() {
        let accepted: Vec<&str> = crate::products::mrms_table::MRMS_PARAMETERS
            .iter()
            .filter(|r| {
                let name = format!("{}_surface", r.3);
                crate::units::variable_from_name(&name)
                    == Some(crate::units::Variable::Precipitation)
                    && not_a_rain_depth(&name, r.5).is_none()
            })
            .map(|r| r.3)
            .collect();
        for name in &accepted {
            let lower = name.to_ascii_lowercase();
            assert!(
                QPE_FAMILIES.iter().any(|f| lower.starts_with(f)) && lower.contains("qpe"),
                "{name}"
            );
        }
        // 7 durations each of RadarOnly, GaugeCorr, GaugeOnly, MountainMapper, MultiSensor Pass1 and
        // Pass2, and RadarOnly's 15M.
        assert_eq!(accepted.len(), 43, "{accepted:?}");
        for name in ["FLASH_QPE_ARI24H", "FLASH_QPE_FFG01H", "PrecipRate"] {
            assert!(!accepted.contains(&name), "{name}");
        }
    }

    #[test]
    fn what_is_a_depth() {
        for (variable, units, depth) in [
            ("MultiSensor_QPE_24H_Pass2_surface", "mm", true),
            ("Total_precipitation_surface", "kg.m-2", true),
            ("APCP_surface", "in", true),
            ("FLASH_QPE_ARI24H_surface", "year", false),
            ("FLASH_QPE_ARI24H_surface", "mm", false),
            ("FLASH_QPE_FFG01H_surface", "dimensionless", false),
            ("Precipitable_water_entire_atmosphere", "kg.m-2", false),
            ("Precipitation_rate_surface", "mm/hr", false),
            ("Rain", "mm/hr", false),
            ("Rain", "", false),
            ("Rain", "not a unit", false),
            ("Rain", "CFS", false),
        ] {
            assert_eq!(
                not_a_rain_depth(variable, units).is_none(),
                depth,
                "{variable} {units}"
            );
        }
    }

    #[test]
    fn every_name_a_statement_can_take_round_trips_and_nothing_else_parses() {
        for a in Accumulation::ALL {
            assert_eq!(Accumulation::parse(a.name()), Some(a));
        }
        for bad in [
            "",
            "24",
            "24h",
            "1H",
            "24H ",
            "02H",
            "30M",
            "accumulation:24H",
        ] {
            assert_eq!(Accumulation::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn who_states_it_is_reported_and_any_disagreement_is_refused() {
        let w = Accumulation::H24;
        const V24: &str = "MultiSensor_QPE_24H_Pass2_altitude_above_msl";
        assert_eq!(
            stated_by("x_QPE_24H.grib2", V24, w).unwrap(),
            "variable+file_name"
        );
        assert_eq!(stated_by("x.grib2", V24, w).unwrap(), "variable");
        assert_eq!(stated_by("x_24H.grib2", V24, w).unwrap(), "variable");
        assert_eq!(stated_by("x_01H.grib2", V24, w).unwrap(), "variable");
        assert_eq!(
            stated_by("x_QPE_24H.grib2", "Rain", w).unwrap(),
            "file_name"
        );
        assert_eq!(stated_by("x.grib2", "Rain", w).unwrap(), "caller");
        // Each side alone, and a name that states two.
        for (file, variable) in [
            ("x_QPE_01H.grib2", V24),
            ("x.grib2", "RadarOnly_QPE_01H_altitude_above_msl"),
            ("RadarOnly_QPE_24H_MultiSensor_QPE_01H.grib2", V24),
            ("x_RadarOnly_QPE_Since12Z.grib2", V24),
            ("x.grib2", "RadarOnly_QPE_Since12Z"),
        ] {
            let e = stated_by(file, variable, w).unwrap_err();
            assert!(
                matches!(e, VortexError::Refused { .. }),
                "{file} {variable}"
            );
        }
    }
}

#[cfg(test)]
mod rain_depth_words {
    use super::not_a_rain_depth;

    #[test]
    fn a_word_ending_in_rate_is_a_rate_and_a_word_holding_it_is_not() {
        for name in ["PrecipRate_surface", "precip_rate", "Precipitation_Rate"] {
            let why = not_a_rain_depth(name, "mm").unwrap();
            assert!(why.contains("a rate"), "{why}");
        }
        for name in [
            "Integrated_QPE_01H",
            "MultiSensor_QPE_24H_Pass2_altitude_above_msl",
        ] {
            assert_eq!(not_a_rain_depth(name, "mm"), None, "{name}");
        }
    }

    #[test]
    fn each_refusal_names_the_route_that_keeps_vortexs_record() {
        for why in [
            not_a_rain_depth("FLASH_QPE_ARI24H_surface", "year").unwrap(),
            not_a_rain_depth("Some_QPE_01H", "year").unwrap(),
        ] {
            assert!(why.contains("window_rule \"vortex\""), "{why}");
            assert!(why.contains("data_type \"INST-VAL\""), "{why}");
        }
    }
}

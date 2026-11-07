//! The product table: names, units, missing values and time windows, as Vortex reports them.
//!
//! NetCDF-Java builds these names from the WMO, NCEP and MRMS tables. Only entries with a sampled
//! product are here, so a record outside this table is a typed refusal rather than a guess.
//! MRMS's table is generated in full from `MrmsLocalTables.java`.
//!
//! Sources: NetCDF-Java 5.5.3 `Grib2Iosp.makeVariableNameFromTable`, `makeVariableLongName`,
//! `Grib2Tables.getLevelNameShort`, `GribStatType`, `GribIospBuilder.makeGroup`,
//! `EnhanceScaleMissingUnsignedImpl.isMissing`. Vortex 1.1.0 `GridDatasetReader`,
//! `VortexGrid.inferDataType`, `VortexVariable.fromName`.
//!
//! Windows set from a file's name are Vortex's `SpecialFileType` rules, and they mean a renamed
//! file gets a different window. We follow them because the target is Vortex's result, with one
//! exception.
//!
//! Observed behaviour we did not implement: on Vortex 1.1.0 and 0.13.3, a file named
//! `hrrr*wrfsfcf*` gets a window of the record's start plus one hour, whatever the record's own
//! interval. In `hrrr.t06z.wrfsfcf06` (HRRR, 2024-09-20) the 0-6 h APCP total is reported as
//! 06:00 to 07:00. `ge*.pgrb2*.0p*.f*.*` has the same form with three hours. This library
//! refuses a statistically processed record whose own interval differs from that window, by
//! default, and [`WindowRule::Vortex`] returns Vortex's window.

pub(crate) mod mrms_table;
pub mod window;

use crate::error::VortexError;
use crate::source::grib::pds::Product;
use crate::DataType;
use mrms_table::MRMS_PARAMETERS;
use std::collections::BTreeMap;

/// The parameter a record carries, as the table states it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableEntry {
    /// `Grib2Parameter.getName`, before `GribUtils.makeNameFromDescription`.
    pub parameter: String,
    /// Empty where the table gives no unit.
    pub unit: String,
    /// `_FillValue` when the table has one.
    pub fill: Option<f32>,
    /// `missing_value`. Vortex reports this as the grid's nodata. NaN for WMO and NCEP tables.
    pub missing: f32,
    /// `Unknown Parameter` long name instead of a name, for MRMS codes outside its table.
    pub unknown_parameter: bool,
}

/// WMO and NCEP parameters with a sampled product: (discipline, category, number, name, unit).
/// Sources: `WmoParamTable` and the NCEP local tables, as Vortex 1.1.0 reports them.
const PARAMETERS: &[(u8, u8, u8, &str, &str)] = &[
    (0, 0, 0, "Temperature", "K"),
    (0, 1, 8, "Total precipitation", "kg.m-2"),
    (0, 19, 1, "Albedo", "%"),
];

/// NCEP local parameters, center 7 only (`NcepLocalTables`).
const NCEP_PARAMETERS: &[(u8, u8, u8, &str, &str)] =
    &[(0, 16, 196, "Composite reflectivity", "dB")];
const NCEP_CENTER: u16 = 7;

/// `Grib2Tables.getLevelNameShort` and code table 4.5, for levels with a sampled product.
const LEVELS: &[(u8, &str, &str)] = &[
    (1, "surface", "Ground or water surface"),
    (10, "entire_atmosphere", "Entire atmosphere"),
    (
        102,
        "altitude_above_msl",
        "Specific altitude above mean sea level",
    ),
    (
        103,
        "height_above_ground",
        "Specified height level above ground",
    ),
];

/// MRMS records carry center 161 (`Grib2TableConfig`, the `mrms` table).
const MRMS_CENTER: u16 = 161;

/// Find a record's table entry.
pub fn identify(product: &Product) -> Result<TableEntry, VortexError> {
    let (discipline, category, number) = (product.discipline, product.category, product.number);
    let refuse = || VortexError::Refused {
        what: format!("GRIB2 parameter {discipline}-{category}-{number}"),
        reason: "no product table entry".to_string(),
    };
    if product.center == MRMS_CENTER {
        // LocalTables.getParameter: a later add() of the same id replaces an earlier one.
        let row = MRMS_PARAMETERS
            .iter()
            .rev()
            .find(|r| (r.0, r.1, r.2) == (discipline, category, number));
        return match row {
            Some(row) => Ok(TableEntry {
                parameter: row.3.to_string(),
                unit: row.5.to_string(),
                fill: Some(row.6),
                missing: row.7,
                unknown_parameter: false,
            }),
            // Grib2Iosp: no table entry gives VAR..._FROM_..., units "" and missing NaN.
            None if discipline == 209 => Ok(TableEntry {
                parameter: String::new(),
                unit: String::new(),
                fill: None,
                missing: f32::NAN,
                unknown_parameter: true,
            }),
            None => Err(refuse()),
        };
    }
    let local: &[_] = if product.center == NCEP_CENTER {
        NCEP_PARAMETERS
    } else {
        &[]
    };
    PARAMETERS
        .iter()
        .chain(local)
        .find(|r| (r.0, r.1, r.2) == (discipline, category, number))
        .map(|r| TableEntry {
            parameter: r.3.to_string(),
            unit: r.4.to_string(),
            fill: None,
            missing: f32::NAN,
            unknown_parameter: false,
        })
        .ok_or_else(refuse)
}

/// `GribStatType.getStatTypeFromGrib2` names for the codes with a sampled product.
fn statistic_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Average"),
        1 => Some("Accumulation"),
        2 => Some("Maximum"),
        3 => Some("Minimum"),
        _ => None,
    }
}

/// A record and what the table says about it.
pub struct RecordIdentity {
    pub index: usize,
    pub product: Product,
    pub entry: TableEntry,
}

/// One time step of a variable.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub record: usize,
    pub start: i64,
    pub end: i64,
}

/// One of Vortex's variables in a file.
#[derive(Debug, Clone)]
pub struct Variable {
    /// `GridDatatype.getShortName`.
    pub name: String,
    /// `GridDatatype.getDescription`.
    pub description: String,
    pub entry: TableEntry,
    /// Code table 4.10, for a statistically processed variable.
    pub statistic: Option<u8>,
    /// `true` when the variable's time axis has bounds (`CoordinateTimeIntv`).
    pub interval: bool,
    pub steps: Vec<Step>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct GroupKey {
    discipline_category_number: (u8, u8, u8),
    level_type: u8,
    layer: bool,
    statistic: Option<u8>,
    center: u16,
    template: u16,
    /// `Grib2Variable.hashCode`: generating process types 6 and 7 are variables of their own.
    error_process: Option<u8>,
}

/// Code table 4.3 values with a sampled product: analysis, forecast, observation, and the two
/// error types `Grib2Variable` separates. Names do not carry the others, so a probability or
/// climatological record would read as the parameter itself. Ensemble templates are refused by
/// their own rule.
const SAMPLED_PROCESSES: [u8; 5] = [0, 2, 6, 7, 8];

/// `GribCollectionMutable` variable grouping, for what the sampled products use: one vertical
/// level, no ensembles. Anything else is a typed refusal of that variable:
/// `GridDatasetReader` would read every level or member into one array.
pub fn group(records: Vec<RecordIdentity>) -> (Vec<Variable>, Vec<(usize, VortexError)>) {
    let mut groups: BTreeMap<GroupKey, Vec<RecordIdentity>> = BTreeMap::new();
    let mut unlisted = Vec::new();
    for record in records {
        let p = &record.product;
        if p.ensemble_member.is_none() && !SAMPLED_PROCESSES.contains(&p.generating_type) {
            unlisted.push((
                record.index,
                VortexError::Refused {
                    what: "GRIB2 variable".to_string(),
                    reason: format!(
                        "generating process type {} has no sampled product",
                        p.generating_type
                    ),
                },
            ));
            continue;
        }
        let key = GroupKey {
            discipline_category_number: (p.discipline, p.category, p.number),
            level_type: p.level_type1,
            layer: p.is_layer(),
            statistic: p.statistic,
            center: p.center,
            template: p.template,
            error_process: Some(p.generating_type).filter(|t| matches!(t, 6 | 7)),
        };
        groups.entry(key).or_default().push(record);
    }
    let mut variables: Vec<Variable> = Vec::new();
    let mut records_of = Vec::new();
    for (_, group) in groups {
        let refuse = |reason: String, unlisted: &mut Vec<(usize, VortexError)>| {
            for r in &group {
                unlisted.push((
                    r.index,
                    VortexError::Refused {
                        what: "GRIB2 variable".to_string(),
                        reason: reason.clone(),
                    },
                ));
            }
        };
        let first = &group[0];
        let same_level = group.iter().all(|r| {
            r.product.level1 == first.product.level1 && r.product.level2 == first.product.level2
        });
        if !same_level {
            refuse("several vertical levels".to_string(), &mut unlisted);
            continue;
        }
        if group.iter().any(|r| r.product.ensemble_member.is_some()) {
            refuse("an ensemble member template".to_string(), &mut unlisted);
            continue;
        }
        // Grib2Variable leaves ordinary process types in one variable, and no sampled product has
        // that, so there is no reference for what the variable then means.
        if group
            .iter()
            .any(|r| r.product.generating_type != first.product.generating_type)
        {
            refuse(
                "several generating process types".to_string(),
                &mut unlisted,
            );
            continue;
        }
        let Some((name, description)) = names(first, &group) else {
            refuse(
                "no level or statistic table entry".to_string(),
                &mut unlisted,
            );
            continue;
        };
        let mut steps: Vec<Step> = group
            .iter()
            .map(|r| Step {
                record: r.index,
                start: r.product.start,
                end: r.product.end,
            })
            .collect();
        // TimeCoordIntvValue and CoordinateTime sort by end, then start.
        steps.sort_by_key(|s| (s.end, s.start));
        if steps
            .windows(2)
            .any(|w| (w[0].start, w[0].end) == (w[1].start, w[1].end))
        {
            refuse("two records at one time".to_string(), &mut unlisted);
            continue;
        }
        records_of.push(group.iter().map(|r| r.index).collect::<Vec<_>>());
        variables.push(Variable {
            name,
            description,
            entry: first.entry.clone(),
            statistic: first.product.statistic,
            interval: first.product.statistic.is_some(),
            steps,
        });
    }
    // A name that two groups reach would select the first; neither can be the requested one.
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for variable in &variables {
        *count.entry(variable.name.clone()).or_default() += 1;
    }
    let mut kept = Vec::new();
    for (variable, records) in variables.into_iter().zip(records_of) {
        if count[&variable.name] > 1 {
            for index in records {
                unlisted.push((
                    index,
                    VortexError::Refused {
                        what: format!("GRIB2 variable {}", variable.name),
                        reason: "two kinds of record share this name".to_string(),
                    },
                ));
            }
        } else {
            kept.push(variable);
        }
    }
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    (kept, unlisted)
}

/// `CoordinateTimeIntv.getTimeIntervalName`.
fn interval_name(group: &[RecordIdentity]) -> Option<String> {
    let unit = group.first()?.product.time_unit_seconds;
    let lengths: Vec<i64> = group
        .iter()
        .map(|r| r.product.end - r.product.start)
        .collect();
    if lengths.windows(2).any(|w| w[0] != w[1]) {
        return Some("Mixed_intervals".to_string());
    }
    let (value, field) = match unit {
        60 => (lengths[0] / 60, "Minute"),
        3600 => (lengths[0] / 3600, "Hour"),
        86_400 => (lengths[0] / 86_400, "Day"),
        1 => (lengths[0], "Second"),
        _ => return None,
    };
    Some(format!("{value}_{field}"))
}

/// `makeVariableNameFromTable` and `makeVariableLongName`.
fn names(first: &RecordIdentity, group: &[RecordIdentity]) -> Option<(String, String)> {
    let p = &first.product;
    let level = LEVELS.iter().find(|l| l.0 == p.level_type1)?;
    let mut name = if first.entry.unknown_parameter {
        format!(
            // GribCollectionMutable.VariableIndex: tableVersion is -1 for GRIB2.
            "VAR{}-{}-{}_FROM_{}-{}--1",
            p.discipline, p.category, p.number, p.center, p.subcenter
        )
    } else {
        make_name(&first.entry.parameter)
    };
    let mut long = if first.entry.unknown_parameter {
        format!(
            "Unknown Parameter {}-{}-{}",
            p.discipline, p.category, p.number
        )
    } else {
        first.entry.parameter.clone()
    };
    // Generating types 6 and 7 are "error" variables.
    if p.generating_type == 6 || p.generating_type == 7 {
        name.push_str("_error");
    }
    name.push('_');
    name.push_str(level.1);
    if p.is_layer() {
        name.push_str("_layer");
    }
    if let Some(code) = p.statistic {
        let statistic = statistic_name(code)?;
        let interval = interval_name(group)?;
        name = format!("{name}_{interval}_{statistic}");
        long = format!("{long} ({interval} {statistic})");
    }
    if p.generating_type == 6 || p.generating_type == 7 {
        long.push_str(" error");
    }
    long.push_str(" @ ");
    long.push_str(level.2);
    if p.is_layer() {
        long.push_str(" layer");
    }
    Some((name, long))
}

/// `GribUtils.makeNameFromDescription`.
fn make_name(description: &str) -> String {
    let mut text = description;
    let at = text.find("(see").or_else(|| text.find("(See"));
    if let Some(at) = at.filter(|&a| a > 0) {
        text = &text[..at];
    }
    text.trim()
        .replace('+', "plus")
        .chars()
        .map(|c| match c {
            '/' | '.' | ' ' => match c {
                '/' => '-',
                '.' => 'p',
                _ => '_',
            },
            other => other,
        })
        .filter(|c| !";,=[]()".contains(*c))
        .collect()
}

/// `EnhanceScaleMissingUnsignedImpl.isMissing`: a value within 1e-5f of `missing_value` or
/// `_FillValue`, compared in `f64` by `Misc.nearlyEquals`, is missing. Fill and missing compare
/// even when, as some MRMS table rows say, both are 0.
pub fn apply_missing(entry: &TableEntry, cells: &mut [f32]) {
    const MAX_RELATIVE: f64 = 1.0e-5f32 as f64;
    use crate::source::grib::nearly_equals_f64;
    let mut targets = [f64::NAN; 2];
    if !entry.missing.is_nan() {
        targets[0] = f64::from(entry.missing);
    }
    if let Some(fill) = entry.fill {
        targets[1] = f64::from(fill);
    }
    if targets.iter().all(|t| t.is_nan()) {
        return;
    }
    for cell in cells.iter_mut() {
        let value = f64::from(*cell);
        if targets
            .iter()
            .any(|&t| !t.is_nan() && nearly_equals_f64(value, t, MAX_RELATIVE))
        {
            *cell = f32::NAN;
        }
    }
}

/// `GridDatasetReader.getUnits`.
pub fn units(variable: &Variable) -> String {
    if variable.name.to_lowercase().contains("var209-6") {
        return "mm".to_string();
    }
    variable.entry.unit.clone()
}

/// The window a frame covers, seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub start: i64,
    pub end: i64,
}

/// What to do when a file-name rule gives a record a window other than its own interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowRule {
    /// Refuse. This is the default: see the module's note on observed behaviour.
    #[default]
    Refuse,
    /// Accept the window, as Vortex reports it.
    Vortex,
    /// The record is a depth over this duration, ending at the file's time. Not Vortex's: see
    /// [`window`].
    Accumulation(window::Accumulation),
}

/// `GridDatasetReader.SpecialFileType` rule names that can match a GRIB file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecialFile {
    AbrscGauge,
    AbrscRadar,
    MrmsIdp,
    Gefs,
    HrrrWrfsfcf,
    Undefined,
    /// A rule Vortex has for another format, matched only by renaming. Refused here.
    Other,
}

/// `Pattern.matches` for a pattern of literals joined by `.*`, anchored at both ends. `.` does
/// not match line terminators, so a name holding one matches nothing.
fn matches_literals(name: &str, parts: &[&str]) -> bool {
    if name.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']) {
        return false;
    }
    let Some((first, rest)) = parts.split_first() else {
        return false;
    };
    let Some(mut remainder) = name.strip_prefix(first) else {
        return false;
    };
    for (i, part) in rest.iter().enumerate() {
        if i + 1 == rest.len() {
            return remainder.ends_with(part);
        }
        match remainder.find(part) {
            Some(at) => remainder = &remainder[at + part.len()..],
            None => return false,
        }
    }
    remainder.is_empty()
}

/// `GridDatasetReader.determineSpecialFileType`, first match wins, on the lower-cased name.
fn special_file(file_name: &str, variable: &str) -> SpecialFile {
    let name = file_name.to_lowercase();
    if matches_literals(&name, &["", "gaugecorr", "qpe", "01h", "grib2"]) {
        return SpecialFile::AbrscGauge;
    }
    if matches_literals(&name, &["", "radaronly", "qpe", "01h", "grib2"]) {
        return SpecialFile::AbrscRadar;
    }
    if matches_literals(&name, &["", "multisensor", "qpe", "01h", "grib2"]) {
        return SpecialFile::MrmsIdp;
    }
    // GPM, AORC and UA_SWE, LIVNEH are names of other formats. Reached by a renamed GRIB only.
    let has_digit_pair_nc = name.len() == 5
        && name.as_bytes()[..2].iter().all(u8::is_ascii_digit)
        && name.as_bytes()[2] != b'\n'
        && name.ends_with("nc");
    let livneh = name.len() == 12
        && name.starts_with("prec")
        && name.ends_with("nc")
        && name.as_bytes()[5..9].iter().all(u8::is_ascii_digit);
    if matches_literals(&name, &["", "hhr", "ms", "mrg", "hdf", ""])
        || matches_literals(&name, &["", "aorc", "apcp", "nc4", ""])
        || matches_literals(&name, &["", "aorc", "tmp", "nc4", ""])
        || has_digit_pair_nc
        || (matches_literals(&name, &["nldas_fora0125_h", "a", ""]) && variable == "APCP")
        || matches_literals(&name, &["", "cmorph", "h", "ly", ""])
    {
        return SpecialFile::Other;
    }
    if matches_literals(&name, &["ge", ".pgrb2", ".0p", ".f", ".", ""]) {
        return SpecialFile::Gefs;
    }
    if livneh {
        return SpecialFile::Other;
    }
    if matches_literals(&name, &["hrrr", "wrfsfcf", ""]) {
        return SpecialFile::HrrrWrfsfcf;
    }
    // GFS("gfs.nc", "Precipitation_rate_surface").
    if name == "gfs.nc" && variable == "Precipitation_rate_surface" {
        return SpecialFile::Other;
    }
    SpecialFile::Undefined
}

/// `GridDatasetReader.getTimeRecordsWithTimeAxis1D` and `adjustTimeForSpecialFile`.
pub fn time_window(
    file_name: &str,
    variable_name: &str,
    variable: &Variable,
    step: usize,
    rule: WindowRule,
) -> Result<TimeWindow, VortexError> {
    let refuse = |reason: String| VortexError::Refused {
        what: format!("time window of {file_name}"),
        reason,
    };
    let current = variable
        .steps
        .get(step)
        .ok_or_else(|| refuse(format!("step {step}")))?;
    let special = special_file(file_name, variable_name);
    if special == SpecialFile::Other {
        return Err(refuse(
            "the file name matches a Vortex window rule for another format".to_string(),
        ));
    }
    if let WindowRule::Accumulation(wanted) = rule {
        return window::accumulation_window(file_name, variable, step, wanted);
    }
    // Observed behaviour we did not implement by default: Vortex's file-name rule gives a 24H file renamed
    // `*_QPE_01H_*.grib2` one-hour stamps. WindowRule::Vortex returns Vortex's window.
    if rule == WindowRule::Refuse
        && matches!(
            special,
            SpecialFile::AbrscGauge | SpecialFile::AbrscRadar | SpecialFile::MrmsIdp
        )
    {
        window::refuse_one_hour_name_disagreement(file_name, variable_name)?;
    }
    if !variable.interval && special == SpecialFile::Undefined {
        // getTimeInstants.
        return Ok(TimeWindow {
            start: current.end,
            end: current.end,
        });
    }
    // getCoordBoundsDate on an instant axis of several points takes midpoints between coordinates
    // (CoordinateAxis1D.makeEdges), so an instant becomes a period.
    //
    // Observed behaviour we did not implement by default: on Vortex 1.1.0, four hourly MRMS 01H
    // messages in one file get two-hour windows, and hourly HRRR 2 m temperatures in one file become
    // PER-AVER windows from half past to half past. This library refuses that, and
    // WindowRule::Vortex returns Vortex's window.
    let several_instants = !variable.interval && variable.steps.len() > 1;
    if several_instants && rule == WindowRule::Refuse {
        return Err(refuse(
            "several instants under a file-name window rule. Observed behaviour we did not implement: \
             Vortex gives each a window between midpoints. Pass WindowRule::Vortex for that"
                .to_string(),
        ));
    }
    let (start, end) = if several_instants {
        midpoint_bounds(&variable.steps, step).ok_or_else(|| {
            refuse("midpoints between these instants are not whole seconds".to_string())
        })?
    } else {
        (current.start, current.end)
    };
    const HOUR: i64 = 3600;
    let adjusted_start = match special {
        SpecialFile::AbrscGauge | SpecialFile::AbrscRadar | SpecialFile::MrmsIdp => start - HOUR,
        _ => start,
    };
    let adjusted_end = match special {
        SpecialFile::HrrrWrfsfcf => adjusted_start + HOUR,
        SpecialFile::Gefs => adjusted_start + 3 * HOUR,
        _ => end,
    };
    if rule == WindowRule::Refuse && variable.interval && adjusted_end != end {
        return Err(refuse(format!(
            "the record covers {} s, and Vortex's file-name rule would report {} s; pass \
             WindowRule::Vortex for Vortex's window",
            end - start,
            adjusted_end - adjusted_start
        )));
    }
    Ok(TimeWindow {
        start: adjusted_start,
        end: adjusted_end,
    })
}

/// `CoordinateAxis1D.makeEdges`: halfway to each neighbour, and the end cells mirror their inner
/// edge. `None` when a midpoint is not a whole second, which has no reference here.
fn midpoint_bounds(steps: &[Step], step: usize) -> Option<(i64, i64)> {
    let at = |i: usize| steps.get(i).map(|s| s.end);
    let halfway = |a: i64, b: i64| ((a + b) % 2 == 0).then_some((a + b) / 2);
    let this = at(step)?;
    let start = match step.checked_sub(1).and_then(at) {
        Some(before) => halfway(before, this)?,
        None => this - (halfway(this, at(step + 1)?)? - this),
    };
    let end = match at(step + 1) {
        Some(after) => halfway(this, after)?,
        None => this + (this - halfway(at(step.checked_sub(1)?)?, this)?),
    };
    Some((start, end))
}

/// `VortexVariable.isValidPrecipitationName`, on `fromName`'s lower-cased, space-free name.
pub(crate) fn is_precipitation_name(name: &str) -> bool {
    let name: String = name
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let has = |part: &str| name.contains(part);
    matches!(
        name.as_str(),
        "precipitation"
            | "precip"
            | "precip-inc"
            | "precipitationcal"
            | "precipitationuncal"
            | "irprecipitation"
            | "mwprecipitation"
            | "var209-6"
            | "cmorph"
            | "rainfall"
            | "pcp"
            | "pr"
            | "prec"
            | "prcp"
    ) || has("qpe")
        || (has("precip") && has("rate"))
        || (has("precipitable") && has("water"))
        || (has("total") && has("precipitation"))
        || (has("convective") && has("rainfall"))
        || has("apcp")
}

/// `NetcdfDataReader.getVortexDataType` and `VortexGrid.dataType`.
pub fn data_type(variable: &Variable, window: &TimeWindow) -> DataType {
    match variable.statistic {
        // cell_methods "time: sum" and "time: mean"; VortexDataType.fromString.
        Some(1) => DataType::PerCum,
        Some(0) => DataType::PerAver,
        _ => {
            if window.start == window.end {
                DataType::InstVal
            } else if is_precipitation_name(&variable.name) {
                DataType::PerCum
            } else {
                DataType::PerAver
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_rules_match_as_vortex_does() {
        let f = |n: &str| special_file(n, "x");
        assert_eq!(
            f("MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240615-000000.grib2"),
            SpecialFile::MrmsIdp
        );
        assert_eq!(
            f("RadarOnly_QPE_01H_00.00_20210706-000000.grib2"),
            SpecialFile::AbrscRadar
        );
        assert_eq!(
            f("MRMS_GaugeCorr_QPE_01H_00.00_20170102-120000.grib2"),
            SpecialFile::AbrscGauge
        );
        assert_eq!(f("hrrr.t12z.wrfsfcf01.grib2"), SpecialFile::HrrrWrfsfcf);
        assert_eq!(f("gep01.t06z.pgrb2s.0p25.f000.grb2"), SpecialFile::Gefs);
        assert_eq!(f("st4_conus.2026103012.01h.grb2"), SpecialFile::Undefined);
        assert_eq!(f("MultiSensor_QPE_03H.grib2"), SpecialFile::Undefined);
        assert_eq!(f("MultiSensor_QPE_01H.grib2.gz"), SpecialFile::Undefined);
        assert_eq!(f("01.nc"), SpecialFile::Other);
    }

    fn accumulation(start: i64, end: i64) -> Variable {
        Variable {
            name: "Total_precipitation_surface_Mixed_intervals_Accumulation".to_string(),
            description: String::new(),
            entry: TableEntry {
                parameter: "Total precipitation".to_string(),
                unit: String::new(),
                fill: None,
                missing: f32::NAN,
                unknown_parameter: false,
            },
            statistic: Some(1),
            interval: true,
            steps: vec![Step {
                record: 0,
                start,
                end,
            }],
        }
    }

    #[test]
    fn a_window_other_than_the_record_interval_is_refused_by_default() {
        const NAME: &str = "hrrr.t06z.wrfsfcf06.grib2";
        let six_hours = accumulation(0, 6 * 3600);
        let error = time_window(NAME, "x", &six_hours, 0, WindowRule::default());
        assert!(matches!(error, Err(VortexError::Refused { .. })));
        let parity = time_window(NAME, "x", &six_hours, 0, WindowRule::Vortex).unwrap();
        assert_eq!(
            parity,
            TimeWindow {
                start: 0,
                end: 3600
            }
        );

        // One hour agrees with the rule, so it needs no opt-in.
        let one_hour = accumulation(5 * 3600, 6 * 3600);
        let window = time_window(NAME, "x", &one_hour, 0, WindowRule::default()).unwrap();
        assert_eq!(
            window,
            TimeWindow {
                start: 5 * 3600,
                end: 6 * 3600
            }
        );

        // No file-name rule: the record's own interval.
        let other = time_window("st4.06h.grb2", "x", &six_hours, 0, WindowRule::default()).unwrap();
        assert_eq!(
            other,
            TimeWindow {
                start: 0,
                end: 6 * 3600
            }
        );
    }

    #[test]
    fn names_follow_make_name_from_description() {
        assert_eq!(make_name("Total precipitation"), "Total_precipitation");
        assert_eq!(make_name("A+B (see note)"), "AplusB");
        assert_eq!(make_name("Mass; x=[1]"), "Mass_x1");
    }

    #[test]
    fn missing_values_compare_relative_as_netcdf_java_does() {
        let entry = TableEntry {
            parameter: String::new(),
            unit: String::new(),
            fill: Some(-3.0),
            missing: -1.0,
            unknown_parameter: false,
        };
        let mut cells = [-3.0, -3.00001, -3.1, -1.0, 0.0, 5.5, f32::NAN];
        apply_missing(&entry, &mut cells);
        assert!(cells[0].is_nan());
        assert!(cells[1].is_nan());
        assert_eq!(cells[2], -3.1);
        assert!(cells[3].is_nan());
        assert_eq!(cells[4], 0.0);
        assert_eq!(cells[5], 5.5);
        assert!(cells[6].is_nan());
    }
}

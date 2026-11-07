//! Names, units and data types as Vortex writes them to DSS.
//!
//! Sources, Vortex 1.1.0: `VortexVariable.fromName`, `util/UnitUtil.parse`,
//! `util/DssUtil.UNITS_TO_STRING` and `getGridInfo`, `convert/DataConverter.convert`,
//! `geo/RasterUtils.convert`, `io/DssDataWriter.write` and `getCPart`. Rules were checked against
//! Vortex's own output by `oracle/vortex/writer_cases.py`.
//!
//! `UnitUtil.parse` ends in `SimpleUnitFormat`, a general parser, for strings outside its table.
//! This module does not port that parser. It holds the strings the oracle was asked about, in
//! [`SIMPLE_FORMAT_UNITS`], and anything else is a typed refusal unless the caller states the units.
//!
//! Observed behaviour we did not implement:
//!
//! - `VortexGrid.fileName` is the path as given, and `getCPart` searches all of it. On Vortex 1.1.0,
//!   a variable with no recognised name in `rhodes/plain.grib2` is written as `HUMIDITY`, because
//!   the directory holds `rh`. This library searches the file's name only.
//! - When a grid's names match different variables, `VortexVariable.fromNames` takes the first of an
//!   unordered set. On Vortex 1.1.0, short name `Temperature`, description `Total precipitation` and
//!   units `m` were multiplied by 1000 as precipitation and written as `TEMPERATURE`. This library
//!   refuses a grid whose names disagree where that changes the conversion.

use crate::error::VortexError;
use crate::frame::{DataType, FrameTime};

/// `VortexVariable`, in `fromName`'s order, with its DSS C part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variable {
    Precipitation,
    Temperature,
    ShortwaveRadiation,
    LongwaveRadiation,
    CropCoefficient,
    StorageCapacity,
    PercolationRate,
    StorageCoefficient,
    MoistureDeficit,
    ImperviousArea,
    CurveNumber,
    ColdContent,
    ColdContentAti,
    MeltrateAti,
    LiquidWater,
    SnowWaterEquivalent,
    WaterContent,
    WaterPotential,
    Humidity,
    Windspeed,
    Pressure,
    PrecipitationFrequency,
    Albedo,
    Energy,
    SnowfallAccumulation,
    SnowDepth,
    SnowSublimation,
    SnowMelt,
}

impl Variable {
    /// `VortexVariable.getDssCPart`.
    pub fn dss_c_part(self) -> &'static str {
        match self {
            Variable::Precipitation => "PRECIPITATION",
            Variable::Temperature => "TEMPERATURE",
            Variable::ShortwaveRadiation => "RADIATION-SHORT",
            Variable::LongwaveRadiation => "RADIATION-LONG",
            Variable::CropCoefficient => "CROP COEFFICIENT",
            Variable::StorageCapacity => "STORAGE CAPACITY",
            Variable::PercolationRate => "PERCOLATION",
            Variable::StorageCoefficient => "STORAGE COEFFICIENT",
            Variable::MoistureDeficit => "MOISTURE DEFICIT",
            Variable::ImperviousArea => "IMPERVIOUS AREA",
            Variable::CurveNumber => "CURVE NUMBER",
            Variable::ColdContent => "COLD CONTENT",
            Variable::ColdContentAti => "COLD CONTENT ATI",
            Variable::MeltrateAti => "MELTRATE ATI",
            Variable::LiquidWater => "LIQUID WATER",
            Variable::SnowWaterEquivalent => "SWE",
            Variable::WaterContent => "WATER CONTENT",
            Variable::WaterPotential => "WATER POTENTIAL",
            Variable::Humidity => "HUMIDITY",
            Variable::Windspeed => "WINDSPEED",
            Variable::Pressure => "PRESSURE",
            Variable::PrecipitationFrequency => "PRECIPITATION-FREQUENCY",
            Variable::Albedo => "ALBEDO",
            Variable::Energy => "ENERGY",
            Variable::SnowfallAccumulation => "SNOWFALL ACCUMULATION",
            Variable::SnowDepth => "SNOW DEPTH",
            Variable::SnowSublimation => "SNOW SUBLIMATION",
            Variable::SnowMelt => "SNOW MELT",
        }
    }
}

// `VortexVariable.normalizeString`.
fn normalize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| !java_space(*c) && *c != '_')
        .collect()
}

// Java's `\s`: no Unicode classes.
fn java_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
}

// `Pattern.matches("long.*radiation")`. `.` stops at Java's line terminators; `\s` has already gone.
fn long_radiation(name: &str) -> bool {
    !name.contains(['\u{85}', '\u{2028}', '\u{2029}'])
        && name.len() >= "longradiation".len()
        && name.starts_with("long")
        && name.ends_with("radiation")
}

/// `VortexVariable.fromName`, in its order.
pub fn variable_from_name(name: &str) -> Option<Variable> {
    use Variable as V;
    if name.is_empty() {
        return None;
    }
    let n: String = name
        .to_lowercase()
        .chars()
        .filter(|c| !java_space(*c))
        .collect();
    let n = n.as_str();
    let has = |p: &str| n.contains(p);
    let dss = |v: Variable| normalize(n) == normalize(v.dss_c_part());
    let same = |text: &str| normalize(n) == normalize(text);

    let precipitation = n == "precipitation"
        || dss(V::Precipitation)
        || matches!(
            n,
            "precip"
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
        )
        || has("qpe")
        || (has("precip") && has("rate"))
        || (has("precipitable") && has("water"))
        || (has("total") && has("precipitation"))
        || (has("convective") && has("rainfall"))
        || has("apcp");
    if precipitation {
        return Some(V::Precipitation);
    }
    if dss(V::Temperature)
        || has("temperature")
        || matches!(n, "airtemp" | "tasmin" | "tasmax" | "temp" | "temp-air")
        || has("tmp")
    {
        return Some(V::Temperature);
    }
    if dss(V::ShortwaveRadiation)
        || (has("shortwave") && has("radiation"))
        || (has("solar") && has("radiation"))
        || has("dswrf")
    {
        return Some(V::ShortwaveRadiation);
    }
    if dss(V::LongwaveRadiation) || long_radiation(n) || has("dlwrf") {
        return Some(V::LongwaveRadiation);
    }
    if dss(V::CropCoefficient) || (has("crop") && has("coefficient")) {
        return Some(V::CropCoefficient);
    }
    if dss(V::StorageCapacity) || (has("storage") && has("capacity")) {
        return Some(V::StorageCapacity);
    }
    // `percolation\s?rate`: spaces are gone by here.
    if dss(V::PercolationRate) || n == "percolation" || n == "percolationrate" {
        return Some(V::PercolationRate);
    }
    if dss(V::StorageCoefficient) || (has("storage") && has("coefficient")) {
        return Some(V::StorageCoefficient);
    }
    if dss(V::MoistureDeficit) || n == "moisturedeficit" {
        return Some(V::MoistureDeficit);
    }
    if dss(V::ImperviousArea) || n == "imperviousarea" {
        return Some(V::ImperviousArea);
    }
    if dss(V::CurveNumber) || n == "curvenumber" {
        return Some(V::CurveNumber);
    }
    if dss(V::ColdContent) || same("cold content") {
        return Some(V::ColdContent);
    }
    if dss(V::ColdContentAti) || same("cold content ati") {
        return Some(V::ColdContentAti);
    }
    if dss(V::MeltrateAti) || same("meltrate ati") {
        return Some(V::MeltrateAti);
    }
    if dss(V::LiquidWater) || (has("snow") && has("melt") && has("runoff")) || same("liquid water")
    {
        return Some(V::LiquidWater);
    }
    if dss(V::SnowWaterEquivalent)
        || (has("snow") && has("water") && has("equivalent"))
        || n == "swe"
        || n == "weasd"
    {
        return Some(V::SnowWaterEquivalent);
    }
    if dss(V::WaterContent) || (has("water") && has("content")) {
        return Some(V::WaterContent);
    }
    if dss(V::WaterPotential) || (has("water") && has("potential")) {
        return Some(V::WaterPotential);
    }
    if dss(V::Humidity) || has("humidity") || has("rh") {
        return Some(V::Humidity);
    }
    if dss(V::Windspeed) || has("wind") {
        return Some(V::Windspeed);
    }
    if dss(V::Pressure) || has("pressure") || has("pres") {
        return Some(V::Pressure);
    }
    if dss(V::PrecipitationFrequency) || (has("precipitation") && has("frequency")) {
        return Some(V::PrecipitationFrequency);
    }
    if dss(V::Albedo) || has("albedo") {
        return Some(V::Albedo);
    }
    if dss(V::Energy) || n == "energy" {
        return Some(V::Energy);
    }
    if dss(V::SnowfallAccumulation) || (has("snowfall") && has("accumulation")) {
        return Some(V::SnowfallAccumulation);
    }
    if dss(V::SnowDepth) || (has("snow") && has("depth")) {
        return Some(V::SnowDepth);
    }
    if dss(V::SnowSublimation) || (has("snow") && has("sublimation")) {
        return Some(V::SnowSublimation);
    }
    if dss(V::SnowMelt) || same("snow melt") {
        return Some(V::SnowMelt);
    }
    None
}

/// Which of a grid's names gave its C part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CPartSource {
    ShortName,
    Description,
    FileName,
    /// No name matched a variable, so the short name in upper case.
    ShortNameUpper,
}

impl CPartSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CPartSource::ShortName => "short_name",
            CPartSource::Description => "description",
            CPartSource::FileName => "file_name",
            CPartSource::ShortNameUpper => "short_name_upper",
        }
    }
}

// `DssDataWriter.getCPartForGrid`. `Some` with a variable's C part means `isStandardCPart`, unless
// upper-casing a name happens to give one, which is handled below.
fn c_part_for(name: Option<&str>) -> (String, bool) {
    let Some(name) = name else {
        return (String::new(), false);
    };
    if let Some(v) = variable_from_name(name) {
        return (v.dss_c_part().to_string(), true);
    }
    let upper = name.to_uppercase();
    let standard = is_standard_c_part(&upper);
    (upper, standard)
}

// `DssDataWriter.isStandardCPart`.
fn is_standard_c_part(part: &str) -> bool {
    ALL_VARIABLES.iter().any(|v| v.dss_c_part() == part)
}

const ALL_VARIABLES: [Variable; 28] = [
    Variable::Precipitation,
    Variable::Temperature,
    Variable::ShortwaveRadiation,
    Variable::LongwaveRadiation,
    Variable::CropCoefficient,
    Variable::StorageCapacity,
    Variable::PercolationRate,
    Variable::StorageCoefficient,
    Variable::MoistureDeficit,
    Variable::ImperviousArea,
    Variable::CurveNumber,
    Variable::ColdContent,
    Variable::ColdContentAti,
    Variable::MeltrateAti,
    Variable::LiquidWater,
    Variable::SnowWaterEquivalent,
    Variable::WaterContent,
    Variable::WaterPotential,
    Variable::Humidity,
    Variable::Windspeed,
    Variable::Pressure,
    Variable::PrecipitationFrequency,
    Variable::Albedo,
    Variable::Energy,
    Variable::SnowfallAccumulation,
    Variable::SnowDepth,
    Variable::SnowSublimation,
    Variable::SnowMelt,
];

/// `DssDataWriter.getCPart`, with `file_name` the file's name and not its path (see the module's
/// note). Returns the C part and which name gave it.
pub fn c_part(short: &str, description: Option<&str>, file_name: &str) -> (String, CPartSource) {
    let (from_short, standard) = c_part_for(Some(short));
    if !from_short.trim().is_empty() && standard {
        return (from_short, CPartSource::ShortName);
    }
    let (from_description, standard) = c_part_for(description);
    if !from_description.trim().is_empty() && standard {
        return (from_description, CPartSource::Description);
    }
    let (from_file, standard) = c_part_for(Some(file_name));
    if !from_file.trim().is_empty() && standard {
        return (from_file, CPartSource::FileName);
    }
    (from_short, CPartSource::ShortNameUpper)
}

/// What `UnitUtil.parse` gives, as far as conversions and DSS strings need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// `MILLI(METRE)`.
    Millimetre,
    /// `METRE`.
    Metre,
    /// `MILLI(METRE).divide(SECOND)`.
    MmPerSecond,
    /// `MILLI(METRE).divide(HOUR)`.
    MmPerHour,
    /// `MILLI(METRE).divide(DAY)`.
    MmPerDay,
    /// `KELVIN`.
    Kelvin,
    /// `CELSIUS`.
    Celsius,
    /// `FAHRENHEIT`.
    Fahrenheit,
    /// `PASCAL`.
    Pascal,
    /// `AbstractUnit.ONE`.
    One,
    /// Any other unit, with what `DssUtil.getUnitsString` gives for it.
    Other(&'static str),
}

impl Unit {
    /// `DssUtil.getUnitsString`.
    pub fn dss_string(self) -> &'static str {
        match self {
            Unit::Millimetre => "MM",
            Unit::Metre => "M",
            Unit::MmPerSecond => "MM/S",
            Unit::MmPerHour => "MM/HR",
            Unit::MmPerDay => "MM/DAY",
            Unit::Kelvin => "K",
            Unit::Celsius => "DEG C",
            Unit::Fahrenheit => "DEG F",
            Unit::Pascal => "PA",
            Unit::One => "UNDEF",
            Unit::Other(s) => s,
        }
    }

    /// `units.isCompatible(CELSIUS)`.
    pub fn celsius_compatible(self) -> bool {
        matches!(self, Unit::Kelvin | Unit::Celsius | Unit::Fahrenheit)
    }
}

/// `SimpleUnitFormat` results for strings outside `UnitUtil.parse`'s table, read from Vortex 1.1.0
/// by `oracle/vortex/writer_cases.py` (case-sensitive, as that parser is). Strings Vortex parses to
/// nothing come out as `ONE`, so `UNDEF`. Parses whose DSS string is not ASCII are left out, so they
/// are refused: the writer stores ASCII.
const SIMPLE_FORMAT_UNITS: &[(&str, Unit)] = &[
    ("", Unit::One),
    // Packet P46b: unit strings of real NetCDF products (ERA-40, IMERG, GPCP), each stored as UNDEF
    // with no conversion by Vortex 1.1.0 (`oracle/vortex/nc_refused_on_vortex.py`, then compared in
    // `run_import.py --netcdf`).
    ("(0 - 1)", Unit::One),
    ("count", Unit::One),
    ("kg m**-2", Unit::One),
    ("m of water", Unit::One),
    ("m s**-1", Unit::One),
    ("millimeters/day", Unit::One),
    ("minutes", Unit::One),
    ("-", Unit::One),
    ("0.001/s", Unit::One),
    ("1", Unit::One),
    ("1/(1000 in)", Unit::One),
    ("1/(1000*in)", Unit::One),
    ("1/1000in", Unit::One),
    ("A", Unit::Other("A")),
    ("Hz", Unit::Other("Hz")),
    ("J", Unit::Other("J")),
    ("J m-2", Unit::One),
    ("J.kg-1", Unit::One),
    ("K/m", Unit::Other("K/m")),
    ("N", Unit::Other("N")),
    ("Numeric", Unit::One),
    ("Proportion", Unit::One),
    ("V", Unit::Other("V")),
    ("W", Unit::Other("W")),
    ("W.m-2", Unit::One),
    ("dB", Unit::One),
    ("dBZ", Unit::One),
    ("dBz", Unit::One),
    ("db", Unit::One),
    ("dbZ", Unit::One),
    ("degree", Unit::One),
    ("degree_Celsius", Unit::One),
    ("dimensionless", Unit::One),
    ("flag", Unit::One),
    ("flashes/km^2/min", Unit::One),
    ("fraction", Unit::One),
    ("g", Unit::Other("g")),
    ("g/kg", Unit::Other("g/kg")),
    ("gpm", Unit::One),
    ("h", Unit::Other("HR")),
    ("index", Unit::One),
    ("kft", Unit::One),
    ("kg", Unit::Other("kg")),
    ("kg kg-1", Unit::One),
    ("kg/kg", Unit::One),
    ("lm", Unit::Other("lm")),
    ("m**2 s**-2", Unit::One),
    ("m-1", Unit::One),
    ("m.s-1", Unit::One),
    ("m^3/s", Unit::Other("M3/S")),
    ("mm/h", Unit::MmPerHour),
    ("mol", Unit::Other("mol")),
    ("non-dim", Unit::One),
    ("none", Unit::One),
    ("percent", Unit::One),
    ("s", Unit::Other("s")),
    ("s-1", Unit::One),
    ("sec", Unit::One),
    ("seconds", Unit::One),
    ("unitless", Unit::One),
    ("year", Unit::Other("year")),
];

/// `UnitUtil.parse`. `None` for a string this library has no reviewed answer for. A `None` input
/// is `ONE`, as there.
pub fn parse_unit(text: Option<&str>) -> Option<Unit> {
    let Some(text) = text else {
        return Some(Unit::One);
    };
    let key = java_trim(text).to_lowercase();
    let unit = match key.as_str() {
        "kg.m-2"
        | "kg/m^2"
        | "kg m^-2"
        | "kg m-2"
        | "mm"
        | "millimeter"
        | "millimeters"
        | "millimeters h20"
        | "millimeters snow thickness" => Unit::Millimetre,
        "m" | "meter" | "metre" | "meters" => Unit::Metre,
        "km" => Unit::Other("KM"),
        "1/1000 in" => Unit::Other("in/1000"),
        "in" | "inch" | "inches" => Unit::Other("IN"),
        "ft" | "foot" | "feet" => Unit::Other("FT"),
        "mile" | "mi" => Unit::Other("MILE"),
        "m3/s" | "cms" => Unit::Other("M3/S"),
        "cfs" => Unit::Other("CFS"),
        "kg.m-2.s-1" | "kg m-2 s-1" | "kg/m2s" | "mm/s" | "mm s-1" => Unit::MmPerSecond,
        "mm hr^-1" | "mm/hr" => Unit::MmPerHour,
        "mm/day" | "mm/d" => Unit::MmPerDay,
        "m s-1" | "m/s" => Unit::Other("M/S"),
        "kph" => Unit::Other("KPH"),
        "in/hr" => Unit::Other("IN/HR"),
        "in/day" => Unit::Other("IN/DAY"),
        "ft/s" | "feet/s" | "ft s-1" => Unit::Other("FT/S"),
        "mph" | "mile/hr" | "mile hr^-1" => Unit::Other("MPH"),
        "kelvin" | "k" => Unit::Kelvin,
        "celsius" | "degrees c" | "deg c" | "deg_c" | "degc" | "c" => Unit::Celsius,
        "fahrenheit" | "deg f" | "deg_f" | "degf" | "f" => Unit::Fahrenheit,
        "degc-d" => Unit::Other("DEGC-D"),
        "degf-d" => Unit::Other("DEGF-D"),
        "mm/deg-d" => Unit::Other("MM/DEG-D"),
        "in/deg-d" => Unit::Other("IN/DEG-D"),
        "j m**-2" | "j/m2" => Unit::Other("J/M2"),
        "btu/ft2" => Unit::Other("BTU/FT2"),
        "watt/m2" | "w m-2" | "w/m2" => Unit::Other("WATT/M2"),
        "lang/min" => Unit::Other("LANG/MIN"),
        "hpa" => Unit::Other("hPa"),
        "kpa" => Unit::Other("KPA"),
        "pa" => Unit::Pascal,
        "inhg" | "in hg" | "in-hg" => Unit::Other("IN HG"),
        "min" => Unit::Other("MINUTES"),
        "hr" | "hour" | "hours" => Unit::Other("HR"),
        "degrees" | "degrees_east" | "degrees_north" => Unit::Other("rev/360"),
        "ac" | "acre" => Unit::Other("ACRE"),
        "sqft" | "ft2" => Unit::Other("SQFT"),
        "m2" => Unit::Other("M2"),
        "thou m2" => Unit::Other("THOU M2"),
        "m3" => Unit::Other("M3"),
        "thou m3" => Unit::Other("THOU M3"),
        "ac-ft" => Unit::Other("AC-FT"),
        "tons" => Unit::Other("TONS"),
        "tonnes" => Unit::Other("TONNES"),
        "mg/l" => Unit::Other("MG/L"),
        "%" => Unit::Other("%"),
        "unspecif" | "undef" => Unit::One,
        _ => {
            // parseSimpleUnitFormat: the string as given, and blank is ONE.
            if text.chars().all(char::is_whitespace) {
                return Some(Unit::One);
            }
            return SIMPLE_FORMAT_UNITS
                .iter()
                .find(|(s, _)| *s == text)
                .map(|(_, u)| *u);
        }
    };
    Some(unit)
}

// `String.trim`: every char up to and including U+0020, at both ends.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// What `DataConverter.convert` does to one grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Conversion {
    None,
    /// `RasterUtils.convert`: `value * factor`, in `f32`.
    Scale {
        factor: f32,
        units: Unit,
    },
    /// `data[i] + KELVIN_TO_CELSIUS`, in `f32`, to `celsius`.
    KelvinToCelsius,
}

impl Conversion {
    /// The units the converted grid states.
    fn units(self, before: Unit) -> Unit {
        match self {
            Conversion::None => before,
            Conversion::Scale { units, .. } => units,
            Conversion::KelvinToCelsius => Unit::Celsius,
        }
    }

    /// Convert one valid cell. Missing cells are decided before this, on the source's own values
    /// (`sink_dss::to_stored_cells`), so a converted value equal to the source's marker stays a value.
    pub fn convert(self, value: f32) -> f32 {
        match self {
            Conversion::None => value,
            Conversion::Scale { factor, .. } => value * factor,
            // KELVIN_TO_CELSIUS = -273.15f.
            Conversion::KelvinToCelsius => value + -273.15f32,
        }
    }
}

/// `DataConverter.getPrecipRateConversionFactor`, then the branches of `convert` in order.
/// `1/(1000 in)` has a branch in the source, but no string reaches it: `1/1000 in` parses to
/// `in/1000`, so neither converts, as `writer_cases.py` shows.
fn conversion_for(variable: Option<Variable>, unit: Unit, time: &FrameTime) -> Conversion {
    let precipitation = variable == Some(Variable::Precipitation);
    let interval_seconds = match time {
        FrameTime::Period { start, end } => Some((end.0 - start.0) * 86_400 + end.1 - start.1),
        _ => None,
    };
    let to_mm = |factor: f32| Conversion::Scale {
        factor,
        units: Unit::Millimetre,
    };
    match (unit, interval_seconds) {
        (Unit::MmPerSecond, Some(s)) if precipitation => to_mm(s as f32),
        (Unit::MmPerHour, Some(s)) if precipitation => to_mm(s as f32 / 3600.0),
        (Unit::MmPerDay, Some(s)) if precipitation => to_mm(s as f32 / 86_400.0),
        (Unit::Metre, _) if precipitation => to_mm(1000.0),
        (Unit::One, _) if variable == Some(Variable::Humidity) => Conversion::Scale {
            factor: 100.0,
            units: Unit::Other("%"),
        },
        (Unit::Kelvin, _) => Conversion::KelvinToCelsius,
        (Unit::Pascal, _) => Conversion::Scale {
            factor: 1.0e-3,
            units: Unit::Other("KPA"),
        },
        _ => Conversion::None,
    }
}

/// `VortexVariable.fromGrid` for a conversion: the variable the names agree on. Names that match
/// different variables are refused where the variable changes what `convert` does.
fn conversion_of(
    short: &str,
    description: Option<&str>,
    unit: Unit,
    time: &FrameTime,
) -> Result<(Option<Variable>, Conversion), VortexError> {
    let a = variable_from_name(short);
    let b = description.and_then(variable_from_name);
    let conversion_a = conversion_for(a.or(b), unit, time);
    let conversion_b = conversion_for(b.or(a), unit, time);
    if conversion_a != conversion_b {
        return Err(VortexError::Refused {
            what: format!("units of {short}"),
            reason: format!(
                "its names match {:?} and {:?}, which convert {} differently. Observed behaviour we \
                 did not implement: Vortex takes the first of an unordered set",
                a,
                b,
                unit.dss_string()
            ),
        });
    }
    Ok((a.or(b), conversion_a))
}

// Whether a caller's C part says precipitation. The pathname is what HEC-HMS and a reader see, so the
// instant rule looks at it as well as at the source's own name. DSS6 stores it in upper case.
fn c_part_says_precipitation(part: &str) -> bool {
    let upper = part.trim().to_uppercase();
    if upper.starts_with(Variable::PrecipitationFrequency.dss_c_part()) {
        return false;
    }
    upper.starts_with(Variable::Precipitation.dss_c_part())
        || upper == "PRECIP-CUM"
        || upper.starts_with("RAIN")
        || variable_from_name(part) == Some(Variable::Precipitation)
}

/// A record's data type and units, from what `DssUtil.getGridInfo` and `DssDataWriter.write` do.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordMeaning {
    pub c_part: String,
    pub c_part_source: CPartSource,
    pub conversion: Conversion,
    pub data_type: DataType,
    pub units: String,
}

/// What the caller states over the source (`writeOptions`).
#[derive(Debug, Clone, Default)]
pub struct Overrides<'a> {
    /// Replaces the C part in the pathname. The data type still follows the derived C part, as it
    /// does in `DssDataWriter.write`.
    pub c_part: Option<&'a str>,
    /// Relabels the units, and does not convert them (`DssDataWriter.write`, `setDataUnits`).
    pub units: Option<&'a str>,
    pub data_type: Option<DataType>,
}

/// Decide what one frame becomes. Metadata only, so a run can check it before any write.
pub fn record_meaning(
    short: &str,
    description: Option<&str>,
    file_name: &str,
    source_units: &str,
    time: &FrameTime,
    overrides: &Overrides,
) -> Result<RecordMeaning, VortexError> {
    if let (FrameTime::Instant(_), Some(DataType::PerAver | DataType::PerCum)) =
        (time, overrides.data_type)
    {
        return Err(VortexError::Refused {
            what: format!("data type of {short}"),
            reason: "a period type needs a period, and this frame is an instant. Give the frame an \
                     explicit (start, end) window, or use an instantaneous type. Observed behaviour \
                     we did not implement: Vortex 1.1.0 stores the instant as D and E parts that are \
                     equal"
                .into(),
        });
    }
    let parsed = parse_unit(Some(source_units));
    let (unit, conversion) = match parsed {
        Some(unit) => {
            let (_, conversion) = conversion_of(short, description, unit, time)?;
            (Some(unit), conversion)
        }
        // Only a stated label makes this writable; nothing is converted.
        None if overrides.units.is_some() => (None, Conversion::None),
        None => {
            return Err(VortexError::Refused {
                what: format!("units {source_units:?} of {short}"),
                reason: "no reviewed DSS string for them; state units= to label the record".into(),
            })
        }
    };
    let (derived, c_part_source) = c_part(short, description, file_name);
    if derived.trim().is_empty() && overrides.c_part.is_none() {
        return Err(VortexError::Refused {
            what: "C part".into(),
            reason: "the source gives no variable name; state the C part".into(),
        });
    }
    // The converted grid's units: `UnitUtil.parse` again in getGridInfo.
    let after = unit.map(|u| conversion.units(u));
    let celsius_compatible = after.is_some_and(Unit::celsius_compatible);
    let is_precipitation = derived == Variable::Precipitation.dss_c_part();
    // Either name is enough: a recognised source renamed by the caller is still a depth, and a final
    // C part of PRECIPITATION is read as one whatever the source called it.
    let says_precipitation =
        is_precipitation || overrides.c_part.is_some_and(c_part_says_precipitation);
    if says_precipitation && matches!(time, FrameTime::Instant(_)) && overrides.data_type.is_none()
    {
        return Err(VortexError::Refused {
            what: format!("time of {short}"),
            reason:
                "precipitation at an instant states no period, so its D and E parts cannot say \
                     when the depth fell. Give the frame an explicit (start, end) window; for GRIB2 \
                     state window_rule accumulation:<duration>; or state data_type INST-VAL. A source that is not a rain depth, such as a FLASH product, needs window_rule vortex with data_type INST-VAL to keep Vortex's record. Observed behaviour we did not implement: Vortex 1.1.0 and \
                     0.13.3 store it as INST-VAL, with a D part only"
                    .into(),
        });
    }
    let default_type = match time {
        // GridInfo's default stays; `options.dataType` is set for other C parts only.
        FrameTime::Invariant if is_precipitation => DataType::PerAver,
        FrameTime::Invariant | FrameTime::Instant(_) => DataType::InstVal,
        FrameTime::Period { .. } if !is_precipitation || celsius_compatible => DataType::PerAver,
        FrameTime::Period { .. } => DataType::PerCum,
    };
    let units = match (overrides.units, after) {
        (Some(label), _) => label.to_string(),
        (None, Some(u)) => u.dss_string().to_string(),
        (None, None) => String::new(),
    };
    Ok(RecordMeaning {
        c_part: overrides.c_part.map_or(derived, str::to_string),
        c_part_source,
        conversion,
        data_type: overrides.data_type.unwrap_or(default_type),
        units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precipitation_at_an_instant_needs_a_stated_type() {
        let instant = FrameTime::Instant((45000, 3600));
        let none = Overrides {
            c_part: None,
            units: None,
            data_type: None,
        };
        let e = record_meaning("APCP_surface", None, "", "kg/m^2", &instant, &none).unwrap_err();
        assert!(e.to_string().contains("precipitation at an instant"), "{e}");
        let stated = Overrides {
            data_type: Some(DataType::InstVal),
            ..none
        };
        let m = record_meaning("APCP_surface", None, "", "kg/m^2", &instant, &stated).unwrap();
        assert_eq!(m.data_type, DataType::InstVal);
        // Other variables keep the instant, and precipitation over a period stays PER-CUM.
        let t = record_meaning("TMP_2maboveground", None, "", "K", &instant, &none).unwrap();
        assert_eq!(t.data_type, DataType::InstVal);
        let p = record_meaning("APCP_surface", None, "", "kg/m^2", &period(), &none).unwrap();
        assert_eq!(p.data_type, DataType::PerCum);
    }

    fn period() -> FrameTime {
        FrameTime::Period {
            start: (45000, 3600),
            end: (45000, 7200),
        }
    }

    #[test]
    fn c_parts_follow_get_c_part() {
        let (c, s) = c_part(
            "MultiSensor_QPE_01H_Pass2_altitude_above_msl",
            None,
            "a.grib2",
        );
        assert_eq!((c.as_str(), s), ("PRECIPITATION", CPartSource::ShortName));
        let (c, s) = c_part("zzz", Some("Total precipitation stuff"), "a.grib2");
        assert_eq!((c.as_str(), s), ("PRECIPITATION", CPartSource::Description));
        let (c, s) = c_part("zzz", Some("yyy"), "temperature_file.grib2");
        assert_eq!((c.as_str(), s), ("TEMPERATURE", CPartSource::FileName));
        let (c, s) = c_part("Composite_reflectivity_entire_atmosphere", None, "x.grib2");
        assert_eq!(c, "COMPOSITE_REFLECTIVITY_ENTIRE_ATMOSPHERE");
        assert_eq!(s, CPartSource::ShortNameUpper);
    }

    #[test]
    fn humidity_matches_any_rh_in_a_name_and_the_path_is_not_searched() {
        assert_eq!(variable_from_name("rhodes"), Some(Variable::Humidity));
        // The directory in Vortex's fileName does not reach this function.
        let (c, s) = c_part("zzz", Some("yyy"), "plain.grib2");
        assert_eq!((c.as_str(), s), ("ZZZ", CPartSource::ShortNameUpper));
    }

    #[test]
    fn conversions_use_f32_arithmetic() {
        let m = record_meaning(
            "Precip",
            None,
            "",
            "mm/hr",
            &period(),
            &Overrides::default(),
        )
        .unwrap();
        assert_eq!(
            m.conversion,
            Conversion::Scale {
                factor: 1.0,
                units: Unit::Millimetre
            }
        );
        assert_eq!(m.units, "MM");
        let k = record_meaning(
            "Temperature",
            None,
            "",
            "K",
            &period(),
            &Overrides::default(),
        )
        .unwrap();
        assert_eq!(k.units, "DEG C");
        assert_eq!(k.data_type, DataType::PerAver);
        assert_eq!(
            k.conversion.convert(300.0).to_bits(),
            (300.0f32 + -273.15f32).to_bits()
        );
    }

    #[test]
    fn names_that_disagree_are_refused_only_where_conversion_changes() {
        let t = period();
        let o = Overrides::default();
        let e = record_meaning("Temperature", Some("Total precipitation"), "", "m", &t, &o);
        assert!(matches!(e, Err(VortexError::Refused { .. })));
        // Kelvin converts either way.
        assert!(
            record_meaning("Temperature", Some("Total precipitation"), "", "K", &t, &o).is_ok()
        );
    }

    #[test]
    fn the_final_c_part_alone_makes_an_instant_precipitation() {
        let t = FrameTime::Instant((45000, 3600));
        for part in [
            "PRECIPITATION",
            "precipitation",
            " Precipitation ",
            "PRECIP",
            "PRECIP-INC",
            "PRECIP-CUM",
            "RAINFALL",
            "RAIN",
        ] {
            let o = Overrides {
                c_part: Some(part),
                ..Overrides::default()
            };
            let e = record_meaning("unknown_depth", None, "", "mm", &t, &o).unwrap_err();
            assert!(e.to_string().contains("precipitation at an instant"), "{e}");
            let stated = Overrides {
                data_type: Some(DataType::InstVal),
                ..o
            };
            let m = record_meaning("unknown_depth", None, "", "mm", &t, &stated).unwrap();
            assert_eq!(m.data_type, DataType::InstVal);
        }
        // A recognised source keeps the rule under a C part of the caller's own.
        let o = Overrides {
            c_part: Some("MY-C"),
            ..Overrides::default()
        };
        assert!(record_meaning("APCP_surface", None, "", "mm", &t, &o).is_err());
        // Negative controls: these stay instants.
        for part in [
            "PRECIPITATION-FREQUENCY",
            "PRECIPFLAG",
            "TEMPERATURE",
            "MY-C",
        ] {
            let o = Overrides {
                c_part: Some(part),
                ..Overrides::default()
            };
            let m = record_meaning("unknown_depth", None, "", "mm", &t, &o).unwrap();
            assert_eq!(m.data_type, DataType::InstVal, "{part}");
        }
    }

    #[test]
    fn a_period_type_on_an_instant_is_refused() {
        let t = FrameTime::Instant((45000, 3600));
        for data_type in [DataType::PerCum, DataType::PerAver] {
            let o = Overrides {
                data_type: Some(data_type),
                ..Overrides::default()
            };
            let e = record_meaning("Precip", None, "", "mm", &t, &o);
            assert!(matches!(e, Err(VortexError::Refused { .. })), "{e:?}");
        }
    }

    #[test]
    fn an_unreviewed_unit_needs_a_stated_label() {
        let t = period();
        let e = record_meaning("Precip", None, "", "furlongs", &t, &Overrides::default());
        assert!(matches!(e, Err(VortexError::Refused { .. })));
        let o = Overrides {
            units: Some("MM"),
            ..Overrides::default()
        };
        assert_eq!(
            record_meaning("Precip", None, "", "furlongs", &t, &o)
                .unwrap()
                .units,
            "MM"
        );
    }
}

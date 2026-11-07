//! Which variables are axes and which coordinate system each data variable has.
//!
//! Sources, NetCDF-Java 5.5.3: `CoordSystemFactory.factory`, `CoordSystemBuilder`,
//! `CoardsConventions`, `CSMConvention`, `CF1Convention`, `DefaultConventions`,
//! `GridCoordSys.isGridCoordSys`. Other convention builders are refused, since no sample has one.

use super::model::{attr_string, find_attr_ignore_case, string_attr, units, Dim, Model, Var};
use super::refused;
use super::time;
use crate::error::VortexError;
use netcdf_reader::NcType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Convention {
    Cf,
    Coards,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AxisType {
    Lat,
    Lon,
    GeoX,
    GeoY,
    GeoZ,
    Height,
    Pressure,
    Time,
    RunTime,
    TimeOffset,
    Ensemble,
}

/// Attributes that drive `CoordSystemBuilder` directly. Only the axis type is modelled.
const COORDINATE_ATTRIBUTES: [&str; 8] = [
    "_CoordinateAxes",
    "_CoordinateSystems",
    "_CoordinateSystemFor",
    "_CoordinateTransforms",
    "_CoordinateTransformType",
    "_CoordinateAxisTypes",
    "_CoordinateZisPositive",
    "_CoordinateAliasForDimension",
];

/// Global attributes that select another builder through its `isMine` test when `Conventions` is
/// absent. Those builders have no sample and no reference here.
const OTHER_BUILDER_MARKERS: [&str; 7] = [
    "MAP_PROJ",
    "IOAPI_VERSION",
    "GRIDTYPE",
    "ADASVersion",
    "DataType",
    "GRID_ID",
    "XORIG",
];

pub(super) fn convention(model: &Model) -> Result<Convention, VortexError> {
    let name = string_attr(&model.root_attrs, "Conventions")
        .or_else(|| string_attr(&model.root_attrs, "Convention"))
        .map(|s| s.trim().to_string());
    let Some(name) = name else {
        if let Some(marker) = OTHER_BUILDER_MARKERS
            .iter()
            .find(|m| model.root_attrs.iter().any(|a| a.name == **m))
        {
            return Err(refused(
                "NetCDF conventions",
                format!("global attribute {marker} selects a builder with no reference here"),
            ));
        }
        return Ok(Convention::Default);
    };
    // CoordSystemFactory.breakupConventionNames.
    let names: Vec<&str> = if name.find([',', ';']).is_some_and(|at| at > 0) {
        name.split([',', ';']).map(str::trim).collect()
    } else if name.find('/').is_some_and(|at| at > 0) {
        name.split('/').map(str::trim).collect()
    } else {
        vec![name.as_str()]
    };
    let is_cf = |n: &str| n.starts_with("CF-1.") || n == "CDM-Extended-CF";
    if names.iter().any(|n| is_cf(n)) && names.iter().any(|n| n.eq_ignore_ascii_case("COARDS")) {
        return Err(refused(
            "NetCDF conventions",
            format!("{name:?} names both CF and COARDS; no sample has both"),
        ));
    }
    if names.iter().any(|n| is_cf(n)) {
        return Ok(Convention::Cf);
    }
    if names.iter().any(|n| n.eq_ignore_ascii_case("COARDS")) {
        return Ok(Convention::Coards);
    }
    Err(refused(
        "NetCDF conventions",
        format!("{name:?} has no reference here"),
    ))
}

/// A subset of `SimpleUnit.isCompatible("m", unit)`: the length units the coordinate rules meet.
/// An unlisted unit reads as not a length, and an axis that then has no type is left out, which the
/// gate would catch as a missing variable.
fn is_length(unit: &str) -> bool {
    matches!(
        unit.trim().to_ascii_lowercase().as_str(),
        "m" | "meter"
            | "meters"
            | "metre"
            | "metres"
            | "km"
            | "kilometer"
            | "kilometers"
            | "kilometre"
            | "kilometres"
            | "ft"
            | "foot"
            | "feet"
    )
}

/// A subset of `SimpleUnit.isCompatible("mbar", unit)`.
fn is_pressure(unit: &str) -> bool {
    matches!(
        unit.trim().to_ascii_lowercase().as_str(),
        "mbar" | "millibar" | "millibars" | "mb" | "hpa" | "pa" | "pascal" | "pascals" | "bar"
    )
}

fn is_east(u: &str) -> bool {
    [
        "degrees_east",
        "degrees_E",
        "degreesE",
        "degree_east",
        "degree_E",
        "degreeE",
    ]
    .iter()
    .any(|x| u.eq_ignore_ascii_case(x))
}

fn is_north(u: &str) -> bool {
    [
        "degrees_north",
        "degrees_N",
        "degreesN",
        "degree_north",
        "degree_N",
        "degreeN",
    ]
    .iter()
    .any(|x| u.eq_ignore_ascii_case(x))
}

/// `CoardsConventions.getAxisType`.
fn axis_type_coards(var: &Var) -> Option<AxisType> {
    let unit = units(var)?;
    if is_east(&unit) {
        return Some(AxisType::Lon);
    }
    if is_north(&unit) {
        return Some(AxisType::Lat);
    }
    if time::is_date_unit(&unit) {
        return Some(AxisType::Time);
    }
    if is_pressure(&unit) {
        return Some(AxisType::Pressure);
    }
    if ["level", "layer", "sigma_level"]
        .iter()
        .any(|x| unit.eq_ignore_ascii_case(x))
    {
        return Some(AxisType::GeoZ);
    }
    if string_attr(&var.attrs, "positive").is_some() {
        return Some(if is_length(&unit) {
            AxisType::Height
        } else {
            AxisType::GeoZ
        });
    }
    None
}

const VERTICAL_COORDS: [&str; 11] = [
    "atmosphere_ln_pressure_coordinate",
    "atmosphere_sigma_coordinate",
    "atmosphere_hybrid_sigma_pressure_coordinate",
    "atmosphere_hybrid_height_coordinate",
    "atmosphere_sleve_coordinate",
    "ocean_sigma_coordinate",
    "ocean_s_coordinate",
    "ocean_sigma_z_coordinate",
    "ocean_double_sigma_coordinate",
    "ocean_s_coordinate_g1",
    "ocean_s_coordinate_g2",
];

/// `CF1Convention.getAxisType`.
fn axis_type_cf(var: &Var) -> Option<AxisType> {
    let name = string_attr(&var.attrs, "standard_name").map(|s| s.trim().to_string());
    if let Some(s) = &name {
        if VERTICAL_COORDS.iter().any(|v| s.eq_ignore_ascii_case(v)) {
            return Some(AxisType::GeoZ);
        }
    }
    if let Some(found) = axis_type_coards(var) {
        return Some(found);
    }
    if let Some(s) = &name {
        let is = |x: &str| s.eq_ignore_ascii_case(x);
        if is("realization") {
            return Some(AxisType::Ensemble);
        }
        if is("latitude") {
            return Some(AxisType::Lat);
        }
        if is("longitude") {
            return Some(AxisType::Lon);
        }
        if is("projection_x_coordinate") || is("grid_longitude") || is("rotated_longitude") {
            return Some(AxisType::GeoX);
        }
        if is("projection_y_coordinate") || is("grid_latitude") || is("rotated_latitude") {
            return Some(AxisType::GeoY);
        }
        if is("forecast_reference_time") {
            return Some(AxisType::RunTime);
        }
        if is("forecast_period") {
            return Some(AxisType::TimeOffset);
        }
    }
    if let Some(axis) = string_attr(&var.attrs, "axis") {
        let axis = axis.trim().to_ascii_uppercase();
        let unit = units(var);
        let length = unit.as_deref().is_some_and(is_length);
        match axis.as_str() {
            "X" if length => return Some(AxisType::GeoX),
            "Y" if length => return Some(AxisType::GeoY),
            "Z" => {
                return Some(match unit.as_deref() {
                    None => AxisType::GeoZ,
                    Some(u) if is_length(u) => AxisType::Height,
                    Some(u) if is_pressure(u) => AxisType::Pressure,
                    Some(_) => AxisType::GeoZ,
                })
            }
            _ => {}
        }
    }
    None
}

fn alias(var: &Var) -> String {
    string_attr(&var.attrs, "coord_axis")
        .or_else(|| string_attr(&var.attrs, "coord_alias"))
        .unwrap_or_default()
}

/// `DefaultConventions.getAxisType`.
fn axis_type_default(var: &Var) -> Option<AxisType> {
    if let Some(found) = axis_type_coards(var) {
        return Some(found);
    }
    let name = var.short.as_str();
    let unit = units(var).unwrap_or_default();
    let desc = super::model::description(var).unwrap_or_default();
    let al = alias(var);
    let is = |x: &str| name.eq_ignore_ascii_case(x);
    let al_is = |x: &str| al.eq_ignore_ascii_case(x);
    if is("x") || al_is("x") {
        return Some(AxisType::GeoX);
    }
    if is("lon") || is("longitude") || al_is("lon") {
        return Some(AxisType::Lon);
    }
    if is("y") || al_is("y") {
        return Some(AxisType::GeoY);
    }
    if is("lat") || is("latitude") || al_is("lat") {
        return Some(AxisType::Lat);
    }
    if is("lev") || al_is("lev") || is("level") || al_is("level") {
        return Some(AxisType::GeoZ);
    }
    if (is("z")
        || al_is("z")
        || is("altitude")
        || desc.contains("altitude")
        || is("depth")
        || is("elev")
        || is("elevation"))
        && is_length(&unit)
    {
        return Some(AxisType::Height);
    }
    if (is("time") || al_is("time")) && time::is_date_unit(&unit) {
        return Some(AxisType::Time);
    }
    None
}

fn axis_type_of(var: &Var, convention: Convention) -> Option<AxisType> {
    match convention {
        Convention::Cf => axis_type_cf(var),
        Convention::Coards => axis_type_coards(var),
        Convention::Default => axis_type_default(var),
    }
}

/// `AxisType.getType` on `_CoordinateAxisType`.
fn stated_axis_type(var: &Var, convention: Convention) -> Option<Option<AxisType>> {
    // CF1Convention.augmentGroup adds the attribute from these standard names.
    if convention == Convention::Cf {
        if let Some(name) = string_attr(&var.attrs, "standard_name") {
            let name = name.trim();
            let is = |x: &str| name.eq_ignore_ascii_case(x);
            if is("forecast_reference_time") {
                return Some(Some(AxisType::RunTime));
            }
            if is("forecast_period") {
                return Some(Some(AxisType::TimeOffset));
            }
            if is("time") {
                return Some(Some(AxisType::Time));
            }
            if is("ensemble") || is("realization") {
                return Some(Some(AxisType::Ensemble));
            }
        }
    }
    let attr = find_attr_ignore_case(&var.attrs, "_CoordinateAxisType")?;
    Some(attr_string(attr).and_then(|s| match s.as_str() {
        "Lat" => Some(AxisType::Lat),
        "Lon" => Some(AxisType::Lon),
        "GeoX" => Some(AxisType::GeoX),
        "GeoY" => Some(AxisType::GeoY),
        "GeoZ" => Some(AxisType::GeoZ),
        "Height" => Some(AxisType::Height),
        "Pressure" => Some(AxisType::Pressure),
        "Time" => Some(AxisType::Time),
        "RunTime" => Some(AxisType::RunTime),
        "TimeOffset" => Some(AxisType::TimeOffset),
        "Ensemble" => Some(AxisType::Ensemble),
        _ => None,
    }))
}

/// `CoordSystemBuilder.isCoordinateVariable`.
fn is_coordinate_variable(var: &Var) -> bool {
    match var.dims.as_slice() {
        [d] => d.name == var.short,
        [d, _] => d.name == var.short && var.dtype == NcType::Char,
        _ => false,
    }
}

/// What the builder decided for every variable.
#[derive(Debug)]
pub(super) struct CoordSystems {
    pub convention: Convention,
    /// `Some` for variables made into coordinate axes; the inner value is the axis type.
    pub axis: Vec<Option<Option<AxisType>>>,
    /// The axes of each variable's coordinate system, empty when it has none.
    pub system: Vec<Vec<usize>>,
}

pub(super) fn build(model: &Model) -> Result<CoordSystems, VortexError> {
    let convention = convention(model)?;
    let count = model.vars.len();
    for var in &model.vars {
        if let Some(a) = COORDINATE_ATTRIBUTES
            .iter()
            .find(|name| var.attrs.iter().any(|a| a.name == **name))
        {
            return Err(refused(
                format!("NetCDF variable {}", var.full),
                format!("attribute {a} selects coordinate rules with no sample here"),
            ));
        }
    }
    let coordinate_variable: Vec<bool> = model.vars.iter().map(is_coordinate_variable).collect();
    let mut is_axis: Vec<bool> = model
        .vars
        .iter()
        .map(|v| stated_axis_type(v, convention).is_some())
        .collect();

    // DefaultConventions.identifyCoordinateAxes, first step.
    if convention == Convention::Default {
        for (i, var) in model.vars.iter().enumerate() {
            let al = alias(var);
            if !coordinate_variable[i]
                && !al.is_empty()
                && model.groups[0].dims.iter().any(|d| d.name == al)
            {
                is_axis[i] = true;
            }
        }
    }
    // CoordSystemBuilder.identifyCoordinateAxes, with `coordinates` as CSMConvention and
    // DefaultConventions set it. COARDS itself does not read the attribute.
    let coordinates = |var: &Var| {
        if convention == Convention::Coards {
            None
        } else {
            string_attr(&var.attrs, "coordinates")
        }
    };
    for (i, var) in model.vars.iter().enumerate() {
        if let Some(list) = coordinates(var) {
            for token in list.split_whitespace() {
                if let Some(found) = model.find(token, i) {
                    is_axis[found] = true;
                }
            }
        }
    }
    if convention == Convention::Default {
        force_default_axes(model, &coordinate_variable, &mut is_axis);
    }

    // makeCoordinateAxes.
    let axis: Vec<Option<Option<AxisType>>> = (0..count)
        .map(|i| {
            if !is_axis[i] && !coordinate_variable[i] {
                return None;
            }
            Some(match stated_axis_type(&model.vars[i], convention) {
                Some(stated) => stated,
                None => axis_type_of(&model.vars[i], convention),
            })
        })
        .collect();

    let mut system = vec![Vec::new(); count];
    let all_axes: Vec<usize> = (0..count).filter(|i| axis[*i].is_some()).collect();
    for i in 0..count {
        if coordinate_variable[i] {
            continue;
        }
        let var = &model.vars[i];
        // makeCoordinateSystemsImplicit: findCoordinateAxes(true).
        let mut axes: Vec<usize> = Vec::new();
        if let Some(list) = coordinates(var) {
            for token in list.split_whitespace() {
                if let Some(found) = model.find(token, i) {
                    if axis[found].is_some() && !axes.contains(&found) {
                        axes.push(found);
                    }
                }
            }
        }
        for d in &var.dims {
            let Some(owner) = model.dim_owner(&d.name, var.group) else {
                continue;
            };
            for (j, c) in model.vars.iter().enumerate() {
                if coordinate_variable[j]
                    && c.group == owner
                    && c.dims.first().map(|x| &x.name) == Some(&d.name)
                    && !axes.contains(&j)
                {
                    axes.push(j);
                }
            }
        }
        if axes.len() >= 2 && is_complete(model, &axes, var) {
            system[i] = axes;
            continue;
        }
        // makeCoordinateSystemsMaximal, for variables that are not axes.
        if is_axis[i] {
            continue;
        }
        let fit: Vec<usize> = all_axes
            .iter()
            .copied()
            .filter(|a| axis_fits(&model.vars[*a], var))
            .collect();
        if fit.len() >= 2 && is_complete(model, &fit, var) {
            system[i] = fit;
        }
    }
    Ok(CoordSystems {
        convention,
        axis,
        system,
    })
}

/// `DefaultConventions.identifyCoordinateAxes`, the forcing steps.
fn force_default_axes(model: &Model, coordinate_variable: &[bool], is_axis: &mut [bool]) {
    let mut have: Vec<AxisType> = Vec::new();
    for (i, var) in model.vars.iter().enumerate() {
        if is_axis[i] {
            if let Some(t) = axis_type_default(var) {
                have.push(t);
            }
        }
    }
    if !have.contains(&AxisType::Time) {
        let mut found = false;
        for (i, var) in model.vars.iter().enumerate() {
            if units(var).is_some_and(|u| time::is_date_unit(&u)) {
                is_axis[i] = true;
                found = true;
            }
        }
        if found {
            have.push(AxisType::Time);
        }
    }
    for (i, var) in model.vars.iter().enumerate() {
        if coordinate_variable[i] {
            continue;
        }
        if let Some(t) = axis_type_default(var) {
            if !have.contains(&t) {
                is_axis[i] = true;
                have.push(t);
            }
        }
    }
}

/// `CoordinatesHelper.isComplete`: every dimension of the variable is in the axes' domain.
fn is_complete(model: &Model, axes: &[usize], var: &Var) -> bool {
    var.dims.iter().all(|d| {
        axes.iter()
            .any(|a| model.vars[*a].dims.iter().any(|x| x.name == d.name))
    })
}

/// `CoordSystemBuilder.isCoordinateAxisForVariable`.
fn axis_fits(axis: &Var, var: &Var) -> bool {
    let check = if axis.dtype == NcType::Char {
        axis.dims.len().saturating_sub(1)
    } else {
        axis.dims.len()
    };
    axis.dims.iter().take(check).all(|d| var.dims.contains(d))
}

/// The axes `CoordinateSystem` and `GridCoordSys` pick. Indices into the variable list.
#[derive(Debug, Clone, Default)]
pub(super) struct Picked {
    pub x: Option<usize>,
    pub y: Option<usize>,
    pub lat: Option<usize>,
    pub lon: Option<usize>,
    pub z: Option<usize>,
    pub time: Option<usize>,
    pub run_time: Option<usize>,
    pub ensemble: Option<usize>,
    pub time_offset: Option<usize>,
}

/// `CoordinateSystem.lesserRank`: the lower rank, the first on a tie.
fn lesser(model: &Model, current: Option<usize>, axis: usize) -> Option<usize> {
    match current {
        Some(c) if model.vars[c].dims.len() <= model.vars[axis].dims.len() => Some(c),
        _ => Some(axis),
    }
}

pub(super) fn pick(model: &Model, systems: &CoordSystems, axes: &[usize]) -> Picked {
    let mut p = Picked::default();
    let (mut height, mut pressure, mut geoz) = (None, None, None);
    for &a in axes {
        let Some(Some(kind)) = systems.axis[a] else {
            continue;
        };
        match kind {
            AxisType::GeoX => p.x = lesser(model, p.x, a),
            AxisType::GeoY => p.y = lesser(model, p.y, a),
            AxisType::Lat => p.lat = lesser(model, p.lat, a),
            AxisType::Lon => p.lon = lesser(model, p.lon, a),
            AxisType::Time => p.time = lesser(model, p.time, a),
            AxisType::RunTime => p.run_time = lesser(model, p.run_time, a),
            AxisType::Ensemble => p.ensemble = lesser(model, p.ensemble, a),
            AxisType::TimeOffset => p.time_offset = lesser(model, p.time_offset, a),
            AxisType::Height => height = lesser(model, height, a),
            AxisType::Pressure => pressure = lesser(model, pressure, a),
            AxisType::GeoZ => geoz = lesser(model, geoz, a),
        }
    }
    // GridCoordSys: height, then pressure, then z.
    p.z = [height, pressure, geoz]
        .into_iter()
        .flatten()
        .find(|a| model.vars[*a].dims.len() == 1);
    p
}

/// `CoordinateSystem.isSubset` for `GridCoordSys.isGridCoordSys`'s last test: the variable has no
/// dimension outside the axes `GridCoordSys` uses.
pub(super) fn covers(model: &Model, used: &[usize], dims: &[Dim]) -> bool {
    dims.iter().all(|d| {
        used.iter()
            .any(|a| model.vars[*a].dims.iter().any(|x| x == d))
    })
}

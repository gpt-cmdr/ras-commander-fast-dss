//! CF grid mappings to the WKT Vortex reports.
//!
//! Sources: NetCDF-Java 5.5.3 `dataset/transform/*`, `CF1Convention.augmentGroup` and
//! `CoordinateSystem.getProjection`; Vortex `WktFactory.createWkt` over GDAL 3.2 and PROJ 7.2.
//! Every text here was compared with Vortex on a file. A mapping or earth form no file reached is
//! refused, since there is no reference.

use super::axes::{CoordSystems, Picked};
use super::model::{attr_numbers, find_attr, number_attr, string_attr, units, Model, Var};
use super::refused;
use crate::error::VortexError;
use crate::maths;
use crate::source::grib::wkt_number;

const GEOGCS_UNIT: &str = "UNIT[\"degree\",0.0174532925199433,";

fn number(value: f64) -> Result<String, VortexError> {
    wkt_number(value).ok_or_else(|| refused("NetCDF projection", format!("WKT parameter {value}")))
}

/// `WktFactory.createWkt` for `LatLonProjection`: its default sphere, whatever the file's mapping
/// says about the earth. The GRIB2 source writes the same text.
pub(super) fn geographic() -> Result<String, VortexError> {
    Ok(format!(
        "GEOGCS[\"unknown\",DATUM[\"unknown\",SPHEROID[\"sphere\",{},0]],PRIMEM[\"Greenwich\",0],\
         {GEOGCS_UNIT}AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]]",
        number(6371229.0)?
    ))
}

fn pretty_geogcs(spheroid: &str) -> String {
    format!(
        "    GEOGCS[\"unknown\",\n        DATUM[\"unknown\",\n            SPHEROID[{spheroid}]],\n        \
         PRIMEM[\"Greenwich\",0],\n        {GEOGCS_UNIT}\n            AUTHORITY[\"EPSG\",\"9122\"]]],\n"
    )
}

const PROJECTED_TAIL: &str =
    "    UNIT[\"Meter\",1],\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]";

/// A projection a grid's x and y axes can be in.
#[derive(Debug, Clone)]
pub(super) struct Projection {
    pub wkt: String,
    pub rotated: bool,
}

/// `WktFactory.setGcsParameters`, after `readStandardParams`' test for an ellipsoidal earth.
fn spheroid(var: &Var) -> Result<String, VortexError> {
    let what = || format!("NetCDF grid mapping {}", var.full);
    let major = number_attr(&var.attrs, "semi_major_axis");
    let minor = number_attr(&var.attrs, "semi_minor_axis");
    let flattening = number_attr(&var.attrs, "inverse_flattening").unwrap_or(0.0);
    if let Some(major) = major {
        if minor.is_some() && flattening != 0.0 {
            return Err(refused(
                what(),
                "both a semi-minor axis and an inverse flattening have no sample here",
            ));
        }
        if let Some(minor) = minor {
            let inverse = 1.0 / (1.0 - (minor / major));
            return Ok(format!(
                "\"spheroid\",{},{}",
                number(major)?,
                number(inverse)?
            ));
        }
        if flattening != 0.0 {
            return Ok(format!(
                "\"spheroid\",{},{}",
                number(major)?,
                number(flattening)?
            ));
        }
    }
    // AbstractTransformBuilder.getEarthRadiusInKm: a value over 10000 is in metres.
    let radius_km = match number_attr(&var.attrs, "earth_radius") {
        Some(r) if r > 10000.0 => r * 0.001,
        Some(r) => r,
        None => 6371229.0 * 0.001,
    };
    Ok(format!("\"sphere\",{},0", number(radius_km * 1000.0)?))
}

fn is_ellipsoid(var: &Var) -> bool {
    number_attr(&var.attrs, "semi_major_axis").is_some()
        && (number_attr(&var.attrs, "semi_minor_axis").is_some()
            || number_attr(&var.attrs, "inverse_flattening").is_some_and(|f| f != 0.0))
}

/// Observed behaviour we did not implement: with a false origin on these mappings, Vortex 1.1.0
/// writes NetCDF-Java's kilometre value under `UNIT["Meter",1]`. A file stating 1000 m on an axis in
/// metres gives `false_easting` 1 in the WKT. Seen on synthetic Lambert and Albers files; this
/// crate refuses a nonzero false origin there rather than report that origin.
fn no_false_origin(var: &Var) -> Result<(), VortexError> {
    for name in ["false_easting", "false_northing"] {
        if number_attr(&var.attrs, name).is_some_and(|v| v != 0.0) {
            return Err(refused(
                format!("NetCDF grid mapping {}", var.full),
                format!(
                    "a nonzero {name} here. Observed behaviour we did not implement: Vortex's WKT \
                     states the value in kilometres under a metre unit"
                ),
            ));
        }
    }
    Ok(())
}

fn need(var: &Var, name: &str) -> Result<f64, VortexError> {
    number_attr(&var.attrs, name).ok_or_else(|| {
        refused(
            format!("NetCDF grid mapping {}", var.full),
            format!("it has no {name}"),
        )
    })
}

/// `readAttributeDouble2`: one parallel is used twice.
fn parallels(var: &Var) -> Result<(f64, f64), VortexError> {
    let values = find_attr(&var.attrs, "standard_parallel")
        .and_then(attr_numbers)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            refused(
                format!("NetCDF grid mapping {}", var.full),
                "it has no standard_parallel",
            )
        })?;
    Ok((values[0], *values.get(1).unwrap_or(&values[0])))
}

fn projected(name: &str, geogcs: &str, body: &str) -> String {
    format!(
        "PROJCS[\"{name}\",\n{}{body}{PROJECTED_TAIL}",
        pretty_geogcs(geogcs)
    )
}

const ZERO_ORIGIN: &str =
    "    PARAMETER[\"false_easting\",0],\n    PARAMETER[\"false_northing\",0],\n";

fn lambert(var: &Var) -> Result<Projection, VortexError> {
    let (first, second) = parallels(var)?;
    no_false_origin(var)?;
    let body = format!(
        "    PROJECTION[\"Lambert_Conformal_Conic_2SP\"],\n    PARAMETER[\"latitude_of_origin\",{}],\n    \
         PARAMETER[\"central_meridian\",{}],\n    PARAMETER[\"standard_parallel_1\",{}],\n    \
         PARAMETER[\"standard_parallel_2\",{}],\n{ZERO_ORIGIN}",
        number(need(var, "latitude_of_projection_origin")?)?,
        number(need(var, "longitude_of_central_meridian")?)?,
        number(first)?,
        number(second)?,
    );
    Ok(Projection {
        wkt: projected("Lambert Conformal Conic 2SP", &spheroid(var)?, &body),
        rotated: false,
    })
}

fn albers(var: &Var) -> Result<Projection, VortexError> {
    let (first, second) = parallels(var)?;
    no_false_origin(var)?;
    let body = format!(
        "    PROJECTION[\"Albers_Conic_Equal_Area\"],\n    PARAMETER[\"latitude_of_center\",{}],\n    \
         PARAMETER[\"longitude_of_center\",{}],\n    PARAMETER[\"standard_parallel_1\",{}],\n    \
         PARAMETER[\"standard_parallel_2\",{}],\n{ZERO_ORIGIN}",
        number(need(var, "latitude_of_projection_origin")?)?,
        number(need(var, "longitude_of_central_meridian")?)?,
        number(first)?,
        number(second)?,
    );
    let name = if is_ellipsoid(var) {
        "Albers Equal Area Conic"
    } else {
        "Albers Equal Area"
    };
    Ok(Projection {
        wkt: projected(name, &spheroid(var)?, &body),
        rotated: false,
    })
}

/// `TransverseMercator.makeCoordinateTransform` scales the false origin to kilometres by the x
/// axis's unit, and `WktFactory` multiplies it back by 1000.
fn transverse_mercator(var: &Var, axis_to_km: f64) -> Result<Projection, VortexError> {
    let origin = |name: &str| {
        let value = number_attr(&var.attrs, name).unwrap_or(0.0);
        if value == 0.0 {
            0.0
        } else {
            value * axis_to_km * 1000.0
        }
    };
    let body = format!(
        "    PROJECTION[\"Transverse_Mercator\"],\n    PARAMETER[\"latitude_of_origin\",{}],\n    \
         PARAMETER[\"central_meridian\",{}],\n    PARAMETER[\"scale_factor\",{}],\n    \
         PARAMETER[\"false_easting\",{}],\n    PARAMETER[\"false_northing\",{}],\n",
        number(need(var, "latitude_of_projection_origin")?)?,
        number(need(var, "longitude_of_central_meridian")?)?,
        number(need(var, "scale_factor_at_central_meridian")?)?,
        number(origin("false_easting"))?,
        number(origin("false_northing"))?,
    );
    Ok(Projection {
        wkt: projected("unnamed", &spheroid(var)?, &body),
        rotated: false,
    })
}

/// `PolarStereographic.makeCoordinateTransform`, on a sphere. An ellipsoid gets another projection
/// class, with axes reported otherwise, and is refused here.
fn polar(var: &Var) -> Result<Projection, VortexError> {
    if is_ellipsoid(var) {
        return Err(refused(
            format!("NetCDF grid mapping {}", var.full),
            "an ellipsoidal polar stereographic has no port here",
        ));
    }
    no_false_origin(var)?;
    let lon = number_attr(&var.attrs, "straight_vertical_longitude_from_pole")
        .or_else(|| number_attr(&var.attrs, "longitude_of_projection_origin"))
        .ok_or_else(|| {
            refused(
                format!("NetCDF grid mapping {}", var.full),
                "it has no longitude parameter",
            )
        })?;
    let lat = number_attr(&var.attrs, "latitude_of_projection_origin").unwrap_or(90.0);
    let scale = match (
        number_attr(&var.attrs, "scale_factor_at_projection_origin"),
        number_attr(&var.attrs, "standard_parallel"),
    ) {
        (Some(scale), _) => scale,
        (None, Some(parallel)) => {
            // Math.toRadians is a multiplication by this constant.
            let sine = maths::sin(parallel.abs() * 0.017453292519943295).abs();
            (1.0 + sine) / 2.0
        }
        (None, None) => 0.9330127018922193,
    };
    stereographic_body(var, lat, lon, scale)
}

fn stereographic(var: &Var) -> Result<Projection, VortexError> {
    if is_ellipsoid(var) {
        return Err(refused(
            format!("NetCDF grid mapping {}", var.full),
            "an ellipsoidal stereographic has no port here",
        ));
    }
    no_false_origin(var)?;
    let lat = number_attr(&var.attrs, "latitude_of_projection_origin").unwrap_or(90.0);
    let scale = number_attr(&var.attrs, "scale_factor_at_projection_origin").unwrap_or(1.0);
    stereographic_body(
        var,
        lat,
        need(var, "longitude_of_projection_origin")?,
        scale,
    )
}

fn stereographic_body(
    var: &Var,
    lat: f64,
    lon: f64,
    scale: f64,
) -> Result<Projection, VortexError> {
    let body = format!(
        "    PROJECTION[\"Stereographic\"],\n    PARAMETER[\"latitude_of_origin\",{}],\n    \
         PARAMETER[\"central_meridian\",{}],\n    PARAMETER[\"scale_factor\",{}],\n{ZERO_ORIGIN}",
        number(lat)?,
        number(lon)?,
        number(scale)?,
    );
    Ok(Projection {
        wkt: projected("unnamed", &spheroid(var)?, &body),
        rotated: false,
    })
}

/// A scale factor in place of the parallel goes through `convertScaleToStandardParallel`, and is
/// refused: no sample has it.
fn mercator(var: &Var) -> Result<Projection, VortexError> {
    no_false_origin(var)?;
    let body = format!(
        "    PROJECTION[\"Mercator_2SP\"],\n    PARAMETER[\"standard_parallel_1\",{}],\n    \
         PARAMETER[\"central_meridian\",{}],\n{ZERO_ORIGIN}",
        number(need(var, "standard_parallel")?)?,
        number(need(var, "longitude_of_projection_origin")?)?,
    );
    Ok(Projection {
        wkt: projected("unnamed", &spheroid(var)?, &body),
        rotated: false,
    })
}

fn sinusoidal(var: &Var) -> Result<Projection, VortexError> {
    no_false_origin(var)?;
    let body = format!(
        "    PROJECTION[\"Sinusoidal\"],\n    PARAMETER[\"longitude_of_center\",{}],\n{ZERO_ORIGIN}",
        number(need(var, "longitude_of_central_meridian")?)?,
    );
    Ok(Projection {
        wkt: projected("unnamed", &spheroid(var)?, &body),
        rotated: false,
    })
}

fn rotated_pole(var: &Var) -> Result<Projection, VortexError> {
    let what = || format!("NetCDF grid mapping {}", var.full);
    let need = |name: &str| {
        number_attr(&var.attrs, name).ok_or_else(|| refused(what(), format!("it has no {name}")))
    };
    let lon = need("grid_north_pole_longitude")?;
    let lat = need("grid_north_pole_latitude")?;
    const ANGLE: &str =
        "ANGLEUNIT[\"degree\",0.0174532925199433,\n                ID[\"EPSG\",9122]]";
    Ok(Projection {
        wkt: format!(
            "GEOGCRS[\"unnamed\",\n    BASEGEOGCRS[\"unknown\",\n        DATUM[\"unknown\",\n            \
             ELLIPSOID[\"WGS 84\",6378137,298.257223562997,\n                LENGTHUNIT[\"metre\",1,\n                    \
             ID[\"EPSG\",9001]]]],\n        PRIMEM[\"Greenwich\",0,\n            ANGLEUNIT[\"degree\",0.0174532925199433],\n            \
             ID[\"EPSG\",8901]]],\n    DERIVINGCONVERSION[\"unknown\",\n        METHOD[\"PROJ ob_tran o_proj=longlat\"],\n        \
             PARAMETER[\"lon_0\",{},\n            {ANGLE}],\n        PARAMETER[\"o_lon_p\",0,\n            {ANGLE}],\n        \
             PARAMETER[\"o_lat_p\",{},\n            {ANGLE}]],\n    CS[ellipsoidal,2],\n        \
             AXIS[\"longitude\",east,\n            ORDER[1],\n            {ANGLE}],\n        \
             AXIS[\"latitude\",north,\n            ORDER[2],\n            {ANGLE}]]",
            number(180.0 + lon)?,
            number(lat)?,
        ),
        rotated: true,
    })
}

/// The projection a coordinate system with x and y axes takes. `CF1Convention.augmentGroup` marks
/// mapping variables that a `grid_mapping` attribute names, or failing that any with a
/// `grid_mapping_name`; the builder attaches those of the x and y kind to systems with such axes.
pub(super) fn projection(
    model: &Model,
    systems: &CoordSystems,
    axes: &[usize],
    picked: &Picked,
) -> Result<Option<Projection>, VortexError> {
    if axes.is_empty() || picked.x.is_none() || picked.y.is_none() {
        return Ok(None);
    }
    if systems.convention != super::axes::Convention::Cf {
        // The Default builder's projection attributes and others have no sample here.
        return Ok(None);
    }
    let group = model.vars[axes[0]].group;
    let mut mapping: Vec<usize> = Vec::new();
    for var in model.vars.iter().filter(|v| v.group == group) {
        if let Some(name) = string_attr(&var.attrs, "grid_mapping") {
            if let Some(found) = model.find_local(&name, group) {
                if !mapping.contains(&found) {
                    mapping.push(found);
                }
            }
        }
    }
    if mapping.is_empty() {
        for (i, var) in model.vars.iter().enumerate() {
            if var.group == group && string_attr(&var.attrs, "grid_mapping_name").is_some() {
                mapping.push(i);
            }
        }
    }
    let of_xy: Vec<usize> = mapping
        .into_iter()
        .filter(|i| {
            string_attr(&model.vars[*i].attrs, "grid_mapping_name").as_deref()
                != Some("latitude_longitude")
        })
        .collect();
    match of_xy.as_slice() {
        [] => Ok(None),
        [only] => {
            let var = &model.vars[*only];
            match string_attr(&var.attrs, "grid_mapping_name").as_deref() {
                Some("lambert_conformal_conic") => lambert(var).map(Some),
                Some("albers_conical_equal_area") => albers(var).map(Some),
                Some("transverse_mercator") => {
                    let x = picked.x.map(|x| &model.vars[x]);
                    let unit = x.and_then(units).map(|u| u.to_ascii_lowercase());
                    let to_km = match unit.as_deref() {
                        Some("km") | Some("kilometer") | Some("kilometers") => 1.0,
                        Some("m") | Some("meter") | Some("meters") | Some("metre")
                        | Some("metres") => 0.001,
                        other => {
                            return Err(refused(
                                format!("NetCDF grid mapping {}", var.full),
                                format!("x axis units {other:?} have no sample here"),
                            ))
                        }
                    };
                    transverse_mercator(var, to_km).map(Some)
                }
                Some("polar_stereographic") => polar(var).map(Some),
                Some("stereographic") => stereographic(var).map(Some),
                Some("mercator") => mercator(var).map(Some),
                Some("sinusoidal") => sinusoidal(var).map(Some),
                Some("rotated_latitude_longitude") => rotated_pole(var).map(Some),
                other => Err(refused(
                    format!("NetCDF grid mapping {}", var.full),
                    format!("{other:?} has no sample here"),
                )),
            }
        }
        _ => Err(refused(
            "NetCDF grid mapping",
            "several mappings in one group have no sample here",
        )),
    }
}

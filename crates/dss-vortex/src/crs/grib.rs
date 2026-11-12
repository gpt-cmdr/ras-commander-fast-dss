//! GRIB2 earth shapes and the CRS Vortex derives from them, as NetCDF-Java 5.5.3 does:
//! `ucar/nc2/grib/grib2/Grib2Gds.java` `getEarth` and each template's `makeHorizCoordSys`.

use super::{CrsDef, Datum, Projection};
use crate::error::VortexError;

/// `Grib2Gds.getEarth`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Earth {
    Sphere {
        radius: f64,
    },
    /// Inverse flattening given directly.
    Flattened {
        a: f64,
        rf: f64,
    },
    /// Major and minor axes from the file.
    Axes {
        major: f64,
        minor: f64,
    },
}

/// `ProjectionImpl.EARTH_RADIUS` and `Earth.DEFAULT`, in metres.
pub const NETCDF_JAVA_DEFAULT_RADIUS: f64 = 6371229.0;

/// Code table 3.2, as `Grib2Gds.getEarth` reads it. Values below 6,000,000 are taken as
/// kilometres. Other codes get the default earth, as there.
pub fn grib2_earth(shape: u32, radius: f64, major: f64, minor: f64) -> Earth {
    let metres = |v: f64| if v < 6_000_000.0 { v * 1000.0 } else { v };
    match shape {
        0 => Earth::Sphere { radius: 6367470.0 },
        1 => Earth::Sphere {
            radius: metres(radius),
        },
        2 => Earth::Flattened {
            a: 6378160.0,
            rf: 297.0,
        },
        3 | 7 => Earth::Axes {
            major: metres(major),
            minor: metres(minor),
        },
        4 => Earth::Flattened {
            a: 6378137.0,
            rf: 298.257222101,
        },
        5 => Earth::Flattened {
            a: 6378137.0,
            rf: 298.257223563,
        },
        6 => Earth::Sphere { radius: 6371229.0 },
        8 => Earth::Sphere { radius: 6371200.0 },
        9 => Earth::Flattened {
            a: 6377563.396,
            rf: 299.3249646,
        },
        _ => Earth::Sphere {
            radius: NETCDF_JAVA_DEFAULT_RADIUS,
        },
    }
}

/// The grid templates this tier reads, with their stored parameters in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GribGrid {
    /// Template 3.0.
    LatLon,
    /// Template 3.20. `north_pole` is the projection centre flag's first bit being clear.
    Polar {
        lov: f64,
        lad: f64,
        north_pole: bool,
    },
    /// Template 3.30.
    Lambert { latin1: f64, latin2: f64, lov: f64 },
}

/// The CRS Vortex's WKT states for a GRIB grid, which is not always the file's earth:
///
/// - Template 3.0 gets a default-earth sphere: `CoordinateSystem.getProjection` builds
///   `new LatLonProjection()`.
/// - Template 3.20 on a spherical earth gets a default-earth sphere:
///   `new Stereographic(latOrigin, lov, scale)` uses `ProjectionImpl.EARTH_RADIUS`.
/// - Template 3.30 on a spherical earth uses the file's radius.
///
/// Ellipsoidal projected grids take other NetCDF-Java classes, and no sample has exercised
/// them, so they are refused until one does.
pub fn vortex_grib_crs(grid: GribGrid, earth: Earth) -> Result<CrsDef, VortexError> {
    let sphere = |radius: f64, projection: Projection| CrsDef {
        datum: Datum::Sphere,
        a: radius,
        rf: 0.0,
        projection,
    };
    let refuse = || VortexError::Unsupported {
        what: "GRIB projected grid on an ellipsoidal earth".into(),
    };
    match grid {
        GribGrid::LatLon => Ok(CrsDef::sphere_lonlat(NETCDF_JAVA_DEFAULT_RADIUS)),
        GribGrid::Polar {
            lov,
            lad,
            north_pole,
        } => {
            if !matches!(earth, Earth::Sphere { .. }) {
                return Err(refuse());
            }
            // Grib2Gds.PolarStereographic: scale = (1 + sin(toRadians(|lad|))) / 2, with Java's
            // toRadians factor. The WKT then keeps 15 significant digits.
            let scale = (1.0 + super::maths::sin(lad.abs() * 0.017453292519943295)) / 2.0;
            Ok(sphere(
                NETCDF_JAVA_DEFAULT_RADIUS,
                Projection::Stereographic {
                    lat_0: if north_pole { 90.0 } else { -90.0 },
                    lon_0: lov,
                    k_0: super::proj_param(scale),
                    lat_ts: None,
                    x_0: 0.0,
                    y_0: 0.0,
                },
            ))
        }
        GribGrid::Lambert {
            latin1,
            latin2,
            lov,
        } => {
            let Earth::Sphere { radius } = earth else {
                return Err(refuse());
            };
            Ok(sphere(
                radius,
                Projection::Lambert {
                    lat_1: latin1,
                    lat_2: latin2,
                    lat_0: latin1,
                    lon_0: lov,
                    k_0: 1.0,
                    x_0: 0.0,
                    y_0: 0.0,
                },
            ))
        }
    }
}

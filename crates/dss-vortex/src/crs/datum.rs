//! The datum policy: WGS84, NAD83 and spheres are treated as no shift, as observed in
//! GDAL 3.2.1 and PROJ 7.2.0, where the pipelines between them contain no datum step. Any
//! other datum is refused unless the caller opts in. A caller-supplied shift grid is not
//! implemented yet, so that route is also a refusal.
//!
//! Stated limitation: PROJ 7.2.0 applies EPSG "NAD83 to WGS 84 (3)" (+1, +1, -1 m, Hawaii)
//! and "(2)" (-2, 0, +4 m, Aleutians) from a NAD83 geographic CRS to a WGS84-based CRS or a
//! WGS84 geographic CRS to a NAD83 geographic CRS. This library applies neither, so those points
//! differ from PROJ. WGS84 to SHG shows no shift in PROJ either. The affected points are listed in
//! `tests/crs_golden.json` and checked by `tests/crs_transform.rs`.

use super::proj::proj_param;
use crate::error::VortexError;

/// Datums as the policy classifies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Datum {
    Wgs84,
    Nad83,
    /// Any datum whose ellipsoid is a sphere.
    Sphere,
    /// A datum the policy does not name. It carries the source's name.
    Other(String),
    /// The source states shift parameters that are not all zero.
    Shifted(String),
}

/// What to do with a datum outside the no-shift set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DatumPolicy {
    #[default]
    Refuse,
    /// The caller accepts treating every datum as coincident. PROJ would shift NAD27 and the
    /// like, so results then differ from PROJ on purpose.
    AssumeNoShift,
}

impl Datum {
    pub(crate) fn check(&self, policy: DatumPolicy) -> Result<(), VortexError> {
        match (self, policy) {
            (Datum::Wgs84 | Datum::Nad83 | Datum::Sphere, _) => Ok(()),
            (_, DatumPolicy::AssumeNoShift) => Ok(()),
            (Datum::Other(name) | Datum::Shifted(name), DatumPolicy::Refuse) => {
                Err(VortexError::Refused {
                    what: format!("datum {name}"),
                    reason: "needs a shift this library does not apply; only WGS84, NAD83 and \
                             spheres are treated as no shift unless the caller opts in"
                        .into(),
                })
            }
        }
    }

    /// Classify a WKT datum from everything it states. A name, ellipsoid or authority that
    /// disagrees with the others is `Other`, so it is refused rather than taken at its word.
    /// An unnamed datum on exactly the WGS84 or GRS80 ellipsoid takes that ellipsoid's datum:
    /// NetCDF-Java writes `unknown` for GRIB shapes 4 and 5.
    pub(crate) fn classify_stated(
        name: &str,
        a: f64,
        rf: f64,
        shifts: Option<&[f64]>,
        datum_authority: Option<String>,
        spheroid_authority: Option<String>,
        geogcs_authority: Option<String>,
    ) -> Datum {
        if shifts.is_some_and(|s| s.iter().any(|v| *v != 0.0)) {
            return Datum::Shifted(name.to_string());
        }
        let is_wgs84 = proj_param(a) == 6378137.0 && proj_param(rf) == 298.257223563;
        let is_grs80 = proj_param(a) == 6378137.0 && proj_param(rf) == 298.257222101;
        let upper = name.to_ascii_uppercase().replace([' ', '-'], "_");
        let bare = upper.strip_prefix("D_").unwrap_or(&upper);
        let unnamed = matches!(bare, "UNKNOWN" | "UNNAMED" | "SPHERE");
        let other = || Datum::Other(name.to_string());
        let classified = match bare {
            "WGS_1984" | "WGS84" | "WGS_84" | "WORLD_GEODETIC_SYSTEM_1984" if is_wgs84 => {
                Datum::Wgs84
            }
            "NORTH_AMERICAN_DATUM_1983" | "NORTH_AMERICAN_1983" | "NAD83" if is_grs80 => {
                Datum::Nad83
            }
            _ if unnamed && rf == 0.0 => Datum::Sphere,
            // HEC's HRAP (`zStructSpatialGrid.h`, HRAP_SRC_DEFINITION): a sphere with no EPSG code,
            // which PROJ 7.2.0 relates to the others with no datum step (measured).
            "SPHERE_LFM" if rf == 0.0 && proj_param(a) == 6371200.0 => Datum::Sphere,
            _ if unnamed && is_wgs84 => Datum::Wgs84,
            _ if unnamed && is_grs80 => Datum::Nad83,
            _ => return other(),
        };
        let (datum_code, spheroid_code, geogcs_code) = match classified {
            Datum::Wgs84 => ("6326", "7030", "4326"),
            Datum::Nad83 => ("6269", "7019", "4269"),
            // A sphere has no EPSG code in the no-shift set.
            _ => ("", "", ""),
        };
        let agrees = |stated: Option<String>, code: &str| stated.is_none_or(|s| s == code);
        if agrees(datum_authority, datum_code)
            && agrees(spheroid_authority, spheroid_code)
            && agrees(geogcs_authority, geogcs_code)
        {
            classified
        } else {
            other()
        }
    }
}

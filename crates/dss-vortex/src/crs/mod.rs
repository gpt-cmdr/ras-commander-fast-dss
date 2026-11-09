//! Coordinate reference systems: parsing the supported WKT subset, the datum policy, and
//! transformations that reproduce PROJ 7.2.0 as GDAL 3.2.1 drives it inside Vortex.
//!
//! Supported: geographic, Albers, Lambert conformal conic, and stereographic (polar, oblique,
//! equatorial), on a sphere or an ellipsoid, in metres, from Greenwich; and Transverse Mercator,
//! UTM included, on an ellipsoid, in metres, US survey feet or international feet. Everything
//! else is a typed refusal. All maths goes through [`maths`](self::maths).

pub mod datum;
pub mod envelope;
pub mod grib;
pub mod maths;
mod proj;
pub mod wkt;

pub use datum::{Datum, DatumPolicy};
pub use envelope::{reproject_envelope, Envelope};
pub use grib::{grib2_earth, vortex_grib_crs, Earth, GribGrid};
pub use proj::PointFailed;

use crate::error::VortexError;
use crate::frame::{Crs, NamedCrs};
use proj::{deg_to_rad, proj_param, rad_to_deg, Common, Projected};

/// A map projection's defining parameters; angles in degrees, offsets in metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Projection {
    LongLat,
    Albers {
        lat_1: f64,
        lat_2: f64,
        lat_0: f64,
        lon_0: f64,
        x_0: f64,
        y_0: f64,
    },
    Lambert {
        lat_1: f64,
        lat_2: f64,
        lat_0: f64,
        lon_0: f64,
        k_0: f64,
        x_0: f64,
        y_0: f64,
    },
    /// `lat_ts` set is WKT's `Polar_Stereographic` variant B; `k_0` then has no effect on a
    /// polar grid, as in `stere.cpp`.
    Stereographic {
        lat_0: f64,
        lon_0: f64,
        k_0: f64,
        lat_ts: Option<f64>,
        x_0: f64,
        y_0: f64,
    },
    /// `merc.cpp` `webmerc`: spherical Mercator on the ellipsoid's semi-major axis. ESRI's
    /// `Mercator_Auxiliary_Sphere` with type 0, whose other parameters must all be zero.
    WebMercator,
    /// `tmerc.cpp`, PROJ's exact (Poder/Engsager) form, on an ellipsoid. Unlike the other
    /// variants, `x_0` and `y_0` are in the CRS's own linear unit, of `to_metre` metres: the
    /// metre, the US survey foot or the international foot. A definition with UTM's values, in
    /// metres, is transformed as PROJ's `utm` (`Conversion::isUTM`).
    TransverseMercator {
        lat_0: f64,
        lon_0: f64,
        k_0: f64,
        x_0: f64,
        y_0: f64,
        to_metre: f64,
    },
}

/// The linear units a [`Projection::TransverseMercator`] may be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearUnit {
    Metre,
    /// 1200/3937 m.
    UsSurveyFoot,
    /// 0.3048 m.
    InternationalFoot,
}

impl LinearUnit {
    /// The unit a WKT `UNIT` value states, by PROJ's rule: within 1e-10, relative, of the unit's
    /// factor (`common.cpp` `UnitOfMeasure::exportToPROJString`). `None` is any other unit.
    pub fn of_metres(to_metre: f64) -> Option<LinearUnit> {
        match proj::linear_unit_factor(to_metre) {
            Ok(None) => Some(LinearUnit::Metre),
            Ok(Some(f)) if f == proj::INTERNATIONAL_FOOT => Some(LinearUnit::InternationalFoot),
            Ok(Some(_)) => Some(LinearUnit::UsSurveyFoot),
            Err(()) => None,
        }
    }
}

/// A parsed CRS definition.
#[derive(Debug, Clone, PartialEq)]
pub struct CrsDef {
    pub datum: Datum,
    /// Semi-major axis in metres.
    pub a: f64,
    /// Inverse flattening; 0 is a sphere, as in WKT1.
    pub rf: f64,
    pub projection: Projection,
}

impl CrsDef {
    /// The Standard Hydrologic Grid (`WktFactory.getShg`).
    pub fn shg() -> CrsDef {
        CrsDef {
            datum: Datum::Nad83,
            a: 6378137.0,
            rf: 298.257222101,
            projection: Projection::Albers {
                lat_1: 29.5,
                lat_2: 45.5,
                lat_0: 23.0,
                lon_0: -96.0,
                x_0: 0.0,
                y_0: 0.0,
            },
        }
    }

    /// HRAP: polar stereographic, true at 60 N, central meridian -105, on a 6,371,200 m sphere.
    pub fn hrap() -> CrsDef {
        CrsDef {
            datum: Datum::Sphere,
            a: 6371200.0,
            rf: 0.0,
            projection: Projection::Stereographic {
                lat_0: 90.0,
                lon_0: -105.0,
                k_0: 1.0,
                lat_ts: Some(60.0),
                x_0: 0.0,
                y_0: 0.0,
            },
        }
    }

    /// A sphere's geographic CRS, degrees of (longitude, latitude).
    pub fn sphere_lonlat(radius: f64) -> CrsDef {
        CrsDef {
            datum: Datum::Sphere,
            a: radius,
            rf: 0.0,
            projection: Projection::LongLat,
        }
    }

    /// What a frame's stated CRS means. PROJ strings are refused: no source produces them yet.
    pub fn from_frame(crs: &Crs) -> Result<CrsDef, VortexError> {
        match crs {
            Crs::Named(NamedCrs::Shg) => Ok(CrsDef::shg()),
            Crs::Named(NamedCrs::Hrap) => Ok(CrsDef::hrap()),
            Crs::Wkt(text) => CrsDef::parse_wkt(text),
            Crs::Proj(_) => Err(VortexError::Unsupported {
                what: "PROJ string as a frame CRS".into(),
            }),
            Crs::Undefined => Err(VortexError::Refused {
                what: "transformation".into(),
                reason: "the frame states no CRS".into(),
            }),
        }
    }

    /// A UTM zone on this datum's ellipsoid: zone 1 to 60, northern or southern. `None` for a
    /// zone outside that range.
    pub fn utm(datum: Datum, a: f64, rf: f64, zone: u32, south: bool) -> Option<CrsDef> {
        (1..=60).contains(&zone).then(|| CrsDef {
            datum,
            a,
            rf,
            projection: Projection::TransverseMercator {
                lat_0: 0.0,
                lon_0: f64::from(zone) * 6.0 - 183.0,
                k_0: 0.9996,
                x_0: 500000.0,
                y_0: if south { 10000000.0 } else { 0.0 },
                to_metre: 1.0,
            },
        })
    }

    /// The UTM zone and hemisphere PROJ takes this for, if it does (`Conversion::isUTM`).
    pub fn utm_zone(&self) -> Option<(u32, bool)> {
        match self.projection {
            Projection::TransverseMercator {
                lat_0,
                lon_0,
                k_0,
                x_0,
                y_0,
                to_metre,
            } => proj::utm_zone(
                &Common {
                    lat_0,
                    lon_0,
                    k_0,
                    x_0,
                    y_0,
                },
                to_metre,
            ),
            _ => None,
        }
    }

    /// Metres per unit of this CRS's coordinates; 1 for every CRS but a Transverse Mercator in
    /// feet. Meaningless for a geographic CRS.
    pub fn metres_per_unit(&self) -> f64 {
        match self.projection {
            Projection::TransverseMercator { to_metre, .. } => to_metre,
            _ => 1.0,
        }
    }

    /// Whether this is (longitude, latitude) in degrees.
    pub fn is_geographic(&self) -> bool {
        matches!(self.projection, Projection::LongLat)
    }

    fn build(&self) -> Result<Option<Projected>, VortexError> {
        let invalid = || VortexError::Refused {
            what: "CRS".into(),
            reason: "its parameters are ones PROJ 7.2.0 rejects".into(),
        };
        let s = proj::shape(self.a, self.rf).ok_or_else(invalid)?;
        let built = match self.projection {
            Projection::LongLat => return Ok(None),
            Projection::WebMercator => Projected::webmerc(s),
            Projection::Albers {
                lat_1,
                lat_2,
                lat_0,
                lon_0,
                x_0,
                y_0,
            } => {
                let c = Common {
                    lat_0,
                    lon_0,
                    k_0: 1.0,
                    x_0,
                    y_0,
                };
                Projected::aea(s, lat_1, lat_2, &c)
            }
            Projection::Lambert {
                lat_1,
                lat_2,
                lat_0,
                lon_0,
                k_0,
                x_0,
                y_0,
            } => {
                let c = Common {
                    lat_0,
                    lon_0,
                    k_0,
                    x_0,
                    y_0,
                };
                Projected::lcc(s, lat_1, lat_2, &c)
            }
            Projection::Stereographic {
                lat_0,
                lon_0,
                k_0,
                lat_ts,
                x_0,
                y_0,
            } => {
                let c = Common {
                    lat_0,
                    lon_0,
                    k_0,
                    x_0,
                    y_0,
                };
                Projected::stere(s, lat_ts, &c)
            }
            Projection::TransverseMercator {
                lat_0,
                lon_0,
                k_0,
                x_0,
                y_0,
                to_metre,
            } => {
                if self.rf == 0.0 {
                    return Err(VortexError::Unsupported {
                        what:
                            "Transverse Mercator on a sphere: PROJ's spherical form is not ported"
                                .into(),
                    });
                }
                if LinearUnit::of_metres(to_metre).is_none() {
                    return Err(VortexError::Unsupported {
                        what: format!(
                            "Transverse Mercator in a linear unit of {to_metre} m: only the metre, \
                             the US survey foot and the international foot are compared"
                        ),
                    });
                }
                let c = Common {
                    lat_0,
                    lon_0,
                    k_0,
                    x_0,
                    y_0,
                };
                Projected::tmerc(s, &c, to_metre)
            }
        };
        built.map(Some).ok_or_else(invalid)
    }

    /// Equality on the values that reach the formulas, ignoring the datum label: PROJ's
    /// pipeline between two such CRSs is `noop`.
    fn same_values(&self, other: &CrsDef) -> bool {
        let norm = |v: f64| proj_param(v).to_bits();
        norm(self.a) == norm(other.a)
            && norm(self.rf) == norm(other.rf)
            && self.read_by_proj() == other.read_by_proj()
    }

    /// A Transverse Mercator's values as PROJ reads them, 15 significant digits each, the unit's
    /// factor included: two texts of one CRS that differ past those digits, such as a US survey
    /// foot of 0.3048006096012192 and of 0.304800609601219, are the same CRS to PROJ, and its
    /// pipeline between them is `noop` (measured against PROJ). Other projections are compared as
    /// stated, as measured against PROJ.
    fn read_by_proj(&self) -> Projection {
        match self.projection {
            Projection::TransverseMercator {
                lat_0,
                lon_0,
                k_0,
                x_0,
                y_0,
                to_metre,
            } => Projection::TransverseMercator {
                lat_0: proj_param(lat_0),
                lon_0: proj_param(lon_0),
                k_0: proj_param(k_0),
                x_0: proj_param(x_0),
                y_0: proj_param(y_0),
                to_metre: proj_param(to_metre),
            },
            other => other,
        }
    }
}

/// One direction of a coordinate transformation: `OGRProjCT::TransformWithErrorCodes` over the
/// pipeline PROJ builds, which for every supported pair is the source's inverse followed by
/// the target's forward, with no datum step. x is longitude on a geographic CRS.
#[derive(Debug, Clone)]
pub struct Transformer {
    kind: Kind,
    src_wrap: Option<f64>,
    dst_wrap: Option<f64>,
    src_geographic: bool,
    dst_geographic: bool,
}

// One value per transformer, read per point: no indirection on the path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
enum Kind {
    /// Same CRS, or both geographic: PROJ's pipeline is `noop` or an axis swap.
    Identity,
    Steps {
        src: Option<Projected>,
        dst: Option<Projected>,
    },
}

impl Transformer {
    /// Fails with a typed refusal when either datum is outside `policy`, or the parameters are
    /// ones PROJ rejects.
    pub fn new(src: &CrsDef, dst: &CrsDef, policy: DatumPolicy) -> Result<Self, VortexError> {
        src.datum.check(policy)?;
        dst.datum.check(policy)?;
        let built_src = src.build()?;
        let built_dst = dst.build()?;
        let kind = if (src.is_geographic() && dst.is_geographic()) || src.same_values(dst) {
            Kind::Identity
        } else {
            Kind::Steps {
                src: built_src,
                dst: built_dst,
            }
        };
        Ok(Transformer {
            kind,
            src_wrap: None,
            dst_wrap: None,
            src_geographic: src.is_geographic(),
            dst_geographic: dst.is_geographic(),
        })
    }

    /// Wrap longitudes around these centres, as `OGRProjCT` does for the `CENTER_LONG` option
    /// `GDALCreateGenImgProjTransformer2` passes. Wrapping applies only on a geographic side.
    /// A `CENTER_LONG` written in WKT text has no effect in GDAL 3.2.1 (measured), so this is
    /// the only way to ask for it.
    pub fn with_center_long(mut self, src: Option<f64>, dst: Option<f64>) -> Self {
        self.src_wrap = src.filter(|_| self.src_geographic);
        self.dst_wrap = dst.filter(|_| self.dst_geographic);
        self
    }

    /// Transform one point. A failure is what PROJ reports as `HUGE_VAL`.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64), PointFailed> {
        let x = wrap(x, y, self.src_wrap);
        let (rx, ry) = match &self.kind {
            Kind::Identity => (x, y),
            Kind::Steps { src, dst } => {
                let (lam, phi) = match src {
                    Some(p) => p.inverse(x, y)?,
                    None => (deg_to_rad(x), deg_to_rad(y)),
                };
                match dst {
                    Some(p) => p.forward(lam, phi)?,
                    None => (rad_to_deg(lam), rad_to_deg(phi)),
                }
            }
        };
        Ok((wrap(rx, ry, self.dst_wrap), ry))
    }
}

// ogrct.cpp, "Potentially do longitude wrapping".
fn wrap(x: f64, y: f64, center: Option<f64>) -> f64 {
    match center {
        Some(c) if x != f64::INFINITY && y != f64::INFINITY => {
            if x < c - 180.0 {
                x + 360.0
            } else if x > c + 180.0 {
                x - 360.0
            } else {
                x
            }
        }
        _ => x,
    }
}

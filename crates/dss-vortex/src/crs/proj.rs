//! Projection formulas ported from PROJ 7.2.0, the version bundled in Vortex (MIT; see
//! `NOTICE`). The `proj4rs` crate was removed: it calls platform maths, which the
//! maintainer's platform-parity decision forbids, and its results differed from PROJ's in the
//! last bits. Keep the arithmetic in PROJ's order: reordering changes the last bit.
//!
//! Sources, under `src/`: `projections/aea.cpp`, `projections/lcc.cpp`,
//! `projections/stere.cpp`, `projections/tmerc.cpp`, `fwd.cpp`, `inv.cpp`, `adjlon.cpp`, `tsfn.cpp`, `phi2.cpp`,
//! `msfn.cpp`, `qsfn.cpp`, `ell_set.cpp`, `ellps.cpp`, `init.cpp`, `dmstor.cpp`,
//! `conversions/unitconvert.cpp`, `units.cpp`, `iso19111/datum.cpp`, `iso19111/internal.cpp`,
//! `iso19111/coordinateoperation.cpp`, `iso19111/common.cpp`.

use super::maths as m;

// proj_internal.h, proj.h
pub(crate) const DEG_TO_RAD: f64 = 0.017453292519943296;
const M_HALFPI: f64 = std::f64::consts::FRAC_PI_2;
const M_FORTPI: f64 = std::f64::consts::FRAC_PI_4;
const M_PI: f64 = std::f64::consts::PI;
const M_TWOPI: f64 = std::f64::consts::TAU;
const PJ_EPS_LAT: f64 = 1e-12;
const EPS10: f64 = 1e-10;

/// A point PROJ would report as failed: `proj_trans` returns `HUGE_VAL` for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointFailed;

type Point = Result<(f64, f64), PointFailed>;

/// `iso19111/internal.cpp` `toString(val, 15)`: PROJ reads every CRS parameter back from this
/// text, so a value keeps only its 15 significant digits, or 14 if those show "9999999999".
pub(crate) fn proj_param(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let text = format!("{value:.14e}");
    let text = if text.contains("9999999999") {
        format!("{value:.13e}")
    } else {
        text
    };
    // `pj_atof`, `pj_strtod` and Rust's parse all round correctly.
    text.parse().unwrap_or(value)
}

/// `pj_ellps` in `ellps.cpp`. `true` means the shape column is `b=`.
const PJ_ELLPS: &[(f64, f64, bool)] = &[
    (6378137.0, 298.257, false),
    (6378136.0, 298.257, false),
    (6378137.0, 298.257222101, false),
    (6378140.0, 298.257, false),
    (6377563.396, 299.3249646, false),
    (6378137.0, 298.25, false),
    (6378145.0, 298.25, false),
    (6377340.189, 6356034.446, true),
    (6377104.43, 300.0, false),
    (6377019.2563, 300.0, false),
    (6378160.0, 298.25, false),
    (6378160.0, 298.247167427, false),
    (6378136.5, 298.2564151, false),
    (6377397.155, 299.1528128, false),
    (6377483.865, 299.1528128, false),
    (6378206.4, 6356583.8, true),
    (6378249.145, 293.4663, false),
    (6378249.2, 293.4660212936269, false),
    (6375738.7, 334.29, false),
    (6376428.0, 311.5, false),
    (6378136.05, 298.2566, false),
    (6377276.345, 300.8017, false),
    (6377304.063, 300.8017, false),
    (6377301.243, 300.8017, false),
    (6377295.664, 300.8017, false),
    (6377298.556, 300.8017, false),
    (6378166.0, 298.3, false),
    (6378155.0, 298.3, false),
    (6378150.0, 298.3, false),
    (6378200.0, 298.3, false),
    (6378270.0, 297.0, false),
    (6378388.0, 297.0, false),
    (6378245.0, 298.3, false),
    (6378163.0, 298.24, false),
    (6378139.0, 298.257, false),
    (6397300.0, 191.0, false),
    (6378157.5, 6356772.2, true),
    (6376523.0, 6355863.0, true),
    (6378136.0, 298.25784, false),
    (6378155.0, 6356773.3205, true),
    (6376896.0, 6355834.8467, true),
    (6378165.0, 298.3, false),
    (6378145.0, 298.25, false),
    (6378135.0, 298.26, false),
    (6378137.0, 298.257223563, false),
    (6370997.0, 6370997.0, true),
];

/// The derived shape constants a projection uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Shape {
    a: f64,
    ra: f64,
    es: f64,
    e: f64,
    one_es: f64,
    /// Third flattening, `P->n`.
    n: f64,
}

/// `ell_set.cpp` `ellps_shape` and `pj_calc_ellipsoid_params`, reached through the choice
/// `Ellipsoid::_exportToPROJString` makes: a known `ellps=`, then `R=`, then `a= rf=`.
/// `rf == 0` is a sphere, as in WKT1.
pub(crate) fn shape(a: f64, rf: f64) -> Option<Shape> {
    let b = if rf == 0.0 { a } else { (1.0 - 1.0 / rf) * a };
    // datum.cpp lookForProjWellKnownEllps: the table's text replaces the stated values.
    let known = PJ_ELLPS.iter().find(|&&(a_i, s_i, is_b)| {
        (a - a_i).abs() < 1e-10 * a_i
            && if is_b {
                (b - s_i).abs() < 1e-10 * s_i
            } else {
                (rf - s_i).abs() < 1e-10 * s_i
            }
    });
    let (a, es) = match known {
        Some(&(a_i, s_i, true)) => {
            let es = if s_i == a_i {
                0.0
            } else {
                let f = (a_i - s_i) / a_i;
                2.0 * f - f * f
            };
            (a_i, es)
        }
        Some(&(a_i, s_i, false)) => {
            let f = 1.0 / s_i;
            (a_i, 2.0 * f - f * f)
        }
        None if rf == 0.0 => (proj_param(a), 0.0),
        None => {
            let f = 1.0 / proj_param(rf);
            (proj_param(a), 2.0 * f - f * f)
        }
    };
    let valid = a > 0.0 && (0.0..1.0).contains(&es);
    valid.then(|| {
        let e = m::sqrt(es);
        // pj_calc_ellipsoid_params: the angular eccentricity, then the third flattening.
        let alpha = m::asin(e);
        Shape {
            a,
            ra: 1.0 / a,
            es,
            e,
            one_es: 1.0 - es,
            n: m::pow(m::tan(alpha / 2.0), 2.0),
        }
    })
}

// adjlon.cpp
fn adjlon(mut lon: f64) -> f64 {
    if lon.abs() < M_PI + 1e-12 {
        return lon;
    }
    lon += M_PI;
    lon -= M_TWOPI * m::floor(lon / M_TWOPI);
    lon -= M_PI;
    lon
}

// tsfn.cpp
fn tsfn(phi: f64, sinphi: f64, e: f64) -> f64 {
    let sinphi = sinphi * e;
    let denominator = 1.0 + sinphi;
    if denominator == 0.0 {
        return f64::INFINITY;
    }
    m::tan(0.5 * (M_HALFPI - phi)) / m::pow((1.0 - sinphi) / denominator, 0.5 * e)
}

// msfn.cpp
fn msfn(sinphi: f64, cosphi: f64, es: f64) -> f64 {
    cosphi / m::sqrt(1.0 - es * sinphi * sinphi)
}

// qsfn.cpp
fn qsfn(sinphi: f64, e: f64, one_es: f64) -> f64 {
    if e >= 1.0e-7 {
        let con = e * sinphi;
        let div1 = 1.0 - con * con;
        let div2 = 1.0 + con;
        if div1 == 0.0 || div2 == 0.0 {
            return f64::INFINITY;
        }
        one_es * (sinphi / div1 - (0.5 / e) * m::log((1.0 - con) / div2))
    } else {
        sinphi + sinphi
    }
}

// phi2.cpp. Running out of iterations sets the context errno, which `pj_inv` then reports.
fn phi2(ts0: f64, e: f64) -> Result<f64, PointFailed> {
    let eccnth = 0.5 * e;
    let mut ts = ts0;
    let mut i = 15;
    loop {
        let sin_phi = (1.0 - ts * ts) / (1.0 + ts * ts);
        let con = e * sin_phi;
        let old_ts = ts;
        ts = ts0 * m::pow((1.0 - con) / (1.0 + con), eccnth);
        let dphi = 2.0 * (ts - old_ts);
        if dphi.abs() > 1.0e-10 {
            i -= 1;
            if i != 0 {
                continue;
            }
        }
        break;
    }
    if i <= 0 {
        return Err(PointFailed);
    }
    Ok(M_HALFPI - 2.0 * m::atan(ts))
}

// aea.cpp phi1_
fn aea_phi1(qs: f64, te: f64, tone_es: f64) -> f64 {
    let mut phi = m::asin(0.5 * qs);
    if te < 1.0e-7 {
        return phi;
    }
    let mut i = 15;
    loop {
        let sinpi = m::sin(phi);
        let cospi = m::cos(phi);
        let con = te * sinpi;
        let com = 1.0 - con * con;
        let dphi = 0.5 * com * com / cospi
            * (qs / tone_es - sinpi / com + 0.5 / te * m::log((1.0 - con) / (1.0 + con)));
        phi += dphi;
        // `!(fabs(dphi) > TOL)`: NaN returns too.
        if dphi.abs() <= 1.0e-10 || dphi.is_nan() {
            return phi;
        }
        i -= 1;
        if i < 0 {
            return f64::INFINITY;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Aea {
    ellips: bool,
    n: f64,
    n2: f64,
    c: f64,
    dd: f64,
    rho0: f64,
    ec: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Lcc {
    n: f64,
    c: f64,
    rho0: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StereMode {
    SPole,
    NPole,
    Obliq,
    Equit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Stere {
    mode: StereMode,
    akm1: f64,
    sin_x1: f64,
    cos_x1: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Formula {
    Aea(Aea),
    Lcc(Lcc),
    Stere(Stere),
    WebMerc,
    Tmerc(Tmerc),
}

/// `tmerc.cpp` `struct PoderEngsager`: the "exact" algorithm, which PROJ 7.2.0 selects for every
/// ellipsoidal `tmerc` and `utm` unless `proj.ini`, `+approx` or `+algo` says otherwise
/// (`getAlgoFromParams`; `proj_internal.h` `defaultTmercAlgo`). Vortex ships no `proj.ini`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Tmerc {
    /// Meridian quadrant, scaled to the projection.
    qn: f64,
    /// Radius vector in polar coordinate systems.
    zb: f64,
    /// Gaussian to geodetic latitude.
    cgb: [f64; 6],
    /// Geodetic to Gaussian latitude.
    cbg: [f64; 6],
    /// Transverse Mercator to geodetic.
    utg: [f64; 6],
    /// Geodetic to Transverse Mercator.
    gtu: [f64; 6],
}

/// One initialised map projection: `pj_init` plus its `PROJECTION()` setup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Projected {
    s: Shape,
    phi0: f64,
    lam0: f64,
    k0: f64,
    x0: f64,
    y0: f64,
    f: Formula,
    /// Metres per CRS unit when the CRS is not in metres: PROJ then puts a `unitconvert` step
    /// between this projection and the CRS (`ProjectedCRS::addUnitConvertAndAxisSwap`).
    unit: Option<f64>,
}

/// `units.cpp` `pj_units`: the factor `unitconvert` uses for `us-ft`.
pub(crate) const US_SURVEY_FOOT: f64 = 1200.0 / 3937.0;
/// `iso19111/io.cpp` `buildUnit`, `US_FOOT_CONV_FACTOR`: the factor a US survey foot read from
/// WKT has, which converts a false origin to metres. One ulp above [`US_SURVEY_FOOT`].
const US_FOOT_CONV_FACTOR: f64 = 12.0 / 39.37;
/// `units.cpp` `pj_units`: the factor `unitconvert` uses for `ft`.
pub(crate) const INTERNATIONAL_FOOT: f64 = 0.3048;

/// The factor `unitconvert` applies for a WKT linear unit of `to_metre` metres, for the units
/// compared against PROJ 7.2.0: the metre (no step), the US survey foot and the international
/// foot. `common.cpp` `UnitOfMeasure::exportToPROJString` names a unit whose factor is within
/// 1e-10, relative, of its `pj_units` entry, and `unitconvert` then uses that entry's value, not
/// the one the WKT states. `Err` is any other unit.
pub(crate) fn linear_unit_factor(to_metre: f64) -> Result<Option<f64>, ()> {
    let near = |f: f64| (f - to_metre).abs() < 1e-10 * to_metre;
    if near(1.0) {
        Ok(None)
    } else if near(INTERNATIONAL_FOOT) {
        Ok(Some(INTERNATIONAL_FOOT))
    } else if near(US_SURVEY_FOOT) {
        Ok(Some(US_SURVEY_FOOT))
    } else {
        Err(())
    }
}

/// `coordinateoperation.cpp` `Conversion::isUTM`, on the values a WKT states: the zone, and
/// whether it is a southern one. PROJ then writes `+proj=utm +zone= [+south]`, whose central
/// meridian is computed from the zone and so need not have the bits of the stated one.
pub(crate) fn utm_zone(c: &Common, to_metre: f64) -> Option<(u32, bool)> {
    let metre = (to_metre - 1.0).abs() <= 1e-10;
    let df_zone = (c.lon_0 + 183.0) / 6.0;
    // `std::round` is half away from zero, and `df_zone` is positive wherever this is used.
    let nearest = m::floor(df_zone + 0.5);
    let zone = df_zone > 0.9 && df_zone < 60.1 && (df_zone - nearest).abs() < 1e-10;
    let north = c.y_0.abs() < 1e-10;
    let south = (c.y_0 - 10000000.0).abs() < 1e-10;
    (metre
        && c.lat_0.abs() < 1e-10
        && zone
        && (c.k_0 - 0.9996).abs() < 1e-10
        && c.x_0 == 500000.0
        && (north || south))
        .then_some((nearest as u32, south))
}

/// Parameters as PROJ would read them from its text form; angles in degrees.
pub(crate) struct Common {
    pub lat_0: f64,
    pub lon_0: f64,
    pub k_0: f64,
    pub x_0: f64,
    pub y_0: f64,
}

/// `dmstor.cpp`: a plain decimal value is multiplied by `vm[0]`, and `pj_param`'s `r` code
/// returns that. dmstor applies the sign afterwards, which gives the same bits.
fn rad(deg: f64) -> f64 {
    proj_param(deg) * DEG_TO_RAD
}

fn common(s: Shape, c: &Common, f: Formula) -> Option<Projected> {
    let phi0 = rad(c.lat_0);
    let k0 = proj_param(c.k_0);
    // init.cpp
    if phi0.abs() > M_HALFPI || k0 <= 0.0 || k0.is_nan() {
        return None;
    }
    Some(Projected {
        s,
        phi0,
        lam0: rad(c.lon_0),
        k0,
        x0: proj_param(c.x_0),
        y0: proj_param(c.y_0),
        f,
        unit: None,
    })
}

impl Projected {
    /// `merc.cpp` `PROJECTION(webmerc)`: a sphere of the semi-major axis, with `k0` 1 and no origin.
    pub(crate) fn webmerc(mut s: Shape) -> Option<Projected> {
        s.es = 0.0;
        s.e = 0.0;
        s.one_es = 1.0;
        Some(Projected {
            s,
            phi0: 0.0,
            lam0: 0.0,
            k0: 1.0,
            x0: 0.0,
            y0: 0.0,
            f: Formula::WebMerc,
            unit: None,
        })
    }

    /// `tmerc.cpp` `PROJECTION(tmerc)` and `PROJECTION(utm)`, then `setup` and `setup_exact`. `c`
    /// holds the false origin in the CRS's unit, of `to_metre` metres. `None` is a definition
    /// PROJ rejects, a unit not compared, or a sphere: PROJ takes a sphere to the Evenden/Snyder
    /// spherical form, which needs `exp` and `acos` and is not ported.
    pub(crate) fn tmerc(s: Shape, c: &Common, to_metre: f64) -> Option<Projected> {
        if s.es == 0.0 {
            return None;
        }
        // GDAL gives PROJ each CRS as WKT2 text (`ogrct.cpp`, `exportToWkt` with
        // `FORMAT=WKT2_2018`), so every value is read back from 15 significant digits in its own
        // unit before PROJ does anything with it, the unit's factor included.
        let c = &Common {
            lat_0: proj_param(c.lat_0),
            lon_0: proj_param(c.lon_0),
            k_0: proj_param(c.k_0),
            x_0: proj_param(c.x_0),
            y_0: proj_param(c.y_0),
        };
        let to_metre = proj_param(to_metre);
        let unit = linear_unit_factor(to_metre).ok()?;
        let placeholder = Formula::WebMerc;
        let mut p = match utm_zone(c, to_metre) {
            // `PROJECTION(utm)`, zone given.
            Some((zone, south)) => Projected {
                s,
                phi0: 0.0,
                lam0: (f64::from(zone - 1) + 0.5) * M_PI / 30.0 - M_PI,
                k0: 0.9996,
                x0: 500000.0,
                y0: if south { 10000000.0 } else { 0.0 },
                f: placeholder,
                unit: None,
            },
            None => {
                // `Measure::getSIValue`: the false origin reaches PROJ's text in metres. io.cpp
                // `buildUnit` replaces a factor within 1e-10 of the US survey foot by its own.
                let scale = match unit {
                    Some(f) if f == US_SURVEY_FOOT => US_FOOT_CONV_FACTOR,
                    Some(_) => to_metre,
                    None => 1.0,
                };
                let metres = Common {
                    lat_0: c.lat_0,
                    lon_0: c.lon_0,
                    k_0: c.k_0,
                    x_0: c.x_0 * scale,
                    y_0: c.y_0 * scale,
                };
                let mut p = common(s, &metres, placeholder)?;
                p.unit = unit;
                p
            }
        };
        p.f = Formula::Tmerc(tmerc_setup_exact(s.n, p.k0, p.phi0));
        Some(p)
    }

    /// `aea.cpp` `setup`.
    pub(crate) fn aea(s: Shape, lat_1: f64, lat_2: f64, c: &Common) -> Option<Projected> {
        let phi1 = rad(lat_1);
        let phi2 = rad(lat_2);
        let phi0 = rad(c.lat_0);
        if phi1.abs() > M_HALFPI || phi2.abs() > M_HALFPI || (phi1 + phi2).abs() < EPS10 {
            return None;
        }
        let mut sinphi = m::sin(phi1);
        let mut n = sinphi;
        let mut cosphi = m::cos(phi1);
        let secant = (phi1 - phi2).abs() >= EPS10;
        let ellips = s.es > 0.0;
        let f = if ellips {
            let m1 = msfn(sinphi, cosphi, s.es);
            let ml1 = qsfn(sinphi, s.e, s.one_es);
            if secant {
                sinphi = m::sin(phi2);
                cosphi = m::cos(phi2);
                let m2 = msfn(sinphi, cosphi, s.es);
                let ml2 = qsfn(sinphi, s.e, s.one_es);
                if ml2 == ml1 {
                    return None;
                }
                n = (m1 * m1 - m2 * m2) / (ml2 - ml1);
                if n == 0.0 {
                    return None;
                }
            }
            let ec = 1.0 - 0.5 * s.one_es * m::log((1.0 - s.e) / (1.0 + s.e)) / s.e;
            let cc = m1 * m1 + n * ml1;
            let dd = 1.0 / n;
            let rho0 = dd * m::sqrt(cc - n * qsfn(m::sin(phi0), s.e, s.one_es));
            Aea {
                ellips,
                n,
                n2: 0.0,
                c: cc,
                dd,
                rho0,
                ec,
            }
        } else {
            if secant {
                n = 0.5 * (n + m::sin(phi2));
            }
            let n2 = n + n;
            let cc = cosphi * cosphi + n2 * sinphi;
            let dd = 1.0 / n;
            let rho0 = dd * m::sqrt(cc - n2 * m::sin(phi0));
            Aea {
                ellips,
                n,
                n2,
                c: cc,
                dd,
                rho0,
                ec: 0.0,
            }
        };
        common(s, c, Formula::Aea(f))
    }

    /// `lcc.cpp` `PROJECTION(lcc)`.
    pub(crate) fn lcc(s: Shape, lat_1: f64, lat_2: f64, c: &Common) -> Option<Projected> {
        let phi1 = rad(lat_1);
        let phi2 = rad(lat_2);
        let phi0 = rad(c.lat_0);
        if phi1.abs() > M_HALFPI || phi2.abs() > M_HALFPI || (phi1 + phi2).abs() < EPS10 {
            return None;
        }
        let sinphi = m::sin(phi1);
        let mut n = sinphi;
        let cosphi = m::cos(phi1);
        let secant = (phi1 - phi2).abs() >= EPS10;
        let (cc, rho0);
        if s.es != 0.0 {
            let m1 = msfn(sinphi, cosphi, s.es);
            let ml1 = tsfn(phi1, sinphi, s.e);
            if ml1 == 0.0 {
                return None;
            }
            if secant {
                let sinphi2 = m::sin(phi2);
                n = m::log(m1 / msfn(sinphi2, m::cos(phi2), s.es));
                if n == 0.0 {
                    return None;
                }
                let ml2 = tsfn(phi2, sinphi2, s.e);
                if ml2 == 0.0 {
                    return None;
                }
                let denom = m::log(ml1 / ml2);
                if denom == 0.0 {
                    return None;
                }
                n /= denom;
            }
            let r = m1 * m::pow(ml1, -n) / n;
            cc = r;
            rho0 = r * if (phi0.abs() - M_HALFPI).abs() < EPS10 {
                0.0
            } else {
                m::pow(tsfn(phi0, m::sin(phi0), s.e), n)
            };
        } else {
            if cosphi.abs() < EPS10 || m::cos(phi2).abs() < EPS10 {
                return None;
            }
            if secant {
                n = m::log(cosphi / m::cos(phi2))
                    / m::log(m::tan(M_FORTPI + 0.5 * phi2) / m::tan(M_FORTPI + 0.5 * phi1));
            }
            if n == 0.0 {
                return None;
            }
            cc = cosphi * m::pow(m::tan(M_FORTPI + 0.5 * phi1), n) / n;
            rho0 = if (phi0.abs() - M_HALFPI).abs() < EPS10 {
                0.0
            } else {
                cc * m::pow(m::tan(M_FORTPI + 0.5 * phi0), -n)
            };
        }
        common(s, c, Formula::Lcc(Lcc { n, c: cc, rho0 }))
    }

    /// `stere.cpp` `PROJECTION(stere)` and `setup`. No `lat_ts` means `M_HALFPI`.
    pub(crate) fn stere(s: Shape, lat_ts: Option<f64>, c: &Common) -> Option<Projected> {
        let phi0 = rad(c.lat_0);
        let k0 = proj_param(c.k_0);
        let phits = lat_ts.map_or(M_HALFPI, rad).abs();
        let t = phi0.abs();
        let mode = if (t - M_HALFPI).abs() < EPS10 {
            if phi0 < 0.0 {
                StereMode::SPole
            } else {
                StereMode::NPole
            }
        } else if t > EPS10 {
            StereMode::Obliq
        } else {
            StereMode::Equit
        };
        let mut akm1: f64;
        let (mut sin_x1, mut cos_x1) = (0.0, 0.0);
        if s.es != 0.0 {
            match mode {
                StereMode::NPole | StereMode::SPole => {
                    if (phits - M_HALFPI).abs() < EPS10 {
                        akm1 = 2.0 * k0
                            / m::sqrt(m::pow(1.0 + s.e, 1.0 + s.e) * m::pow(1.0 - s.e, 1.0 - s.e));
                    } else {
                        let t = m::sin(phits);
                        akm1 = m::cos(phits) / tsfn(phits, t, s.e);
                        let t = t * s.e;
                        akm1 /= m::sqrt(1.0 - t * t);
                    }
                }
                StereMode::Equit | StereMode::Obliq => {
                    let t = m::sin(phi0);
                    let x = 2.0 * m::atan(ssfn(phi0, t, s.e)) - M_HALFPI;
                    let t = t * s.e;
                    akm1 = 2.0 * k0 * m::cos(phi0) / m::sqrt(1.0 - t * t);
                    sin_x1 = m::sin(x);
                    cos_x1 = m::cos(x);
                }
            }
        } else {
            match mode {
                StereMode::Obliq => {
                    sin_x1 = m::sin(phi0);
                    cos_x1 = m::cos(phi0);
                    akm1 = 2.0 * k0;
                }
                StereMode::Equit => akm1 = 2.0 * k0,
                StereMode::SPole | StereMode::NPole => {
                    akm1 = if (phits - M_HALFPI).abs() >= EPS10 {
                        m::cos(phits) / m::tan(M_FORTPI - 0.5 * phits)
                    } else {
                        2.0 * k0
                    };
                }
            }
        }
        let f = Stere {
            mode,
            akm1,
            sin_x1,
            cos_x1,
        };
        common(s, c, Formula::Stere(f))
    }

    /// `pj_fwd4d`: `fwd_prepare`, the formula, `fwd_finalize`. Radians in, CRS units out.
    pub(crate) fn forward(&self, lam: f64, phi: f64) -> Point {
        if lam == f64::INFINITY || phi == f64::INFINITY {
            return Err(PointFailed);
        }
        // Computed as in fwd_prepare: NaN passes these tests, as it does in C.
        let t = (if phi < 0.0 { -phi } else { phi }) - M_HALFPI;
        if t > PJ_EPS_LAT || !(-10.0..=10.0).contains(&lam) && !lam.is_nan() {
            return Err(PointFailed);
        }
        // NaN stays NaN, as in the two comparisons in fwd_prepare.
        let phi = phi.clamp(-M_HALFPI, M_HALFPI);
        let lam = adjlon(lam);
        let lam = (lam - 0.0) - self.lam0;
        let lam = adjlon(lam);
        let (x, y) = match &self.f {
            Formula::Aea(q) => aea_fwd(q, &self.s, lam, phi)?,
            Formula::Lcc(q) => lcc_fwd(q, &self.s, self.k0, lam, phi)?,
            Formula::Stere(q) if self.s.es != 0.0 => stere_e_fwd(q, &self.s, lam, phi)?,
            Formula::Stere(q) => stere_s_fwd(q, lam, phi)?,
            Formula::WebMerc => merc_s_fwd(lam, phi)?,
            Formula::Tmerc(q) => tmerc_exact_fwd(q, lam, phi),
        };
        if x == f64::INFINITY {
            return Err(PointFailed);
        }
        // fr_meter is 1 and x * 1.0 keeps its bits: a CRS in another unit has its own step.
        let (x, y) = (x * self.s.a + self.x0, y * self.s.a + self.y0);
        Ok(match self.unit {
            // unitconvert.cpp `forward_2d`, `xy_in=m xy_out=<unit>`: the factor is 1.0 / f.
            Some(f) => {
                let factor = 1.0 / f;
                (x * factor, y * factor)
            }
            None => (x, y),
        })
    }

    /// `pj_inv4d`: `inv_prepare`, the formula, `inv_finalize`. CRS units in, radians out.
    pub(crate) fn inverse(&self, x: f64, y: f64) -> Point {
        if x == f64::INFINITY || y == f64::INFINITY {
            return Err(PointFailed);
        }
        // unitconvert.cpp `forward_2d`, `xy_in=<unit> xy_out=m`: the factor is f / 1.0.
        let (x, y) = match self.unit {
            Some(f) => (x * f, y * f),
            None => (x, y),
        };
        // to_meter is 1, as in `forward`.
        let x = (x - self.x0) * self.s.ra;
        let y = (y - self.y0) * self.s.ra;
        let (lam, phi) = match &self.f {
            Formula::Aea(q) => aea_inv(q, &self.s, x, y)?,
            Formula::Lcc(q) => lcc_inv(q, &self.s, self.k0, x, y)?,
            Formula::Stere(q) if self.s.es != 0.0 => stere_e_inv(q, &self.s, x, y)?,
            Formula::Stere(q) => stere_s_inv(q, self.phi0, x, y),
            Formula::WebMerc => (x, m::atan(m::sinh(y))),
            Formula::Tmerc(q) => tmerc_exact_inv(q, x, y),
        };
        if lam == f64::INFINITY {
            return Err(PointFailed);
        }
        let lam = lam + 0.0 + self.lam0;
        Ok((adjlon(lam), phi))
    }
}

const ETMERC_ORDER: usize = 6;
/// `tmerc.cpp`: the limit on the normalised easting, "150 degrees".
const TMERC_CE_LIMIT: f64 = 2.623395162778;

/// `tmerc.cpp` `gatg`.
fn gatg(p1: &[f64; ETMERC_ORDER], b: f64, cos_2b: f64, sin_2b: f64) -> f64 {
    let two_cos_2b = 2.0 * cos_2b;
    let mut h = 0.0;
    let mut h2 = 0.0;
    let mut h1 = p1[ETMERC_ORDER - 1];
    for &c in p1[..ETMERC_ORDER - 1].iter().rev() {
        h = -h2 + two_cos_2b * h1 + c;
        h2 = h1;
        h1 = h;
    }
    b + h * sin_2b
}

/// `tmerc.cpp` `clenS`, complex Clenshaw summation: `(R, I)`.
fn clen_s(
    a: &[f64; ETMERC_ORDER],
    sin_arg_r: f64,
    cos_arg_r: f64,
    sinh_arg_i: f64,
    cosh_arg_i: f64,
) -> (f64, f64) {
    let r = 2.0 * cos_arg_r * cosh_arg_i;
    let i = -2.0 * sin_arg_r * sinh_arg_i;
    let (mut hi1, mut hr1, mut hi) = (0.0, 0.0, 0.0);
    let mut hr = a[ETMERC_ORDER - 1];
    for &c in a[..ETMERC_ORDER - 1].iter().rev() {
        let hr2 = hr1;
        let hi2 = hi1;
        hr1 = hr;
        hi1 = hi;
        hr = -hr2 + r * hr1 - i * hi1 + c;
        hi = -hi2 + i * hr1 + r * hi1;
    }
    let r = sin_arg_r * cosh_arg_i;
    let i = cos_arg_r * sinh_arg_i;
    (r * hr - i * hi, r * hi + i * hr)
}

/// `tmerc.cpp` `clens`, real Clenshaw summation.
fn clens(a: &[f64; ETMERC_ORDER], arg_r: f64) -> f64 {
    let cos_arg_r = m::cos(arg_r);
    let r = 2.0 * cos_arg_r;
    let mut hr1 = 0.0;
    let mut hr = a[ETMERC_ORDER - 1];
    for &c in a[..ETMERC_ORDER - 1].iter().rev() {
        let hr2 = hr1;
        hr1 = hr;
        hr = -hr2 + r * hr1 + c;
    }
    m::sin(arg_r) * hr
}

/// `tmerc.cpp` `setup_exact`. Each quotient of two constants is C's constant division.
#[rustfmt::skip]
fn tmerc_setup_exact(n: f64, k0: f64, phi0: f64) -> Tmerc {
    let mut cgb = [0.0; 6];
    let mut cbg = [0.0; 6];
    let mut utg = [0.0; 6];
    let mut gtu = [0.0; 6];
    let mut np = n;
    cgb[0] = n*( 2.0 + n*(-2.0/3.0  + n*(-2.0      + n*(116.0/45.0 + n*(26.0/45.0 +
                n*(-2854.0/675.0 ))))));
    cbg[0] = n*(-2.0 + n*( 2.0/3.0  + n*( 4.0/3.0  + n*(-82.0/45.0 + n*(32.0/45.0 +
                n*( 4642.0/4725.0))))));
    np     *= n;
    cgb[1] = np*(7.0/3.0 + n*( -8.0/5.0  + n*(-227.0/45.0 + n*(2704.0/315.0 +
                n*( 2323.0/945.0)))));
    cbg[1] = np*(5.0/3.0 + n*(-16.0/15.0 + n*( -13.0/9.0  + n*( 904.0/315.0 +
                n*(-1522.0/945.0)))));
    np     *= n;
    cgb[2] = np*( 56.0/15.0  + n*(-136.0/35.0 + n*(-1262.0/105.0 +
                n*( 73814.0/2835.0))));
    cbg[2] = np*(-26.0/15.0  + n*(  34.0/21.0 + n*(    8.0/5.0   +
                n*(-12686.0/2835.0))));
    np     *= n;
    cgb[3] = np*(4279.0/630.0 + n*(-332.0/35.0 + n*(-399572.0/14175.0)));
    cbg[3] = np*(1237.0/630.0 + n*( -12.0/5.0  + n*( -24832.0/14175.0)));
    np     *= n;
    cgb[4] = np*(4174.0/315.0 + n*(-144838.0/6237.0 ));
    cbg[4] = np*(-734.0/315.0 + n*( 109598.0/31185.0));
    np     *= n;
    cgb[5] = np*(601676.0/22275.0 );
    cbg[5] = np*(444337.0/155925.0);

    np = n*n;
    let qn = k0/(1.0 + n) * (1.0 + np*(1.0/4.0 + np*(1.0/64.0 + np/256.0)));
    utg[0] = n*(-0.5  + n*( 2.0/3.0 + n*(-37.0/96.0 + n*( 1.0/360.0 +
                n*(  81.0/512.0 + n*(-96199.0/604800.0))))));
    gtu[0] = n*( 0.5  + n*(-2.0/3.0 + n*(  5.0/16.0 + n*(41.0/180.0 +
                n*(-127.0/288.0 + n*(  7891.0/37800.0 ))))));
    utg[1] = np*(-1.0/48.0 + n*(-1.0/15.0 + n*(437.0/1440.0 + n*(-46.0/105.0 +
                n*( 1118711.0/3870720.0)))));
    gtu[1] = np*(13.0/48.0 + n*(-3.0/5.0  + n*(557.0/1440.0 + n*(281.0/630.0 +
                n*(-1983433.0/1935360.0)))));
    np      *= n;
    utg[2] = np*(-17.0/480.0 + n*(  37.0/840.0 + n*(  209.0/4480.0  +
                n*( -5569.0/90720.0 ))));
    gtu[2] = np*( 61.0/240.0 + n*(-103.0/140.0 + n*(15061.0/26880.0 +
                n*(167603.0/181440.0))));
    np      *= n;
    utg[3] = np*(-4397.0/161280.0 + n*(  11.0/504.0 + n*( 830251.0/7257600.0)));
    gtu[3] = np*(49561.0/161280.0 + n*(-179.0/168.0 + n*(6601661.0/7257600.0)));
    np     *= n;
    utg[4] = np*(-4583.0/161280.0 + n*(  108847.0/3991680.0));
    gtu[4] = np*(34729.0/80640.0  + n*(-3418889.0/1995840.0));
    np     *= n;
    utg[5] = np*(-20648693.0/638668800.0);
    gtu[5] = np*(212378941.0/319334400.0);

    // Gaussian latitude of the origin latitude, then the origin northing minus the true one.
    let z = gatg(&cbg, phi0, m::cos(2.0*phi0), m::sin(2.0*phi0));
    let zb = -qn*(z + clens(&gtu, 2.0*z));
    Tmerc { qn, zb, cgb, cbg, utg, gtu }
}

/// `tmerc.cpp` `exact_e_fwd`. A point past the limit is `HUGE_VAL`, which the caller reports.
fn tmerc_exact_fwd(q: &Tmerc, lam: f64, phi: f64) -> (f64, f64) {
    // Ellipsoidal to Gaussian latitude, then to the complementary sphere.
    let cn = gatg(&q.cbg, phi, m::cos(2.0 * phi), m::sin(2.0 * phi));
    let sin_cn = m::sin(cn);
    let cos_cn = m::cos(cn);
    let sin_ce = m::sin(lam);
    let cos_ce = m::cos(lam);

    let cos_cn_cos_ce = cos_cn * cos_ce;
    let mut cn = m::atan2(sin_cn, cos_cn_cos_ce);

    let inv_denom_tan_ce = 1.0 / m::hypot(sin_cn, cos_cn_cos_ce);
    let tan_ce = sin_ce * cos_cn * inv_denom_tan_ce;

    let mut ce = m::asinh(tan_ce);

    let two_inv_denom_tan_ce = 2.0 * inv_denom_tan_ce;
    let two_inv_denom_tan_ce_square = two_inv_denom_tan_ce * inv_denom_tan_ce;
    let tmp_r = cos_cn_cos_ce * two_inv_denom_tan_ce_square;
    let sin_arg_r = sin_cn * tmp_r;
    let cos_arg_r = cos_cn_cos_ce * tmp_r - 1.0;

    let sinh_arg_i = tan_ce * two_inv_denom_tan_ce;
    let cosh_arg_i = two_inv_denom_tan_ce_square - 1.0;

    let (d_cn, d_ce) = clen_s(&q.gtu, sin_arg_r, cos_arg_r, sinh_arg_i, cosh_arg_i);
    cn += d_cn;
    ce += d_ce;
    if ce.abs() <= TMERC_CE_LIMIT {
        (q.qn * ce, q.qn * cn + q.zb)
    } else {
        (f64::INFINITY, f64::INFINITY)
    }
}

/// `tmerc.cpp` `exact_e_inv`. A point past the limit is `HUGE_VAL`, which the caller reports.
fn tmerc_exact_inv(q: &Tmerc, x: f64, y: f64) -> (f64, f64) {
    let mut cn = (y - q.zb) / q.qn;
    let mut ce = x / q.qn;
    // The source tests `fabs(Ce) <= limit`, so NaN takes the failing branch, as here.
    if ce.abs() > TMERC_CE_LIMIT || ce.is_nan() {
        return (f64::INFINITY, f64::INFINITY);
    }
    // Normalised N, E to the complementary sphere.
    let sin_arg_r = m::sin(2.0 * cn);
    let cos_arg_r = m::cos(2.0 * cn);

    let exp_2_ce = m::tmerc_exp(2.0 * ce);
    let half_inv_exp_2_ce = 0.5 / exp_2_ce;
    let sinh_arg_i = 0.5 * exp_2_ce - half_inv_exp_2_ce;
    let cosh_arg_i = 0.5 * exp_2_ce + half_inv_exp_2_ce;

    let (d_cn, d_ce) = clen_s(&q.utg, sin_arg_r, cos_arg_r, sinh_arg_i, cosh_arg_i);
    cn += d_cn;
    ce += d_ce;

    // Complementary sphere to Gaussian latitude and longitude.
    let sin_cn = m::sin(cn);
    let cos_cn = m::cos(cn);

    let sinh_ce = m::sinh(ce);
    let ce = m::atan2(sinh_ce, cos_cn);
    let modulus_ce = m::hypot(sinh_ce, cos_cn);
    let cn = m::atan2(sin_cn, modulus_ce);

    // Gaussian to ellipsoidal latitude.
    let tmp = 2.0 * modulus_ce / (sinh_ce * sinh_ce + 1.0);
    let sin_2_cn = sin_cn * tmp;
    let cos_2_cn = tmp * modulus_ce - 1.0;
    (ce, gatg(&q.cgb, cn, cos_2_cn, sin_2_cn))
}

/// `merc.cpp` `merc_s_forward` and `logtanpfpim1`, with `k0` 1.
fn merc_s_fwd(lam: f64, phi: f64) -> Point {
    if (phi.abs() - M_HALFPI).abs() <= 1.0e-10 {
        return Err(PointFailed);
    }
    let y = if phi.abs() <= f64::EPSILON {
        // `log1p(x)` is `x` to within this `x`'s rounding.
        phi
    } else {
        m::log(m::tan(std::f64::consts::FRAC_PI_4 + 0.5 * phi))
    };
    Ok((lam, y))
}

fn aea_fwd(q: &Aea, s: &Shape, lam: f64, phi: f64) -> Point {
    let rho = q.c
        - if q.ellips {
            q.n * qsfn(m::sin(phi), s.e, s.one_es)
        } else {
            q.n2 * m::sin(phi)
        };
    if rho < 0.0 {
        return Err(PointFailed);
    }
    let rho = q.dd * m::sqrt(rho);
    let lam = lam * q.n;
    Ok((rho * m::sin(lam), q.rho0 - rho * m::cos(lam)))
}

fn aea_inv(q: &Aea, s: &Shape, x: f64, y: f64) -> Point {
    let (mut x, mut y) = (x, q.rho0 - y);
    let mut rho = m::hypot(x, y);
    if rho == 0.0 {
        return Ok((0.0, if q.n > 0.0 { M_HALFPI } else { -M_HALFPI }));
    }
    if q.n < 0.0 {
        rho = -rho;
        x = -x;
        y = -y;
    }
    let mut phi = rho / q.dd;
    if q.ellips {
        phi = (q.c - phi * phi) / q.n;
        if (q.ec - phi.abs()).abs() > 1e-7 {
            if phi.abs() > 2.0 {
                return Err(PointFailed);
            }
            phi = aea_phi1(phi, s.e, s.one_es);
            if phi == f64::INFINITY {
                return Err(PointFailed);
            }
        } else {
            phi = if phi < 0.0 { -M_HALFPI } else { M_HALFPI };
        }
    } else {
        phi = (q.c - phi * phi) / q.n2;
        phi = if phi.abs() <= 1.0 {
            m::asin(phi)
        } else if phi < 0.0 {
            -M_HALFPI
        } else {
            M_HALFPI
        };
    }
    Ok((m::atan2(x, y) / q.n, phi))
}

fn lcc_fwd(q: &Lcc, s: &Shape, k0: f64, lam: f64, phi: f64) -> Point {
    let rho = if (phi.abs() - M_HALFPI).abs() < EPS10 {
        if phi * q.n <= 0.0 {
            return Err(PointFailed);
        }
        0.0
    } else {
        q.c * if s.es != 0.0 {
            m::pow(tsfn(phi, m::sin(phi), s.e), q.n)
        } else {
            m::pow(m::tan(M_FORTPI + 0.5 * phi), -q.n)
        }
    };
    let lam = lam * q.n;
    Ok((k0 * (rho * m::sin(lam)), k0 * (q.rho0 - rho * m::cos(lam))))
}

fn lcc_inv(q: &Lcc, s: &Shape, k0: f64, x: f64, y: f64) -> Point {
    let x = x / k0;
    let y = y / k0;
    let (mut x, mut y) = (x, q.rho0 - y);
    let mut rho = m::hypot(x, y);
    if rho == 0.0 {
        return Ok((0.0, if q.n > 0.0 { M_HALFPI } else { -M_HALFPI }));
    }
    if q.n < 0.0 {
        rho = -rho;
        x = -x;
        y = -y;
    }
    let phi = if s.es != 0.0 {
        let phi = phi2(m::pow(rho / q.c, 1.0 / q.n), s.e)?;
        if phi == f64::INFINITY {
            return Err(PointFailed);
        }
        phi
    } else {
        2.0 * m::atan(m::pow(q.c / rho, 1.0 / q.n)) - M_HALFPI
    };
    Ok((m::atan2(x, y) / q.n, phi))
}

// stere.cpp ssfn_
fn ssfn(phit: f64, sinphi: f64, eccen: f64) -> f64 {
    let sinphi = sinphi * eccen;
    m::tan(0.5 * (M_HALFPI + phit)) * m::pow((1.0 - sinphi) / (1.0 + sinphi), 0.5 * eccen)
}

fn stere_e_fwd(q: &Stere, s: &Shape, lam: f64, phi: f64) -> Point {
    let mut coslam = m::cos(lam);
    let sinlam = m::sin(lam);
    let mut sinphi = m::sin(phi);
    let mut phi = phi;
    let (mut sin_x, mut cos_x) = (0.0, 0.0);
    if matches!(q.mode, StereMode::Obliq | StereMode::Equit) {
        let x = 2.0 * m::atan(ssfn(phi, sinphi, s.e)) - M_HALFPI;
        sin_x = m::sin(x);
        cos_x = m::cos(x);
    }
    let (x, y);
    match q.mode {
        StereMode::Obliq => {
            let denom = q.cos_x1 * (1.0 + q.sin_x1 * sin_x + q.cos_x1 * cos_x * coslam);
            if denom == 0.0 {
                return Err(PointFailed);
            }
            let a = q.akm1 / denom;
            y = a * (q.cos_x1 * sin_x - q.sin_x1 * cos_x * coslam);
            x = a * cos_x;
        }
        StereMode::Equit => {
            let mut a = 0.0;
            if 1.0 + cos_x * coslam == 0.0 {
                y = f64::INFINITY;
            } else {
                a = q.akm1 / (1.0 + cos_x * coslam);
                y = a * sin_x;
            }
            x = a * cos_x;
        }
        StereMode::SPole | StereMode::NPole => {
            if q.mode == StereMode::SPole {
                phi = -phi;
                coslam = -coslam;
                sinphi = -sinphi;
            }
            x = q.akm1 * tsfn(phi, sinphi, s.e);
            y = -x * coslam;
        }
    }
    Ok((x * sinlam, y))
}

fn stere_s_fwd(q: &Stere, lam: f64, phi: f64) -> Point {
    let sinphi = m::sin(phi);
    let cosphi = m::cos(phi);
    let mut coslam = m::cos(lam);
    let sinlam = m::sin(lam);
    let mut phi = phi;
    match q.mode {
        StereMode::Equit | StereMode::Obliq => {
            let d = if q.mode == StereMode::Equit {
                1.0 + cosphi * coslam
            } else {
                1.0 + q.sin_x1 * sinphi + q.cos_x1 * cosphi * coslam
            };
            if d <= EPS10 {
                return Err(PointFailed);
            }
            let a = q.akm1 / d;
            let x = a * cosphi * sinlam;
            let y = a * if q.mode == StereMode::Equit {
                sinphi
            } else {
                q.cos_x1 * sinphi - q.sin_x1 * cosphi * coslam
            };
            Ok((x, y))
        }
        StereMode::NPole | StereMode::SPole => {
            if q.mode == StereMode::NPole {
                coslam = -coslam;
                phi = -phi;
            }
            if (phi - M_HALFPI).abs() < 1.0e-8 {
                return Err(PointFailed);
            }
            let y = q.akm1 * m::tan(M_FORTPI + 0.5 * phi);
            Ok((sinlam * y, y * coslam))
        }
    }
}

fn stere_e_inv(q: &Stere, s: &Shape, x: f64, y: f64) -> Point {
    let (mut x, mut y) = (x, y);
    let rho = m::hypot(x, y);
    let (mut tp, mut phi_l, halfpi, halfe);
    match q.mode {
        StereMode::Obliq | StereMode::Equit => {
            tp = 2.0 * m::atan2(rho * q.cos_x1, q.akm1);
            let cosphi = m::cos(tp);
            let sinphi = m::sin(tp);
            phi_l = if rho == 0.0 {
                m::asin(cosphi * q.sin_x1)
            } else {
                m::asin(cosphi * q.sin_x1 + (y * sinphi * q.cos_x1 / rho))
            };
            tp = m::tan(0.5 * (M_HALFPI + phi_l));
            x *= sinphi;
            y = rho * q.cos_x1 * cosphi - y * q.sin_x1 * sinphi;
            halfpi = M_HALFPI;
            halfe = 0.5 * s.e;
        }
        StereMode::NPole | StereMode::SPole => {
            if q.mode == StereMode::NPole {
                y = -y;
            }
            tp = -rho / q.akm1;
            phi_l = M_HALFPI - 2.0 * m::atan(tp);
            halfpi = -M_HALFPI;
            halfe = -0.5 * s.e;
        }
    }
    for _ in 0..8 {
        let sinphi = s.e * m::sin(phi_l);
        let phi = 2.0 * m::atan(tp * m::pow((1.0 + sinphi) / (1.0 - sinphi), halfe)) - halfpi;
        if (phi_l - phi).abs() < 1.0e-10 {
            let phi = if q.mode == StereMode::SPole {
                -phi
            } else {
                phi
            };
            let lam = if x == 0.0 && y == 0.0 {
                0.0
            } else {
                m::atan2(x, y)
            };
            return Ok((lam, phi));
        }
        phi_l = phi;
    }
    Err(PointFailed)
}

fn stere_s_inv(q: &Stere, phi0: f64, x: f64, y: f64) -> (f64, f64) {
    let mut y = y;
    let rh = m::hypot(x, y);
    let c = 2.0 * m::atan(rh / q.akm1);
    let sinc = m::sin(c);
    let cosc = m::cos(c);
    let mut lam = 0.0;
    let phi;
    match q.mode {
        StereMode::Equit => {
            phi = if rh.abs() <= EPS10 {
                0.0
            } else {
                m::asin(y * sinc / rh)
            };
            if cosc != 0.0 || x != 0.0 {
                lam = m::atan2(x * sinc, cosc * rh);
            }
        }
        StereMode::Obliq => {
            phi = if rh.abs() <= EPS10 {
                phi0
            } else {
                m::asin(cosc * q.sin_x1 + y * sinc * q.cos_x1 / rh)
            };
            let c = cosc - q.sin_x1 * m::sin(phi);
            if c != 0.0 || x != 0.0 {
                lam = m::atan2(x * sinc * q.cos_x1, c * rh);
            }
        }
        StereMode::NPole | StereMode::SPole => {
            if q.mode == StereMode::NPole {
                y = -y;
            }
            phi = if rh.abs() <= EPS10 {
                phi0
            } else {
                m::asin(if q.mode == StereMode::SPole {
                    -cosc
                } else {
                    cosc
                })
            };
            lam = if x == 0.0 && y == 0.0 {
                0.0
            } else {
                m::atan2(x, y)
            };
        }
    }
    (lam, phi)
}

/// `unitconvert.cpp` `xy_in=deg xy_out=rad`: the factor is `0.017453292519943296 / 1.0`.
#[inline]
pub(crate) fn deg_to_rad(v: f64) -> f64 {
    v * DEG_TO_RAD
}

/// `unitconvert.cpp` `xy_in=rad xy_out=deg`: the factor is `1.0 / 0.017453292519943296`.
#[inline]
pub(crate) fn rad_to_deg(v: f64) -> f64 {
    v * (1.0 / DEG_TO_RAD)
}

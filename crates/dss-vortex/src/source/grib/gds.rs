//! Section 3 to the georeference Vortex hands its warp, from the GRIB bytes alone.
//!
//! Sources, NetCDF-Java 5.5.3: `Grib2Gds` (templates and earth), `LambertConformal` and
//! `Stereographic` (`latLonToProj`), `Array.makeArray`, `CoordinateAxis1D.makeEdges` and
//! `calcIsRegular`, `CoordinateSystem.getProjection`. Vortex 1.1.0:
//! `GridDatasetReader.getGridDefinition`, `NetcdfDataReader.shiftGrid`, `WktFactory.createWkt`.

use super::georef_maths as m;
use super::numbers::{
    int4, nearly_equals_f32, nearly_equals_f64, pow10_f64, wkt_number, UNDEFINED,
};
use crate::error::VortexError;
use crate::units::km_to_m_or_refuse;

/// Exactly what the Vortex grid definition holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Georeference {
    pub nx: usize,
    pub ny: usize,
    pub origin_x: f64,
    pub origin_y: f64,
    pub dx: f64,
    pub dy: f64,
    /// The radius in the WKT, in metres: not always the file's (`Grib2Gds.PolarStereographic`
    /// uses the three-argument `Stereographic` constructor; lat/lon uses `LatLonProjection()`).
    pub wkt_radius: f64,
    pub wkt: String,
    /// `Grib2Gds.makeHorizCoordSys` start point, in degrees or km, before rounding to `f32`.
    pub start_x: f64,
    pub start_y: f64,
    pub template: u16,
    pub scan_mode: u8,
}

fn decode_error(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: "GRIB2",
        message: message.into(),
    }
}

fn refused(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "GRIB2 grid".to_string(),
        reason: reason.into(),
    }
}

struct Section<'a>(&'a [u8]);

impl Section<'_> {
    fn octet(&self, n: usize) -> Result<u8, VortexError> {
        self.0
            .get(n - 1)
            .copied()
            .ok_or_else(|| decode_error(format!("section 3 ends before octet {n}")))
    }

    /// `Grib2Gds.getOctet4`.
    fn int4(&self, n: usize) -> Result<i32, VortexError> {
        Ok(int4([
            self.octet(n)?,
            self.octet(n + 1)?,
            self.octet(n + 2)?,
            self.octet(n + 3)?,
        ]))
    }

    /// `Grib2Gds.getScaledValue`: `(float) (value / Math.pow(10, factor))`, or `(float) value`.
    /// The factor is `GribNumbers.convertSignedByte`.
    fn scaled(&self, n: usize) -> Result<Option<f32>, VortexError> {
        let byte = self.octet(n)?;
        let magnitude = i32::from(byte & 0x7f);
        let factor = if byte & 0x80 != 0 {
            -magnitude
        } else {
            magnitude
        };
        let value = self.int4(n + 1)?;
        if factor == 0 {
            return Ok(Some(value as f32));
        }
        Ok(pow10_f64(factor).map(|p| (f64::from(value) / p) as f32))
    }

    fn u16(&self, n: usize) -> Result<u16, VortexError> {
        Ok(u16::from_be_bytes([self.octet(n)?, self.octet(n + 1)?]))
    }
}

/// `Grib2Gds.getEarth`, for a spherical earth; `None` for an ellipsoid.
fn spherical_radius(section: &Section) -> Result<Option<f64>, VortexError> {
    Ok(match section.octet(15)? {
        0 => Some(6367470.0),
        1 => {
            let mut radius = section
                .scaled(16)?
                .ok_or_else(|| refused("earth radius scale is out of range"))?;
            if radius < 6000000.0 {
                radius *= 1000.0; // "bad units"
            }
            Some(f64::from(radius))
        }
        2 | 3 | 4 | 5 | 7 | 9 => None,
        6 => Some(6371229.0),
        8 => Some(6371200.0),
        // Earth(): WGS84_EARTH_RADIUS_METERS.
        _ => Some(6371229.0),
    })
}

const SCALE6: f32 = 1.0e-6;
pub(crate) const DEG_TO_RAD: f64 = 0.017453292519943295;
const RAD_TO_DEG: f64 = 57.29577951308232;
const PI_OVER_4: f64 = std::f64::consts::PI / 4.0;
/// `Earth.WGS84_EARTH_RADIUS_METERS`, and `ProjectionImpl.EARTH_RADIUS` in km.
pub(crate) const DEFAULT_RADIUS_M: f64 = 6371229.0;
const DEFAULT_RADIUS_KM: f64 = 6371.229;
/// `ProjectionImpl.TOLERANCE`.
const TOLERANCE: f64 = 1.0e-6;

/// `Math.IEEEremainder(x, 360.0)`. The remainder is exact, so this is not a libm dependency in
/// any way that can vary: `%` is defined to be exact.
fn ieee_remainder_360(x: f64) -> f64 {
    let mut r = x % 360.0;
    let half = 180.0;
    if r.abs() > half {
        r -= 360.0f64.copysign(r);
    } else if r.abs() == half {
        let quotient = ((x - r) / 360.0).abs();
        if quotient % 2.0 == 1.0 {
            r -= 360.0f64.copysign(r);
        }
    }
    r
}

/// `LatLonPoints.lonNormal`.
fn lon_normal(lon: f64) -> f64 {
    if !(-180.0..=180.0).contains(&lon) {
        ieee_remainder_360(lon)
    } else {
        lon
    }
}

/// `LatLonPoints.latNormal`.
fn lat_normal(lat: f64) -> f64 {
    lat.clamp(-90.0, 90.0)
}

/// `LambertConformal.precalculate` and `latLonToProj`, in km.
fn lambert_start(
    lat0_deg: f64,
    lon0_deg: f64,
    par1: f64,
    par2: f64,
    radius_km: f64,
    la1: f64,
    lo1: f64,
) -> Result<(f64, f64), VortexError> {
    let lat0 = lat0_deg * DEG_TO_RAD;
    let lon0 = lon0_deg * DEG_TO_RAD;
    if lat0.abs() > std::f64::consts::FRAC_PI_2 || par1.abs() >= 90.0 || par2.abs() >= 90.0 {
        return Err(refused("Lambert parameters are out of range"));
    }
    let par1r = par1 * DEG_TO_RAD;
    let par2r = par2 * DEG_TO_RAD;
    let t1 = m::tan(std::f64::consts::PI / 4.0 + par1r / 2.0);
    let t2 = m::tan(std::f64::consts::PI / 4.0 + par2r / 2.0);
    let n = if (par2 - par1).abs() < TOLERANCE {
        m::sin(par1r)
    } else {
        m::log(m::cos(par1r) / m::cos(par2r)) / m::log(t2 / t1)
    };
    let t1n = m::pow(t1, n);
    let f = m::cos(par1r) * t1n / n;
    let earth_radius_times_f = radius_km * f;
    let t0n = m::pow(m::tan(std::f64::consts::PI / 4.0 + lat0 / 2.0), n);
    let rho = earth_radius_times_f / t0n;
    let lon0_degrees = lon0 * RAD_TO_DEG;

    let from_lat = lat_normal(la1) * DEG_TO_RAD;
    let from_lon = lon_normal(lo1);
    let dlon = lon_normal(from_lon - lon0_degrees);
    let theta = n * (dlon * DEG_TO_RAD);
    let tn = m::pow(m::tan(PI_OVER_4 + from_lat / 2.0), n);
    let r = earth_radius_times_f / tn;
    Ok((r * m::sin(theta), rho - r * m::cos(theta)))
}

/// `Stereographic` with the three-argument constructor and `latLonToProj`, in km.
pub(crate) fn stereographic_start(
    latt_deg: f64,
    lont_deg: f64,
    scale: f64,
    la1: f64,
    lo1: f64,
) -> (f64, f64) {
    let latt = latt_deg * DEG_TO_RAD;
    let lont = lont_deg * DEG_TO_RAD;
    let scaled = scale * DEFAULT_RADIUS_KM;
    let sinlatt = m::sin(latt);
    let coslatt = m::cos(latt);
    let mut lat = lat_normal(la1) * DEG_TO_RAD;
    let lon = lon_normal(lo1) * DEG_TO_RAD;
    if (lat + latt).abs() <= TOLERANCE {
        lat = -latt * (1.0 - TOLERANCE);
    }
    let sdlon = m::sin(lon - lont);
    let cdlon = m::cos(lon - lont);
    let sinlat = m::sin(lat);
    let coslat = m::cos(lat);
    let k = 2.0 * scaled / (1.0 + sinlatt * sinlat + coslatt * coslat * cdlon);
    (
        k * coslat * sdlon,
        k * (coslatt * sinlat - sinlatt * coslat * cdlon),
    )
}

/// `Array.makeArray(DataType.FLOAT, n, start, delta)` then `CoordinateAxis1D`: the axis values
/// are `f32`, the edges `f64`; `getGridDefinition` keeps the first edge and the mean spacing.
/// Refuses an axis `calcIsRegular` would call irregular, because `getGridData` then resamples.
pub(crate) fn axis(start: f64, delta: f64, n: usize) -> Result<(f64, f64), VortexError> {
    let value = |i: usize| f64::from((start + i as f64 * delta) as f32);
    if n < 2 {
        // makeEdges: both edges equal the coordinate, so the spacing is 0.
        return Ok((value(0), 0.0));
    }
    let edge1 = (value(0) + value(1)) / 2.0;
    let first = value(0) - (edge1 - value(0));
    let last_inner = (value(n - 2) + value(n - 1)) / 2.0;
    let last = value(n - 1) + (value(n - 1) - last_inner);

    let increment = (value(n - 1) - value(0)) / (n - 1) as f64;
    for i in 1..n {
        if !nearly_equals_f64(value(i) - value(i - 1), increment, 5.0e-3) {
            return Err(refused(format!(
                "axis of {n} points is not regular at {i}; Vortex resamples such an axis, and no \
                 sampled product has one"
            )));
        }
    }
    Ok((first, (last - first) / n as f64))
}

fn checked_points(
    section: &Section,
    nx_octet: usize,
) -> Result<(usize, usize, usize), VortexError> {
    let nx = section.int4(nx_octet)?;
    let ny = section.int4(nx_octet + 4)?;
    let points = u32::from_be_bytes([
        section.octet(7)?,
        section.octet(8)?,
        section.octet(9)?,
        section.octet(10)?,
    ]);
    let (nx, ny) = match (usize::try_from(nx), usize::try_from(ny)) {
        (Ok(x), Ok(y)) if x > 0 && y > 0 => (x, y),
        _ => return Err(decode_error(format!("grid is {nx} x {ny}"))),
    };
    if nx.checked_mul(ny) != Some(points as usize) {
        return Err(decode_error(format!(
            "grid is {nx} x {ny}, section 3 declares {points} points"
        )));
    }
    Ok((nx, ny, points as usize))
}

/// Scan bits NetCDF-Java leaves as they are. Everything else has no sampled product, so there is
/// no reference to match.
fn check_scan_mode(scan_mode: u8) -> Result<(), VortexError> {
    if scan_mode == 0 || scan_mode == 64 {
        return Ok(());
    }
    Err(refused(format!(
        "scan mode {scan_mode}: no sampled product uses it"
    )))
}

/// The grid's total point count, without building the rest. Needed by the data layer.
pub(super) fn total_points(section3: &[u8]) -> Result<usize, VortexError> {
    let section = Section(section3);
    Ok(checked_points(&section, 31)?.2)
}

/// Read a georeference. Thin grids, rotated grids and ellipsoidal projected grids are refused.
/// Not public: the axis check walks every point, so callers apply the cell limit first.
pub(super) fn read_georeference(section3: &[u8]) -> Result<Georeference, VortexError> {
    let s = Section(section3);
    let template = s.u16(13)?;
    if s.octet(11)? != 0 {
        return Err(refused("quasi-regular grid"));
    }
    let (nx, ny, _) = checked_points(&s, 31)?;
    match template {
        0 => lat_lon(&s, nx, ny),
        20 => polar(&s, nx, ny),
        30 => lambert(&s, nx, ny),
        other => Err(VortexError::Unsupported {
            what: format!("GRIB2 grid template 3.{other}"),
        }),
    }
}

/// `Grib2Gds.LatLon`: constructor, `finish` and `makeHorizCoordSys`.
fn lat_lon(s: &Section, nx: usize, ny: usize) -> Result<Georeference, VortexError> {
    let scan_mode = s.octet(72)?;
    check_scan_mode(scan_mode)?;
    let basic = s.int4(39)?;
    let subdivisions = s.int4(43)?;
    let scale = if basic == 0 || basic == UNDEFINED || subdivisions == UNDEFINED {
        SCALE6
    } else {
        basic as f32 / subdivisions as f32
    };
    let la1 = s.int4(47)? as f32 * scale;
    let mut lo1 = s.int4(51)? as f32 * scale;
    let la2 = s.int4(56)? as f32 * scale;
    let mut lo2 = s.int4(60)? as f32 * scale;
    if lo2 < lo1 {
        lo2 += 360.0;
    }
    if nearly_equals_f32(lo1, lo2) {
        lo1 -= 360.0;
    }
    let mut delta_lon = s.int4(64)? as f32 * scale;
    if nx > 1 {
        // Grib2Gds.java lines 419-425: a stored delta that disagrees with the computed one loses.
        let computed = (lo2 - lo1) / (nx as i32 - 1) as f32;
        if !nearly_equals_f32(delta_lon, computed) {
            delta_lon = computed;
        }
    }
    let mut delta_lat = s.int4(68)? as f32 * scale;
    if la2 < la1 {
        delta_lat = -delta_lat;
    }
    if ny > 1 {
        let computed = (la2 - la1) / (ny as i32 - 1) as f32;
        if !nearly_equals_f32(delta_lat, computed) {
            delta_lat = computed;
        }
    }
    let start_x = f64::from(lo1);
    let start_y = f64::from(la1);
    let (mut origin_x, dx) = axis(start_x, f64::from(delta_lon), nx)?;
    let (origin_y, dy) = axis(start_y, f64::from(delta_lat), ny)?;
    // NetcdfDataReader.shiftGrid, after the axes are built.
    if origin_x > 180.0 || origin_x + dx * nx as f64 > 180.0 {
        origin_x -= 360.0;
    }
    Ok(Georeference {
        nx,
        ny,
        origin_x,
        origin_y,
        dx,
        dy,
        wkt_radius: DEFAULT_RADIUS_M,
        wkt: geographic_wkt()?,
        start_x,
        start_y,
        template: 0,
        scan_mode,
    })
}

/// `Grib2Gds.PolarStereographic`, for a spherical earth.
fn polar(s: &Section, nx: usize, ny: usize) -> Result<Georeference, VortexError> {
    let scan_mode = s.octet(65)?;
    check_scan_mode(scan_mode)?;
    if spherical_radius(s)?.is_none() {
        return Err(refused(
            "polar stereographic on an ellipsoid: no sampled product has one",
        ));
    }
    let la1 = s.int4(39)? as f32 * SCALE6;
    let lo1 = s.int4(43)? as f32 * SCALE6;
    let lad = s.int4(48)? as f32 * SCALE6;
    let lov = s.int4(52)? as f32 * SCALE6;
    let d_x = s.int4(56)? as f32 * SCALE6;
    let d_y = s.int4(60)? as f32 * SCALE6;
    let north = s.octet(64)? & 128 == 0;
    let lat_origin = if north { 90.0 } else { -90.0 };
    // makeHorizCoordSys: GribNumbers.isUndefined(lad) compares with -9999.0.
    let k0 = if f64::from(lad) == -9999.0 {
        0.9330127018922193
    } else {
        (1.0 + m::sin(f64::from(lad).abs() * DEG_TO_RAD)) / 2.0
    };
    let (start_x, start_y) = stereographic_start(
        lat_origin,
        f64::from(lov),
        k0,
        f64::from(la1),
        f64::from(lo1),
    );
    let (origin_km, dx_km) = axis(start_x, f64::from(d_x), nx)?;
    let (origin_y_km, dy_km) = axis(start_y, f64::from(d_y), ny)?;
    let wkt = stereographic_wkt(lat_origin, f64::from(lov), k0)?;
    Ok(Georeference {
        nx,
        ny,
        // NetcdfDataReader.scaleGrid through indriya, as Vortex does.
        origin_x: km_to_m_or_refuse(origin_km, "GRIB2 grid")?,
        origin_y: km_to_m_or_refuse(origin_y_km, "GRIB2 grid")?,
        dx: km_to_m_or_refuse(dx_km, "GRIB2 grid")?,
        dy: km_to_m_or_refuse(dy_km, "GRIB2 grid")?,
        wkt_radius: DEFAULT_RADIUS_M,
        wkt,
        start_x,
        start_y,
        template: 20,
        scan_mode,
    })
}

/// `Grib2Gds.LambertConformal`, for a spherical earth.
fn lambert(s: &Section, nx: usize, ny: usize) -> Result<Georeference, VortexError> {
    let scan_mode = s.octet(65)?;
    check_scan_mode(scan_mode)?;
    let radius = spherical_radius(s)?
        .ok_or_else(|| refused("Lambert on an ellipsoid: no sampled product has one"))?;
    let la1 = s.int4(39)? as f32 * SCALE6;
    let lo1 = s.int4(43)? as f32 * SCALE6;
    let lov = s.int4(52)? as f32 * SCALE6;
    let d_x = s.int4(56)? as f32 * SCALE6;
    let d_y = s.int4(60)? as f32 * SCALE6;
    let latin1 = s.int4(66)? as f32 * SCALE6;
    let latin2 = s.int4(70)? as f32 * SCALE6;
    let (start_x, start_y) = lambert_start(
        f64::from(latin1),
        f64::from(lov),
        f64::from(latin1),
        f64::from(latin2),
        radius * 0.001,
        f64::from(la1),
        f64::from(lo1),
    )?;
    let (origin_km, dx_km) = axis(start_x, f64::from(d_x), nx)?;
    let (origin_y_km, dy_km) = axis(start_y, f64::from(d_y), ny)?;
    let wkt = lambert_wkt(radius, f64::from(latin1), f64::from(lov), f64::from(latin2))?;
    Ok(Georeference {
        nx,
        ny,
        // NetcdfDataReader.scaleGrid through indriya, as Vortex does.
        origin_x: km_to_m_or_refuse(origin_km, "GRIB2 grid")?,
        origin_y: km_to_m_or_refuse(origin_y_km, "GRIB2 grid")?,
        dx: km_to_m_or_refuse(dx_km, "GRIB2 grid")?,
        dy: km_to_m_or_refuse(dy_km, "GRIB2 grid")?,
        wkt_radius: radius,
        wkt,
        start_x,
        start_y,
        template: 30,
        scan_mode,
    })
}

fn number(value: f64) -> Result<String, VortexError> {
    wkt_number(value).ok_or_else(|| refused(format!("WKT parameter {value}")))
}

const GEOGCS_UNIT: &str = "UNIT[\"degree\",0.0174532925199433,";

/// `WktFactory.createWkt`, `LatLonProjection` branch: `ExportToWkt`.
fn geographic_wkt() -> Result<String, VortexError> {
    Ok(format!(
        "GEOGCS[\"unknown\",DATUM[\"unknown\",SPHEROID[\"sphere\",{},0]],PRIMEM[\"Greenwich\",0],\
         {GEOGCS_UNIT}AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]]",
        number(DEFAULT_RADIUS_M)?
    ))
}

fn pretty_geogcs(radius: f64) -> Result<String, VortexError> {
    Ok(format!(
        "    GEOGCS[\"unknown\",\n        DATUM[\"unknown\",\n            SPHEROID[\"sphere\",{},0]],\n        \
         PRIMEM[\"Greenwich\",0],\n        {GEOGCS_UNIT}\n            AUTHORITY[\"EPSG\",\"9122\"]]],\n",
        number(radius)?
    ))
}

const PRETTY_TAIL: &str =
    "    PARAMETER[\"false_easting\",0],\n    PARAMETER[\"false_northing\",0],\n    \
     UNIT[\"Meter\",1],\n    AXIS[\"Easting\",EAST],\n    AXIS[\"Northing\",NORTH]]";

/// `WktFactory.createWkt`, `LambertConformal` branch: `SetLCC`, `ExportToPrettyWkt`.
fn lambert_wkt(radius: f64, lat1: f64, lon: f64, lat2: f64) -> Result<String, VortexError> {
    Ok(format!(
        "PROJCS[\"Lambert Conformal Conic 2SP\",\n{}    PROJECTION[\"Lambert_Conformal_Conic_2SP\"],\n    \
         PARAMETER[\"latitude_of_origin\",{}],\n    PARAMETER[\"central_meridian\",{}],\n    \
         PARAMETER[\"standard_parallel_1\",{}],\n    PARAMETER[\"standard_parallel_2\",{}],\n{PRETTY_TAIL}",
        pretty_geogcs(radius)?,
        number(lat1)?,
        number(lon)?,
        number(lat1)?,
        number(lat2)?,
    ))
}

/// `WktFactory.createWkt`, `Stereographic` branch: `SetStereographic`, `ExportToPrettyWkt`.
pub(crate) fn stereographic_wkt(lat: f64, lon: f64, scale: f64) -> Result<String, VortexError> {
    Ok(format!(
        "PROJCS[\"unnamed\",\n{}    PROJECTION[\"Stereographic\"],\n    PARAMETER[\"latitude_of_origin\",{}],\n    \
         PARAMETER[\"central_meridian\",{}],\n    PARAMETER[\"scale_factor\",{}],\n{PRETTY_TAIL}",
        pretty_geogcs(DEFAULT_RADIUS_M)?,
        number(lat)?,
        number(lon)?,
        number(scale)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ieee_remainder_rounds_to_even() {
        assert_eq!(ieee_remainder_360(190.0), -170.0);
        assert_eq!(ieee_remainder_360(-190.0), 170.0);
        assert_eq!(ieee_remainder_360(540.0), -180.0);
        assert_eq!(ieee_remainder_360(180.0 + 720.0), 180.0);
        assert_eq!(ieee_remainder_360(180.0 + 360.0 * 3.0), -180.0);
    }

    #[test]
    fn a_single_point_axis_has_zero_spacing() {
        assert_eq!(axis(5.0, 1.0, 1).unwrap(), (5.0, 0.0));
    }

    #[test]
    fn polar_spacing_converts_as_indriya_does() {
        // Section 3 of the public Stage IV Puerto Rico grid (205 x 205, template 3.20) with Dx and
        // Dy set to 4763000 mm, the CONUS spacing. Vortex 1.1.0 and 0.13.3 both report dx and dy as
        // 4762.998999618903; `x * 1000.0` gives ...902.
        const SECTION: &str = concat!(
            "0000004103000000a4290000001401000061378000000000000000000000000000",
            "cd000000cd01084f881140830008039387000f32fdc00048ad780048ad780040",
        );
        let bytes: Vec<u8> = (0..SECTION.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&SECTION[i..i + 2], 16).unwrap())
            .collect();
        let g = polar(&Section(&bytes), 205, 205).unwrap();
        assert_eq!(g.dx, 4762.998999618903);
        assert_eq!(g.dy, 4762.998999618903);
        assert_ne!(g.dx, 4762.998999618902);
    }

    #[test]
    fn an_irregular_axis_is_refused_not_resampled() {
        // Above 2^24 km, f32 rounding makes the steps uneven.
        assert!(axis(20_000_000.0, 0.5, 8).is_err());
    }
}

//! Section 2, the grid description, to the georeference Vortex hands its warp.
//!
//! Sources, NetCDF-Java 5.5.3: `Grib1Gds` (constructor: `nx = getOctet2(7)`, `ny = getOctet2(9)`;
//! `getEarth`, `getEarthShapeIsSpherical`), `Grib1Gds.PolarStereographic` (octets 11-28, `lad`
//! fixed at 60, `makeHorizCoordSys`). The projection, the axis and the WKT are the GRIB2 reader's
//! port of `Stereographic.latLonToProj`, `Array.makeArray`, `CoordinateAxis1D.makeEdges` and
//! Vortex's `WktFactory.createWkt`, shared so the two editions cannot drift apart.
//!
//! `PolarStereographic.makeHorizCoordSys` builds `new Stereographic(latOrigin, lov, scale)`, the
//! three-argument constructor, so the projection and WKT use NetCDF-Java's default radius
//! (6371.229 km), not the 6367.47 km `getEarth` gives a spherical GRIB1 earth. Vortex 1.1.0 reports
//! that WKT for Stage IV GRIB1.

use super::scan::decode_error;
use crate::error::VortexError;
use crate::maths;
use crate::source::grib::gds::{self as grib2_gds, Georeference};
use crate::units::km_to_m_or_refuse;

fn refused(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "GRIB1 grid".to_string(),
        reason: reason.into(),
    }
}

/// `Grib1Gds.scale3`.
const SCALE3: f32 = 1.0e-3;

/// `GribNumbers.int3`: sign and magnitude.
fn int3(b: &[u8]) -> i32 {
    let a = i32::from(b[0]);
    (1 - ((a & 128) >> 6)) * ((a & 127) << 16 | i32::from(b[1]) << 8 | i32::from(b[2]))
}

/// A grid length, read as the unsigned 3-octet field it is. Zero is refused, since it gives no
/// grid. A length with the top bit set is valid GRIB1 (over 8,388 km), but NetCDF-Java 5.5.3 reads
/// it with the signed `getOctet3` (`Grib1Gds.PolarStereographic`), so Vortex would use a negative
/// spacing: there is no reference to match, and it is refused rather than read either way.
fn grid_length(b: &[u8], name: &str) -> Result<u32, VortexError> {
    let value = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
    if value == 0 {
        return Err(refused(format!("{name} is 0 m")));
    }
    if value >= 0x80_0000 {
        return Err(refused(format!(
            "{name} is {value} m. Observed behaviour we did not implement: NetCDF-Java reads this \
             unsigned length as signed, {} m",
            -i64::from(value & 0x7f_ffff)
        )));
    }
    Ok(value)
}

/// Octet 6: data representation type (code table 6).
pub(super) fn representation(section: &[u8]) -> Result<u8, VortexError> {
    section
        .get(5)
        .copied()
        .ok_or_else(|| decode_error("GDS ends before octet 6".to_string()))
}

/// `nx * ny` of a polar stereographic GDS, checked, without building the rest.
pub(super) fn points(section: &[u8]) -> Result<(usize, usize, usize), VortexError> {
    if section.len() < 28 {
        return Err(decode_error(format!(
            "GDS of {} octets, under 28",
            section.len()
        )));
    }
    let nx = usize::from(u16::from_be_bytes([section[6], section[7]]));
    let ny = usize::from(u16::from_be_bytes([section[8], section[9]]));
    if nx == 0 || ny == 0 || nx == 0xffff || ny == 0xffff {
        return Err(decode_error(format!("grid is {nx} x {ny}")));
    }
    Ok((nx, ny, nx * ny))
}

/// Read a polar stereographic GDS (representation type 5), the HRAP grid of Stage IV.
pub(super) fn read_georeference(section: &[u8]) -> Result<Georeference, VortexError> {
    let kind = representation(section)?;
    if kind != 5 {
        return Err(VortexError::Unsupported {
            what: format!(
                "GRIB1 grid representation type {kind} (code table 6); this reader takes 5, the \
                 polar stereographic HRAP grid of Stage IV"
            ),
        });
    }
    let (nx, ny, _) = points(section)?;
    // Octet 4 NV, octet 5 PV/PL: vertical coordinates or a thin-grid list.
    if section[3] != 0 {
        return Err(refused("the GDS lists vertical coordinates"));
    }
    if section[4] != 255 && section[4] != 0 {
        return Err(refused(
            "the GDS points to a list of points per row (a thin grid)",
        ));
    }
    let la1 = int3(&section[10..13]) as f32 * SCALE3;
    let lo1 = int3(&section[13..16]) as f32 * SCALE3;
    let resolution = section[16];
    let lov = int3(&section[17..20]) as f32 * SCALE3;
    // Dx and Dy are unsigned 3-octet lengths in metres (WMO FM 92 GRIB1 grid 5; NCEP ON388
    // table D), unlike La1, Lo1 and LoV, which carry a sign bit.
    let d_x = grid_length(&section[20..23], "Dx")? as f32 * SCALE3;
    let d_y = grid_length(&section[23..26], "Dy")? as f32 * SCALE3;
    let center_flag = section[26];
    let scan_mode = section[27];
    // getEarthShapeIsSpherical: code table 7 bit 2 clear. The other branch is NetCDF-Java's
    // proj4 StereographicAzimuthalProjection on the IAU ellipsoid, not ported.
    if resolution & 0x40 != 0 {
        return Err(refused(
            "an oblate spheroid earth (code table 7 bit 2); Stage IV states a sphere",
        ));
    }
    // Scan mode 64 (+i, +j, rows adjacent) is the one Stage IV uses. 128 and 192 are flipped in
    // Grib1DataReader.scanningModeCheck, 0 is north to south; with no sample, no reference.
    if scan_mode != 64 {
        return Err(refused(format!(
            "scan mode {scan_mode}; this reader takes 64, the one Stage IV uses"
        )));
    }
    if center_flag & 0x7f != 0 {
        return Err(refused(format!(
            "projection centre flag {center_flag}: a bipolar projection"
        )));
    }
    let north = center_flag & 128 == 0;
    let lat_origin = if north { 90.0 } else { -90.0 };
    // makeHorizCoordSys: lad is the float 60, so the scale factor is (1 + sin(60 deg)) / 2.
    let lad = 60.0_f32;
    let k0 = (1.0 + maths::sin(f64::from(lad).abs() * grib2_gds::DEG_TO_RAD)) / 2.0;
    let (start_x, start_y) = grib2_gds::stereographic_start(
        lat_origin,
        f64::from(lov),
        k0,
        f64::from(la1),
        f64::from(lo1),
    );
    let (origin_km, dx_km) = grib2_gds::axis(start_x, f64::from(d_x), nx)?;
    let (origin_y_km, dy_km) = grib2_gds::axis(start_y, f64::from(d_y), ny)?;
    let wkt = grib2_gds::stereographic_wkt(lat_origin, f64::from(lov), k0)?;
    Ok(Georeference {
        nx,
        ny,
        // NetcdfDataReader.scaleGrid through indriya, as Vortex does.
        origin_x: km_to_m_or_refuse(origin_km, "GRIB1 grid")?,
        origin_y: km_to_m_or_refuse(origin_y_km, "GRIB1 grid")?,
        dx: km_to_m_or_refuse(dx_km, "GRIB1 grid")?,
        dy: km_to_m_or_refuse(dy_km, "GRIB1 grid")?,
        wkt_radius: grib2_gds::DEFAULT_RADIUS_M,
        wkt,
        start_x,
        start_y,
        // Code table 6 value, in the field GRIB2 uses for its template number.
        template: u16::from(kind),
        scan_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int3_is_sign_and_magnitude() {
        assert_eq!(int3(&[0x00, 0x5a, 0x4d]), 23117);
        assert_eq!(int3(&[0x81, 0xd0, 0xef]), -119023);
    }
}

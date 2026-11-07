//! GeoTIFF keys to a CRS, and a CRS to keys.
//!
//! Key numbers are GeoTIFF 1.1. Which keys carry which parameter, and which a coordinate transform
//! accepts, follow GDAL 3.2.1 `GTIFSetFromOGISDefnEx` (`frmts/gtiff/gt_wkt_srs.cpp`) for writing
//! and libgeotiff's `GTIFFetchProjParms` (`geo_normalize.c`) for reading.
//!
//! Nothing is guessed. A key this reader does not know, a key nothing here consumes, an EPSG code
//! outside its short table, a unit other than metres and degrees, an ellipsoid with no datum, or
//! two statements of one value that differ all give [`CrsResolution::Unresolved`], which the caller
//! refuses by default.
//!
//! `ESRI PE String = <wkt>` in a citation key is the verbatim WKT route GDAL itself uses for what
//! it cannot map to keys. The sink stores a WKT frame's text there, so a frame reads back with the
//! text it was given. Text is believed alone only when the keys state nothing else. Otherwise the
//! two must be compared and agree: GDAL prefers an EPSG code over a citation, so keys that cannot
//! be read must not license the text.

use super::geotiff::{malformed, Entry, Tiff};
use super::{is_wkt1_crs, CrsResolution};
use crate::crs::wkt::{wkt_facts, WktFacts};
use crate::crs::{CrsDef, Datum, Projection};
use crate::error::VortexError;
use crate::frame::{Crs, NamedCrs};

const GT_MODEL_TYPE: u16 = 1024;
const GT_RASTER_TYPE: u16 = 1025;
const GT_CITATION: u16 = 1026;
const GEOGRAPHIC_TYPE: u16 = 2048;
const GEOG_CITATION: u16 = 2049;
const GEOG_DATUM: u16 = 2050;
const GEOG_PRIME_MERIDIAN: u16 = 2051;
const GEOG_LINEAR_UNITS: u16 = 2052;
const GEOG_ANGULAR_UNITS: u16 = 2054;
const GEOG_ANGULAR_SIZE: u16 = 2055;
const GEOG_ELLIPSOID: u16 = 2056;
const GEOG_SEMI_MAJOR: u16 = 2057;
const GEOG_SEMI_MINOR: u16 = 2058;
const GEOG_INV_FLATTENING: u16 = 2059;
const GEOG_AZIMUTH_UNITS: u16 = 2060;
const GEOG_PM_LONG: u16 = 2061;
/// GDAL's `ProjLinearUnitsInterpCorrectGeoKey`: false easting and northing are in the linear unit.
const PROJ_LINEAR_INTERP_CORRECT: u16 = 3059;
const PROJECTED_TYPE: u16 = 3072;
const PCS_CITATION: u16 = 3073;
const PROJECTION: u16 = 3074;
const PROJ_TRANSFORM: u16 = 3075;
const PROJ_LINEAR_UNITS: u16 = 3076;
const PROJ_LINEAR_SIZE: u16 = 3077;
const STD_PARALLEL_1: u16 = 3078;
const STD_PARALLEL_2: u16 = 3079;
const NAT_ORIGIN_LONG: u16 = 3080;
const NAT_ORIGIN_LAT: u16 = 3081;
const FALSE_EASTING: u16 = 3082;
const FALSE_NORTHING: u16 = 3083;
const FALSE_ORIGIN_LONG: u16 = 3084;
const FALSE_ORIGIN_LAT: u16 = 3085;
const FALSE_ORIGIN_EASTING: u16 = 3086;
const FALSE_ORIGIN_NORTHING: u16 = 3087;
const CENTER_LONG: u16 = 3088;
const CENTER_LAT: u16 = 3089;
const CENTER_EASTING: u16 = 3090;
const CENTER_NORTHING: u16 = 3091;
const SCALE_AT_NAT_ORIGIN: u16 = 3092;
const SCALE_AT_CENTER: u16 = 3093;
const STRAIGHT_VERT_POLE_LONG: u16 = 3095;

const USER_DEFINED: u16 = 32767;

const CT_TRANSVERSE_MERCATOR: u16 = 1;
const CT_LAMBERT_2SP: u16 = 8;
const CT_LAMBERT_1SP: u16 = 9;
const CT_ALBERS: u16 = 11;
const CT_STEREOGRAPHIC: u16 = 14;
const CT_POLAR_STEREOGRAPHIC: u16 = 15;

const LOC_SHORT: u16 = 0;
const LOC_DOUBLE: u16 = 34736;
const LOC_ASCII: u16 = 34737;

/// Every key this reader understands. Any other is a statement it cannot act on.
const KNOWN: &[u16] = &[
    GT_MODEL_TYPE,
    GT_RASTER_TYPE,
    GT_CITATION,
    GEOGRAPHIC_TYPE,
    GEOG_CITATION,
    GEOG_DATUM,
    GEOG_PRIME_MERIDIAN,
    GEOG_LINEAR_UNITS,
    GEOG_ANGULAR_UNITS,
    GEOG_ANGULAR_SIZE,
    GEOG_ELLIPSOID,
    GEOG_SEMI_MAJOR,
    GEOG_SEMI_MINOR,
    GEOG_INV_FLATTENING,
    GEOG_AZIMUTH_UNITS,
    GEOG_PM_LONG,
    PROJ_LINEAR_INTERP_CORRECT,
    PROJECTED_TYPE,
    PCS_CITATION,
    PROJECTION,
    PROJ_TRANSFORM,
    PROJ_LINEAR_UNITS,
    PROJ_LINEAR_SIZE,
    STD_PARALLEL_1,
    STD_PARALLEL_2,
    NAT_ORIGIN_LONG,
    NAT_ORIGIN_LAT,
    FALSE_EASTING,
    FALSE_NORTHING,
    FALSE_ORIGIN_LONG,
    FALSE_ORIGIN_LAT,
    FALSE_ORIGIN_EASTING,
    FALSE_ORIGIN_NORTHING,
    CENTER_LONG,
    CENTER_LAT,
    CENTER_EASTING,
    CENTER_NORTHING,
    SCALE_AT_NAT_ORIGIN,
    SCALE_AT_CENTER,
    STRAIGHT_VERT_POLE_LONG,
];

/// Parameter keys, which an EPSG-coded CRS must not also state.
const PARAMETERS: &[u16] = &[
    STD_PARALLEL_1,
    STD_PARALLEL_2,
    NAT_ORIGIN_LONG,
    NAT_ORIGIN_LAT,
    FALSE_EASTING,
    FALSE_NORTHING,
    FALSE_ORIGIN_LONG,
    FALSE_ORIGIN_LAT,
    FALSE_ORIGIN_EASTING,
    FALSE_ORIGIN_NORTHING,
    CENTER_LONG,
    CENTER_LAT,
    CENTER_EASTING,
    CENTER_NORTHING,
    SCALE_AT_NAT_ORIGIN,
    SCALE_AT_CENTER,
    STRAIGHT_VERT_POLE_LONG,
];

/// EPSG conversion 5071, "Conus Albers", the projection of EPSG:5070.
const EPSG_CONUS_ALBERS: u16 = 5071;

const PE_PREFIX: &str = "ESRI PE String = ";
/// GDAL reads citations through a 2400-byte buffer, so longer text would reach it cut.
const GDAL_CITATION_BUFFER: usize = 2400;
/// Most bytes one geokey's text may hold: its count is a SHORT.
const MAX_ASCII: usize = 65_534;

/// The three GeoTIFF tags, as bytes in the file. Nothing is copied.
pub(crate) struct GeoTags<'a> {
    little: bool,
    keys: Option<&'a [u8]>,
    doubles: &'a [u8],
    ascii: &'a [u8],
}

#[derive(Clone, Copy)]
struct Key {
    id: u16,
    location: u16,
    count: u16,
    value: u16,
}

impl<'a> GeoTags<'a> {
    pub(crate) fn read(
        tiff: &Tiff<'a>,
        keys: Option<Entry>,
        doubles: Option<Entry>,
        ascii: Option<Entry>,
    ) -> Result<Self, VortexError> {
        Ok(GeoTags {
            little: tiff.little,
            keys: keys.map(|e| tiff.shorts(&e)).transpose()?,
            doubles: doubles
                .map(|e| tiff.doubles(&e))
                .transpose()?
                .unwrap_or(&[]),
            ascii: ascii.map(|e| tiff.ascii(&e)).transpose()?.unwrap_or(&[]),
        })
    }

    fn word(&self, bytes: &[u8], index: usize) -> Option<u16> {
        let b = bytes.get(index * 2..index * 2 + 2)?;
        Some(if self.little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    /// Key count, or why the directory cannot be read.
    fn key_count(&self) -> Result<usize, String> {
        let Some(keys) = self.keys else {
            return Ok(0);
        };
        let version = self.word(keys, 0).ok_or("the geokey directory is empty")?;
        if version != 1 {
            return Err(format!("geokey directory version {version}"));
        }
        let count = self
            .word(keys, 3)
            .ok_or("the geokey directory header is short")? as usize;
        if keys.len() < (count + 1) * 8 {
            return Err(format!("the geokey directory is short of {count} keys"));
        }
        Ok(count)
    }

    fn key_at(&self, index: usize) -> Option<Key> {
        let keys = self.keys?;
        let at = (index + 1) * 4;
        Some(Key {
            id: self.word(keys, at)?,
            location: self.word(keys, at + 1)?,
            count: self.word(keys, at + 2)?,
            value: self.word(keys, at + 3)?,
        })
    }

    fn find(&self, id: u16) -> Result<Option<Key>, String> {
        let mut found = None;
        for index in 0..self.key_count()? {
            let key = self.key_at(index).ok_or("the geokey directory is short")?;
            if key.id == id {
                if found.is_some() {
                    return Err(format!("geokey {id} is repeated"));
                }
                found = Some(key);
            }
        }
        Ok(found)
    }

    fn short(&self, id: u16) -> Result<Option<u16>, String> {
        match self.find(id)? {
            None => Ok(None),
            Some(k) if k.location == LOC_SHORT && k.count == 1 => Ok(Some(k.value)),
            Some(_) => Err(format!("geokey {id} is not a single SHORT")),
        }
    }

    fn double(&self, id: u16) -> Result<Option<f64>, String> {
        let Some(k) = self.find(id)? else {
            return Ok(None);
        };
        if k.location != LOC_DOUBLE || k.count != 1 {
            return Err(format!("geokey {id} is not a single DOUBLE"));
        }
        let at = usize::from(k.value) * 8;
        let b: [u8; 8] = self
            .doubles
            .get(at..at + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| format!("geokey {id} points past GeoDoubleParams"))?;
        Ok(Some(if self.little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        }))
    }

    /// A key's text, without the `|` that ends it.
    fn text(&self, id: u16) -> Result<Option<&'a str>, String> {
        let Some(k) = self.find(id)? else {
            return Ok(None);
        };
        if k.location != LOC_ASCII {
            return Err(format!("geokey {id} is not ASCII"));
        }
        let (start, count) = (usize::from(k.value), usize::from(k.count));
        let bytes = self
            .ascii
            .get(start..start + count)
            .ok_or_else(|| format!("geokey {id} points past GeoAsciiParams"))?;
        let text = std::str::from_utf8(bytes).map_err(|_| format!("geokey {id} is not ASCII"))?;
        Ok(Some(text.trim_end_matches(['|', '\0'])))
    }

    /// GTRasterTypeGeoKey: 1 is area, 2 is point; absent is area, as in GDAL.
    pub(crate) fn pixel_is_point(&self) -> Result<bool, VortexError> {
        match self.short(GT_RASTER_TYPE) {
            Ok(None) | Ok(Some(1)) => Ok(false),
            Ok(Some(2)) => Ok(true),
            Ok(Some(other)) => Err(malformed(format!("GTRasterType {other}"))),
            // An unreadable directory is the CRS's business; `resolve` reports it.
            Err(_) => Ok(false),
        }
    }

    /// The verbatim WKT in a citation, if any. Two citations must state the same text.
    fn pe_string(&self) -> Result<Option<&'a str>, String> {
        let mut found = None;
        for id in [GT_CITATION, PCS_CITATION] {
            if let Some(text) = self.text(id)? {
                if let Some(at) = text.find(PE_PREFIX) {
                    let tail = &text[at + PE_PREFIX.len()..];
                    let end = tail.find('|').unwrap_or(tail.len());
                    let wkt = &tail[..end];
                    if found.is_some_and(|f| f != wkt) {
                        return Err("two citations state different WKT".to_string());
                    }
                    found = Some(wkt);
                }
            }
        }
        Ok(found)
    }
}

fn require(value: Option<f64>, name: &str) -> Result<f64, String> {
    value.ok_or_else(|| format!("the projection does not state {name}"))
}

/// The one value among several keys that may state it. Two different statements are refused.
fn one_of(k: &GeoTags, ids: &[u16]) -> Result<Option<f64>, String> {
    let mut found: Option<f64> = None;
    for id in ids {
        if let Some(value) = k.double(*id)? {
            if found.is_some_and(|f| f != value) {
                return Err(format!("keys {ids:?} state different values"));
            }
            found = Some(value);
        }
    }
    Ok(found)
}

/// Ellipsoid from an EPSG ellipsoid or datum code: (a, rf).
fn ellipsoid(code: u16) -> Option<(f64, f64)> {
    match code {
        7030 | 6326 | 4326 => Some((6378137.0, 298.257223563)),
        7019 | 6269 | 4269 => Some((6378137.0, 298.257222101)),
        _ => None,
    }
}

/// Degrees from Greenwich, or why not.
fn geographic_units(k: &GeoTags) -> Result<(), String> {
    let unit = k.short(GEOG_ANGULAR_UNITS)?;
    if !matches!(unit, None | Some(9102)) {
        return Err(format!("geographic angular unit {}", unit.unwrap_or(0)));
    }
    let azimuth = k.short(GEOG_AZIMUTH_UNITS)?;
    if !matches!(azimuth, None | Some(9102)) {
        return Err(format!("azimuth unit {}", azimuth.unwrap_or(0)));
    }
    if k.double(GEOG_ANGULAR_SIZE)?
        .is_some_and(|s| s != 0.0174532925199433 && s != std::f64::consts::PI / 180.0)
    {
        return Err("geographic angular unit size".to_string());
    }
    let linear = k.short(GEOG_LINEAR_UNITS)?;
    if !matches!(linear, None | Some(9001)) {
        return Err(format!("geographic linear unit {}", linear.unwrap_or(0)));
    }
    let meridian = k.short(GEOG_PRIME_MERIDIAN)?;
    if !matches!(meridian, None | Some(8901) | Some(USER_DEFINED)) {
        return Err(format!("prime meridian {}", meridian.unwrap_or(0)));
    }
    if meridian == Some(USER_DEFINED) && k.double(GEOG_PM_LONG)?.is_none() {
        return Err("a user-defined prime meridian with no longitude".to_string());
    }
    if k.double(GEOG_PM_LONG)?.is_some_and(|l| l != 0.0) {
        return Err("a prime meridian other than Greenwich".to_string());
    }
    Ok(())
}

/// The ellipsoid the axis keys state, as (a, rf), if they state one this reader can use.
fn explicit_ellipsoid(k: &GeoTags) -> Result<Option<(f64, f64)>, String> {
    let a = k.double(GEOG_SEMI_MAJOR)?;
    let rf = k.double(GEOG_INV_FLATTENING)?;
    let b = k.double(GEOG_SEMI_MINOR)?;
    match (a, rf, b) {
        (None, None, None) => Ok(None),
        (Some(a), Some(rf), None) => Ok(Some((a, rf))),
        // Only a sphere states this exactly; a computed rf would be a choice of rounding.
        (Some(a), None, Some(b)) if a == b => Ok(Some((a, 0.0))),
        _ => Err("the ellipsoid keys are not a semi-major axis and rf".to_string()),
    }
}

/// The datum a geographic citation names in GDAL's `Datum = ` form.
fn citation_datum(citation: &str) -> Option<&str> {
    citation
        .split('|')
        .find_map(|part| part.trim().strip_prefix("Datum = "))
}

/// The geographic part: datum, a and rf. Only degrees from Greenwich.
fn geographic(k: &GeoTags) -> Result<(Datum, f64, f64), String> {
    geographic_units(k)?;
    let code = k.short(GEOGRAPHIC_TYPE)?;
    let datum_code = k.short(GEOG_DATUM)?;
    let ellipsoid_code = k.short(GEOG_ELLIPSOID)?;
    for c in [code, datum_code, ellipsoid_code].into_iter().flatten() {
        if c != USER_DEFINED && ellipsoid(c).is_none() {
            return Err(format!("EPSG code {c} is not in this reader's short table"));
        }
    }
    let stated: Vec<(f64, f64)> = [code, datum_code, ellipsoid_code]
        .into_iter()
        .flatten()
        .filter_map(ellipsoid)
        .collect();
    if stated.windows(2).any(|w| w[0] != w[1]) {
        return Err("geographic EPSG codes disagree".to_string());
    }
    let mut explicit = explicit_ellipsoid(k)?;
    if let Some(&known) = stated.first() {
        if explicit.is_some_and(|e| e != known) {
            return Err("the ellipsoid keys disagree with the EPSG code".to_string());
        }
        explicit = Some(known);
    }
    let citation = k.text(GEOG_CITATION)?;

    // A geographic or datum code names a datum. An ellipsoid code does not.
    let names_datum = [code, datum_code]
        .into_iter()
        .flatten()
        .any(|c| ellipsoid(c).is_some());
    if let (true, Some((a, rf))) = (names_datum, explicit) {
        let datum = if rf == 298.257223563 {
            Datum::Wgs84
        } else {
            Datum::Nad83
        };
        if let Some(name) = citation.and_then(citation_datum) {
            if Datum::classify_stated(name, a, rf, None, None, None, None) != datum {
                return Err(format!(
                    "the citation's datum {name} disagrees with the EPSG code"
                ));
            }
        }
        return Ok((datum, a, rf));
    }
    let (a, rf) = explicit.ok_or("no geographic CRS is stated")?;
    let citation = citation.unwrap_or("unknown");
    let name = citation_datum(citation)
        .or_else(|| citation.split('|').next())
        .unwrap_or("unknown");
    let datum = Datum::classify_stated(name, a, rf, None, None, None, None);
    // A sphere has no datum to name. Any other ellipsoid is not itself a datum.
    if rf != 0.0 && matches!(datum, Datum::Wgs84 | Datum::Nad83) {
        let bare = name.to_ascii_uppercase();
        if matches!(
            bare.strip_prefix("D_").unwrap_or(&bare),
            "UNKNOWN" | "UNNAMED" | "SPHERE"
        ) {
            return Err("an ellipsoid with no datum".to_string());
        }
    }
    Ok((datum, a, rf))
}

/// The CRS the numeric keys state.
fn numeric(k: &GeoTags) -> Result<CrsDef, String> {
    let model = k.short(GT_MODEL_TYPE)?;
    match model {
        Some(2) => {
            let projected_keys = [
                PROJECTED_TYPE,
                PROJECTION,
                PROJ_TRANSFORM,
                PROJ_LINEAR_UNITS,
                PROJ_LINEAR_SIZE,
            ];
            for id in projected_keys.iter().chain(PARAMETERS) {
                if k.find(*id)?.is_some() {
                    return Err(format!("a geographic model with projected key {id}"));
                }
            }
            let (datum, a, rf) = geographic(k)?;
            Ok(CrsDef {
                datum,
                a,
                rf,
                projection: Projection::LongLat,
            })
        }
        Some(1) => projected(k),
        Some(other) => Err(format!("model type {other} with no WKT")),
        None => Err("no model type".to_string()),
    }
}

fn projected(k: &GeoTags) -> Result<CrsDef, String> {
    let pcs = k.short(PROJECTED_TYPE)?.ok_or("no projected CRS code")?;
    if pcs != USER_DEFINED {
        if let Some(zone) = epsg_utm_zone(pcs) {
            return epsg_utm(k, pcs, zone);
        }
        // Nothing else is in the table.
        if pcs != 5070 {
            return Err(format!(
                "EPSG:{pcs} is not in this reader's short table, which holds no database"
            ));
        }
        return epsg_5070(k);
    }
    let projection = k.short(PROJECTION)?;
    if !matches!(projection, None | Some(USER_DEFINED)) {
        return Err(format!(
            "projection code {} is not in this reader's table",
            projection.unwrap_or(0)
        ));
    }
    let units = k.short(PROJ_LINEAR_UNITS)?;
    if units != Some(9001) {
        return Err(format!("linear unit {units:?}, only metres are read"));
    }
    if k.double(PROJ_LINEAR_SIZE)?.is_some_and(|s| s != 1.0) {
        return Err("a linear unit size other than 1".to_string());
    }
    let (datum, a, rf) = geographic(k)?;
    let transform = k.short(PROJ_TRANSFORM)?.ok_or("no coordinate transform")?;
    let lat_0 = || one_of(k, &[NAT_ORIGIN_LAT, FALSE_ORIGIN_LAT, CENTER_LAT]);
    let lon_0 = || {
        one_of(
            k,
            &[
                NAT_ORIGIN_LONG,
                FALSE_ORIGIN_LONG,
                CENTER_LONG,
                STRAIGHT_VERT_POLE_LONG,
            ],
        )
    };
    let x_0 = one_of(k, &[FALSE_EASTING, FALSE_ORIGIN_EASTING, CENTER_EASTING])?.unwrap_or(0.0);
    let y_0 = one_of(k, &[FALSE_NORTHING, FALSE_ORIGIN_NORTHING, CENTER_NORTHING])?.unwrap_or(0.0);
    let scale = one_of(k, &[SCALE_AT_NAT_ORIGIN, SCALE_AT_CENTER])?;
    consumed_parameters_only(k, transform, scale)?;
    let projection = match transform {
        CT_ALBERS => Projection::Albers {
            lat_1: require(k.double(STD_PARALLEL_1)?, "standard parallel 1")?,
            lat_2: require(k.double(STD_PARALLEL_2)?, "standard parallel 2")?,
            lat_0: require(lat_0()?, "latitude of origin")?,
            lon_0: require(lon_0()?, "central meridian")?,
            x_0,
            y_0,
        },
        CT_LAMBERT_2SP => Projection::Lambert {
            lat_1: require(k.double(STD_PARALLEL_1)?, "standard parallel 1")?,
            lat_2: require(k.double(STD_PARALLEL_2)?, "standard parallel 2")?,
            lat_0: require(lat_0()?, "latitude of origin")?,
            lon_0: require(lon_0()?, "central meridian")?,
            k_0: 1.0,
            x_0,
            y_0,
        },
        CT_LAMBERT_1SP => {
            let lat = require(lat_0()?, "latitude of origin")?;
            Projection::Lambert {
                lat_1: lat,
                lat_2: lat,
                lat_0: lat,
                lon_0: require(lon_0()?, "central meridian")?,
                k_0: scale.unwrap_or(1.0),
                x_0,
                y_0,
            }
        }
        CT_POLAR_STEREOGRAPHIC => {
            let lat = require(k.double(NAT_ORIGIN_LAT)?, "latitude of true scale")?;
            if scale.is_some_and(|s| s != 1.0) {
                return Err("polar stereographic with a scale factor".to_string());
            }
            Projection::Stereographic {
                lat_0: if lat < 0.0 { -90.0 } else { 90.0 },
                lon_0: require(lon_0()?, "central meridian")?,
                k_0: 1.0,
                lat_ts: Some(lat),
                x_0,
                y_0,
            }
        }
        CT_STEREOGRAPHIC => Projection::Stereographic {
            lat_0: require(lat_0()?, "latitude of origin")?,
            lon_0: require(lon_0()?, "central meridian")?,
            k_0: scale.unwrap_or(1.0),
            lat_ts: None,
            x_0,
            y_0,
        },
        // Metres only here: the unit test above has passed. Feet are read beside a citation's
        // WKT, by `keys_state_this_transverse_mercator`.
        CT_TRANSVERSE_MERCATOR => Projection::TransverseMercator {
            lat_0: require(lat_0()?, "latitude of origin")?,
            lon_0: require(lon_0()?, "central meridian")?,
            k_0: scale.unwrap_or(1.0),
            x_0,
            y_0,
            to_metre: 1.0,
        },
        other => return Err(format!("coordinate transform {other}")),
    };
    Ok(CrsDef {
        datum,
        a,
        rf,
        projection,
    })
}

/// A UTM zone an EPSG projected code states, by the codes' own rule and with no database:
/// 32601 to 32660 are WGS 84 / UTM zones 1N to 60N, 32701 to 32760 are 1S to 60S, and 26901 to
/// 26923 are NAD83 / UTM zones 1N to 23N, the whole of that series. No other code is read: NAD27
/// (267xx), the NAD83 realisations with their own codes, and ETRS89 among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EpsgUtm {
    wgs84: bool,
    zone: u16,
    south: bool,
}

impl EpsgUtm {
    /// EPSG's name for the CRS, which follows from the code as the zone does.
    fn epsg_name(self) -> String {
        format!(
            "{} / UTM zone {}{}",
            if self.wgs84 { "WGS 84" } else { "NAD83" },
            self.zone,
            if self.south { "S" } else { "N" }
        )
    }
}

fn epsg_utm_zone(pcs: u16) -> Option<EpsgUtm> {
    let (wgs84, zone, south) = match pcs {
        32601..=32660 => (true, pcs - 32600, false),
        32701..=32760 => (true, pcs - 32700, true),
        26901..=26923 => (false, pcs - 26900, false),
        _ => return None,
    };
    Some(EpsgUtm { wgs84, zone, south })
}

/// As for [`epsg_5070`]: whatever stands beside the code must say what the code says.
fn epsg_utm(k: &GeoTags, pcs: u16, utm: EpsgUtm) -> Result<CrsDef, String> {
    for id in PARAMETERS {
        if k.find(*id)?.is_some() {
            return Err(format!("EPSG:{pcs} with parameter key {id}"));
        }
    }
    // EPSG's conversion codes: 16001 to 16060 are UTM zones 1N to 60N, 16101 to 16160 1S to 60S.
    let conversion = if utm.south { 16100 } else { 16000 } + utm.zone;
    let projection = k.short(PROJECTION)?;
    if !matches!(projection, None | Some(USER_DEFINED)) && projection != Some(conversion) {
        return Err(format!(
            "EPSG:{pcs} with projection code {}",
            projection.unwrap_or(0)
        ));
    }
    let transform = k.short(PROJ_TRANSFORM)?;
    if !matches!(transform, None | Some(CT_TRANSVERSE_MERCATOR)) {
        return Err(format!(
            "EPSG:{pcs} with coordinate transform {}",
            transform.unwrap_or(0)
        ));
    }
    let units = k.short(PROJ_LINEAR_UNITS)?;
    if !matches!(units, None | Some(9001)) {
        return Err(format!(
            "EPSG:{pcs} with linear unit {}",
            units.unwrap_or(0)
        ));
    }
    if k.double(PROJ_LINEAR_SIZE)?.is_some_and(|s| s != 1.0) {
        return Err(format!("EPSG:{pcs} with a linear unit size other than 1"));
    }
    geographic_units(k)?;
    let (codes, datum, a, rf) = if utm.wgs84 {
        ([4326, 6326, 7030], Datum::Wgs84, 6378137.0, 298.257223563)
    } else {
        ([4269, 6269, 7019], Datum::Nad83, 6378137.0, 298.257222101)
    };
    for (id, expected) in [GEOGRAPHIC_TYPE, GEOG_DATUM, GEOG_ELLIPSOID]
        .into_iter()
        .zip(codes)
    {
        if let Some(code) = k.short(id)? {
            if code != expected {
                return Err(format!("EPSG:{pcs} with geographic code {code}"));
            }
        }
    }
    if explicit_ellipsoid(k)?.is_some_and(|e| e != (a, rf)) {
        return Err(format!("EPSG:{pcs} with another ellipsoid"));
    }
    if let Some(name) = k.text(GEOG_CITATION)?.and_then(citation_datum) {
        if Datum::classify_stated(name, a, rf, None, None, None, None) != datum {
            return Err(format!("EPSG:{pcs} with the citation's datum {name}"));
        }
    }
    CrsDef::utm(datum, a, rf, u32::from(utm.zone), utm.south)
        .ok_or_else(|| format!("EPSG:{pcs} is not a UTM zone"))
}

/// EPSG:5070, NAD83 / Conus Albers, is SHG's definition. Any other statement beside the code must
/// say the same: GDAL reads a contradicting datum or unit, and SHG would misplace or rescale it.
fn epsg_5070(k: &GeoTags) -> Result<CrsDef, String> {
    for id in PARAMETERS {
        if k.find(*id)?.is_some() {
            return Err(format!("EPSG:5070 with parameter key {id}"));
        }
    }
    let projection = k.short(PROJECTION)?;
    if !matches!(projection, None | Some(EPSG_CONUS_ALBERS)) {
        return Err(format!(
            "EPSG:5070 with projection code {}",
            projection.unwrap_or(0)
        ));
    }
    let transform = k.short(PROJ_TRANSFORM)?;
    if !matches!(transform, None | Some(CT_ALBERS)) {
        return Err(format!(
            "EPSG:5070 with coordinate transform {}",
            transform.unwrap_or(0)
        ));
    }
    let units = k.short(PROJ_LINEAR_UNITS)?;
    if !matches!(units, None | Some(9001)) {
        return Err(format!("EPSG:5070 with linear unit {}", units.unwrap_or(0)));
    }
    if k.double(PROJ_LINEAR_SIZE)?.is_some_and(|s| s != 1.0) {
        return Err("EPSG:5070 with a linear unit size other than 1".to_string());
    }
    geographic_units(k)?;
    for (id, nad83) in [
        (GEOGRAPHIC_TYPE, 4269),
        (GEOG_DATUM, 6269),
        (GEOG_ELLIPSOID, 7019),
    ] {
        if let Some(code) = k.short(id)? {
            if code != nad83 {
                return Err(format!("EPSG:5070 with geographic code {code}"));
            }
        }
    }
    let shg = CrsDef::shg();
    if explicit_ellipsoid(k)?.is_some_and(|e| e != (shg.a, shg.rf)) {
        return Err("EPSG:5070 with another ellipsoid".to_string());
    }
    if let Some(name) = k.text(GEOG_CITATION)?.and_then(citation_datum) {
        if Datum::classify_stated(name, shg.a, shg.rf, None, None, None, None) != shg.datum {
            return Err(format!("EPSG:5070 with the citation's datum {name}"));
        }
    }
    Ok(shg)
}

/// A parameter key this transform does not read would be a statement dropped without notice.
fn consumed_parameters_only(k: &GeoTags, transform: u16, scale: Option<f64>) -> Result<(), String> {
    const ORIGIN: &[u16] = &[
        NAT_ORIGIN_LAT,
        FALSE_ORIGIN_LAT,
        CENTER_LAT,
        NAT_ORIGIN_LONG,
        FALSE_ORIGIN_LONG,
        CENTER_LONG,
        STRAIGHT_VERT_POLE_LONG,
        FALSE_EASTING,
        FALSE_ORIGIN_EASTING,
        CENTER_EASTING,
        FALSE_NORTHING,
        FALSE_ORIGIN_NORTHING,
        CENTER_NORTHING,
    ];
    let parallels = match transform {
        CT_ALBERS | CT_LAMBERT_2SP => {
            if scale.is_some_and(|s| s != 1.0) {
                return Err(format!(
                    "coordinate transform {transform} with a scale factor"
                ));
            }
            true
        }
        CT_LAMBERT_1SP | CT_STEREOGRAPHIC | CT_POLAR_STEREOGRAPHIC | CT_TRANSVERSE_MERCATOR => {
            false
        }
        _ => return Ok(()),
    };
    for id in PARAMETERS {
        let read = ORIGIN.contains(id)
            || matches!(*id, SCALE_AT_NAT_ORIGIN | SCALE_AT_CENTER)
            || (parallels && matches!(*id, STD_PARALLEL_1 | STD_PARALLEL_2));
        if !read && k.find(*id)?.is_some() {
            return Err(format!(
                "coordinate transform {transform} does not read key {id}"
            ));
        }
    }
    Ok(())
}

/// Every key must be one this reader knows.
fn known_keys_only(k: &GeoTags) -> Result<(), String> {
    for index in 0..k.key_count()? {
        let key = k.key_at(index).ok_or("the geokey directory is short")?;
        if !KNOWN.contains(&key.id) {
            return Err(format!(
                "geokey {} is not one this reader understands",
                key.id
            ));
        }
    }
    Ok(())
}

fn named_or_wkt(def: CrsDef) -> Crs {
    if def == CrsDef::shg() {
        Crs::Named(NamedCrs::Shg)
    } else if def == CrsDef::hrap() {
        Crs::Named(NamedCrs::Hrap)
    } else {
        Crs::Wkt(def.to_wkt())
    }
}

/// A file's verbatim text, named when it is exactly what the sink writes for a named CRS. A
/// `.prj` has no other way to carry the name.
pub(crate) fn canonical(crs: Crs) -> Crs {
    match &crs {
        Crs::Wkt(text) if *text == CrsDef::shg().to_wkt() => Crs::Named(NamedCrs::Shg),
        Crs::Wkt(text) if *text == CrsDef::hrap().to_wkt() => Crs::Named(NamedCrs::Hrap),
        _ => crs,
    }
}

/// Keys have no place for `TOWGS84`, and GDAL reads the keys, so text that adds a shift to them is
/// another CRS: measured with GDAL 3.12.2, the file's points transform without the shift. `Shifted`
/// therefore equals nothing the keys can state, and the sink writes such text by the citation route.
fn same_crs(a: &CrsDef, b: &CrsDef) -> bool {
    a.projection == b.projection && a.a == b.a && a.rf == b.rf && a.datum == b.datum
}

/// The keys say nothing but "see the citation": the route GDAL and the sink use for what keys
/// cannot map.
fn keys_defer_to_citation(k: &GeoTags, wkt: &str) -> Result<bool, String> {
    for index in 0..k.key_count()? {
        let key = k.key_at(index).ok_or("the geokey directory is short")?;
        let silent = match key.id {
            GT_MODEL_TYPE | GT_RASTER_TYPE | GT_CITATION | PCS_CITATION => true,
            PROJECTED_TYPE | GEOGRAPHIC_TYPE | PROJECTION | PROJ_TRANSFORM => {
                key.location == LOC_SHORT && key.value == USER_DEFINED
            }
            _ => false,
        };
        if !silent {
            return Ok(false);
        }
    }
    let root_model = if wkt.trim_start().starts_with("GEOGCS[") {
        2
    } else {
        1
    };
    match k.short(GT_MODEL_TYPE)? {
        None | Some(USER_DEFINED) => Ok(true),
        Some(model) if model == root_model => Ok(true),
        Some(model) => Err(format!(
            "model type {model} with a citation's WKT of another kind"
        )),
    }
}

/// Equal at 15 significant digits: WKT prints the same unit with 15, 16 or 17.
fn same_unit_factor(a: f64, b: f64) -> bool {
    format!("{a:.14e}") == format!("{b:.14e}")
}

/// Metres per linear unit, for the units a Transverse Mercator comparison may meet, and the names
/// ESRI and GDAL print for them. PROJ reads the factor, so a name that says otherwise is a
/// contradiction in the text that this reader will not settle.
fn same_linear_unit(code: Option<u16>, name: Option<&str>, stated: Option<f64>) -> bool {
    let (metres, names): (f64, &[&str]) = match code {
        Some(9001) => (1.0, &["meter", "metre"]),
        Some(9002) => (0.3048, &["foot", "foot_international"]),
        Some(9003) => (1200.0 / 3937.0, &["foot_us", "us_survey_foot"]),
        _ => return false,
    };
    name.is_some_and(|n| names.contains(&n)) && stated.is_some_and(|s| same_unit_factor(s, metres))
}

/// Compare what keys and text say for a CRS `CrsDef` cannot hold. Only Transverse Mercator, the
/// shape of Vortex's `hms_cn_grid.tif`, has a comparison; anything else cannot be checked.
///
/// A file that passes is reported as stating the text, and GDAL reads the keys, so every fact of the
/// text must be shown equal to the keys. The pattern below names each field of [`WktFacts`] so that a
/// new one cannot be skipped by accident. A fact the keys have no way to state cannot be compared,
/// and is refused: `TOWGS84`, `AUTHORITY` (there is no EPSG database here to compare a code with),
/// `EXTENSION`, and axes other than the east and north GeoTIFF's projected keys mean.
fn keys_state_this_transverse_mercator(k: &GeoTags, wkt: &str) -> Result<(), String> {
    let cannot = "the keys and a citation's WKT this library cannot parse cannot be compared";
    let WktFacts {
        datum: text_datum_name,
        a: text_a,
        rf: text_rf,
        shifts,
        prime_meridian_name,
        prime_meridian,
        angular_unit_name,
        angular_unit,
        geographic_axes,
        projection,
        parameters,
        linear_unit_name,
        linear_unit,
        projected_axes,
        authorities,
        extensions,
    } = wkt_facts(wkt.trim()).map_err(|e| e.to_string())?;
    if k.short(GT_MODEL_TYPE)? != Some(1)
        || k.short(PROJECTED_TYPE)? != Some(USER_DEFINED)
        || !matches!(k.short(PROJECTION)?, None | Some(USER_DEFINED))
        || k.short(PROJ_TRANSFORM)? != Some(CT_TRANSVERSE_MERCATOR)
        || !projection
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("Transverse_Mercator"))
    {
        return Err(cannot.to_string());
    }
    // `geographic` accepts only keys stating the degree from Greenwich, so those are what the text
    // must state too.
    let (datum, a, rf) = geographic(k)?;
    let text_datum =
        Datum::classify_stated(&text_datum_name, text_a, text_rf, None, None, None, None);
    if (a, rf) != (text_a, text_rf) || datum != text_datum {
        return Err("the keys and the citation's WKT state different datums".to_string());
    }
    if shifts.is_some() {
        return Err("a citation's TOWGS84, which keys cannot state".to_string());
    }
    if authorities != 0 {
        return Err("a citation's AUTHORITY, which cannot be compared with the keys".to_string());
    }
    if extensions != 0 {
        return Err("a citation's EXTENSION, which cannot be compared with the keys".to_string());
    }
    if !geographic_axes.is_empty()
        || !(projected_axes.is_empty() || projected_axes == ["EAST", "NORTH"])
    {
        return Err("a citation's axes, which the keys do not state".to_string());
    }
    if prime_meridian != 0.0 || prime_meridian_name != "greenwich" {
        return Err("a citation's prime meridian other than Greenwich".to_string());
    }
    if angular_unit_name != "degree"
        || !same_unit_factor(angular_unit, std::f64::consts::PI / 180.0)
    {
        return Err("the keys and the citation's WKT state different angular units".to_string());
    }
    if !same_linear_unit(
        k.short(PROJ_LINEAR_UNITS)?,
        linear_unit_name.as_deref(),
        linear_unit,
    ) || k.double(PROJ_LINEAR_SIZE)?.is_some()
    {
        return Err("the keys and the citation's WKT state different units".to_string());
    }
    let false_easting = k.double(FALSE_EASTING)?.unwrap_or(0.0);
    let false_northing = k.double(FALSE_NORTHING)?.unwrap_or(0.0);
    // Without this key libgeotiff takes false easting and northing as metres.
    if k.short(PROJ_LINEAR_INTERP_CORRECT)? != Some(1)
        && (false_easting, false_northing) != (0.0, 0.0)
    {
        return Err("false easting with no statement of its unit".to_string());
    }
    let expected = [
        (
            "central_meridian",
            k.double(NAT_ORIGIN_LONG)?.unwrap_or(0.0),
        ),
        (
            "latitude_of_origin",
            k.double(NAT_ORIGIN_LAT)?.unwrap_or(0.0),
        ),
        (
            "scale_factor",
            k.double(SCALE_AT_NAT_ORIGIN)?.unwrap_or(1.0),
        ),
        ("false_easting", false_easting),
        ("false_northing", false_northing),
    ];
    if parameters.len() != expected.len() {
        return Err("the citation's WKT states other parameters than the keys".to_string());
    }
    for (name, value) in expected {
        if !parameters
            .iter()
            .any(|(n, v)| n == name && v.to_bits() == value.to_bits())
        {
            return Err(format!("the keys and the citation's WKT differ in {name}"));
        }
    }
    for id in PARAMETERS {
        let read = matches!(
            *id,
            NAT_ORIGIN_LONG | NAT_ORIGIN_LAT | SCALE_AT_NAT_ORIGIN | FALSE_EASTING | FALSE_NORTHING
        );
        if !read && k.find(*id)?.is_some() {
            return Err(format!("Transverse Mercator does not read key {id}"));
        }
    }
    Ok(())
}

/// Whether keys beside a citation's WKT let the text stand. A failed parse of either side must not
/// remove the check: GDAL can read the keys and ignore the text.
fn check_keys_against_citation(k: &GeoTags, wkt: &str) -> Result<(), String> {
    if keys_defer_to_citation(k, wkt)? {
        return Ok(());
    }
    match (numeric(k), CrsDef::parse_wkt(wkt)) {
        // Transverse Mercator keeps its stricter comparison, fact by fact: it is the one that
        // reads feet, where a false easting's unit is itself a statement of the keys.
        (_, Ok(from_text))
            if matches!(from_text.projection, Projection::TransverseMercator { .. }) =>
        {
            keys_state_this_transverse_mercator(k, wkt)
        }
        (Ok(from_keys), Ok(from_text)) => {
            if same_crs(&from_keys, &from_text) {
                Ok(())
            } else {
                Err("the keys and the citation's WKT state different CRSs".to_string())
            }
        }
        (Err(why), Ok(_)) => Err(format!(
            "the keys cannot be checked against the citation's WKT: {why}"
        )),
        (_, Err(_)) => keys_state_this_transverse_mercator(k, wkt),
    }
}

fn try_resolve(k: &GeoTags) -> Result<Option<Crs>, String> {
    if k.keys.is_none() {
        return Ok(None);
    }
    known_keys_only(k)?;
    if k.text(GEOG_CITATION)?
        .is_some_and(|t| t.contains(PE_PREFIX))
    {
        return Err(
            "WKT in the geographic citation, which this reader does not compare".to_string(),
        );
    }
    if let Some(wkt) = k.pe_string()? {
        if !is_wkt1_crs(wkt) {
            return Err("the citation's WKT is not a complete WKT1 PROJCS or GEOGCS".to_string());
        }
        check_keys_against_citation(k, wkt)?;
        return Ok(Some(canonical(Crs::Wkt(wkt.to_string()))));
    }
    numeric(k).map(|def| Some(named_or_wkt(def)))
}

/// What GDAL 3.2.1 returns from `GetProjection` for SHG stated in numeric keys, after the `GEOGCS` name:
/// `GTIFGetOGISDefn` (`frmts/gtiff/gt_wkt_srs.cpp`), then OSR's WKT1 export. Read from Vortex 1.1.0's
/// and 0.13.3's own runtime on 72 public fixtures, all one text.
const GDAL_SHG_AFTER_GEOGCS_NAME: &str = concat!(
    "\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,",
    "298.257222101,AUTHORITY[\"EPSG\",\"7019\"]],AUTHORITY[\"EPSG\",\"6269\"]],PRIMEM[\"Greenwich\",0],",
    "UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]]],",
    "PROJECTION[\"Albers_Conic_Equal_Area\"],PARAMETER[\"latitude_of_center\",23],",
    "PARAMETER[\"longitude_of_center\",-96],PARAMETER[\"standard_parallel_1\",29.5],",
    "PARAMETER[\"standard_parallel_2\",45.5],PARAMETER[\"false_easting\",0],",
    "PARAMETER[\"false_northing\",0],UNIT[\"metre\",1,AUTHORITY[\"EPSG\",\"9001\"]],",
    "AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]]"
);

/// The `GEOGCS` name GDAL takes from a citation: `CitationStringParse` (`frmts/gtiff/gt_citation.cpp`),
/// which keeps `GCS Name = `, and with no named field keeps the last field it read. So
/// `GCS_North_American_1983|Greenwich|`, in Vortex's public `test_albers.tiff`, is named `Greenwich`.
fn geogcs_name_of_citation(citation: &str) -> Option<&str> {
    const OTHERS: [&str; 7] = [
        "PCS Name = ",
        "PRJ Name = ",
        "LUnits = ",
        "Datum = ",
        "Ellipsoid = ",
        "Primem = ",
        "AUnits = ",
    ];
    if let Some((_, rest)) = citation.split_once("GCS Name = ") {
        return rest.split('|').next();
    }
    if OTHERS.iter().any(|other| citation.contains(other)) {
        return None;
    }
    citation.rsplit('|').find(|field| !field.is_empty())
}

// `text` with the quoted name after the first `node` replaced.
fn renamed(text: &str, node: &str, name: &str) -> Option<String> {
    let start = text.find(node)? + node.len();
    let end = start + text[start..].find('"')?;
    Some(format!("{}{name}{}", &text[..start], &text[end..]))
}

/// The text a DSS record's form is decided from, for a projected CRS stated in numeric keys.
///
/// Vortex's `DssUtil.getGridInfo` picks the record's form from the names GDAL gives the CRS: the root
/// holding `albers` makes an Albers record, whose datum is NAD83 if the `GEOGCS` name holds `83`.
/// The root is the projected citation, else `GTCitationGeoKey`, else `unnamed` (`GTIFGetOGISDefn`,
/// `frmts/gtiff/gt_wkt_srs.cpp`). So a name not holding `albers` is a specified grid even when the
/// parameters are exactly SHG's, and the frame's `NamedCrs::Shg` cannot decide it. That grid keeps
/// the text, which is GDAL's where the parameters are SHG's. For anything else it keeps the file's
/// two names on this library's wording of the numbers, as GDAL's wording is not reproduced. An EPSG
/// code or a verbatim citation goes the other routes: GDAL names those itself.
pub(crate) fn stored_text(k: &GeoTags) -> Option<String> {
    // An EPSG UTM code: GDAL names the CRS from its database, and that name is the code's own
    // rule, so the record's name is Vortex's. The rest is this library's wording of the zone.
    if let Some(utm) = k.short(PROJECTED_TYPE).ok()?.and_then(epsg_utm_zone) {
        if k.pe_string().ok()?.is_some() {
            return None;
        }
        let def = numeric(k).ok()?;
        return renamed(&def.to_wkt(), "PROJCS[\"", &utm.epsg_name());
    }
    if k.short(PROJECTED_TYPE).ok()? != Some(USER_DEFINED) || k.pe_string().ok()?.is_some() {
        return None;
    }
    let def = numeric(k).ok()?;
    let name = match k.text(PCS_CITATION).ok()? {
        Some(name) => name,
        None => k.text(GT_CITATION).ok()?.unwrap_or("unnamed"),
    };
    let geogcs = k
        .text(GEOG_CITATION)
        .ok()?
        .and_then(geogcs_name_of_citation);
    if [Some(name), geogcs]
        .iter()
        .flatten()
        .any(|text| text.contains(['"', '[', ']']))
    {
        return None;
    }
    // An Albers record keeps numbers, not this text, so SHG's wording matters only under another name.
    if def == CrsDef::shg() && !name.to_lowercase().contains("albers") {
        return Some(format!(
            "PROJCS[\"{name}\",GEOGCS[\"{}{GDAL_SHG_AFTER_GEOGCS_NAME}",
            geogcs.unwrap_or("NAD83")
        ));
    }
    let text = renamed(&def.to_wkt(), "PROJCS[\"", name)?;
    match geogcs {
        Some(geogcs) => renamed(&text, "GEOGCS[\"", geogcs),
        None => Some(text),
    }
}

/// What the keys say.
pub(crate) fn resolve(k: &GeoTags) -> CrsResolution {
    match try_resolve(k) {
        Ok(Some(crs)) => CrsResolution::Stated(crs),
        Ok(None) => CrsResolution::NotInFile,
        Err(reason) => CrsResolution::Unresolved(reason),
    }
}

/// Keys for the sink, in the three tags' forms.
#[derive(Debug, Default)]
pub(crate) struct EncodedKeys {
    pub directory: Vec<u16>,
    pub doubles: Vec<f64>,
    pub ascii: String,
}

#[derive(Default)]
struct Encoder {
    shorts: Vec<(u16, u16)>,
    doubles: Vec<(u16, f64)>,
    ascii: Vec<(u16, String)>,
}

impl Encoder {
    fn short(&mut self, id: u16, value: u16) {
        self.shorts.push((id, value));
    }

    fn double(&mut self, id: u16, value: f64) {
        self.doubles.push((id, value));
    }

    fn finish(self) -> EncodedKeys {
        let mut out = EncodedKeys::default();
        let mut all: Vec<(u16, u16, u16, u16)> = Vec::new();
        for (id, value) in self.shorts {
            all.push((id, LOC_SHORT, 1, value));
        }
        for (id, value) in self.doubles {
            all.push((id, LOC_DOUBLE, 1, out.doubles.len() as u16));
            out.doubles.push(value);
        }
        for (id, text) in self.ascii {
            let offset = out.ascii.len() as u16;
            out.ascii.push_str(&text);
            out.ascii.push('|');
            all.push((id, LOC_ASCII, (text.len() + 1) as u16, offset));
        }
        all.sort_by_key(|k| k.0);
        out.directory = vec![1, 1, 0, all.len() as u16];
        for (id, location, count, value) in all {
            out.directory
                .extend_from_slice(&[id, location, count, value]);
        }
        out
    }
}

fn encode_geographic(e: &mut Encoder, def: &CrsDef) {
    match def.datum {
        Datum::Wgs84 if def.a == 6378137.0 && def.rf == 298.257223563 => {
            e.short(GEOGRAPHIC_TYPE, 4326);
        }
        Datum::Nad83 if def.a == 6378137.0 && def.rf == 298.257222101 => {
            e.short(GEOGRAPHIC_TYPE, 4269);
        }
        _ => {
            e.short(GEOGRAPHIC_TYPE, USER_DEFINED);
            e.short(GEOG_DATUM, USER_DEFINED);
            e.short(GEOG_ELLIPSOID, USER_DEFINED);
            e.double(GEOG_SEMI_MAJOR, def.a);
            if def.rf == 0.0 {
                e.double(GEOG_SEMI_MINOR, def.a);
            } else {
                e.double(GEOG_INV_FLATTENING, def.rf);
            }
            let name = match &def.datum {
                Datum::Other(name) | Datum::Shifted(name) => name.as_str(),
                _ => "unknown",
            };
            e.ascii.push((GEOG_CITATION, format!("Datum = {name}")));
        }
    }
    e.short(GEOG_ANGULAR_UNITS, 9102);
}

fn encode_numeric(e: &mut Encoder, def: &CrsDef) {
    if def.projection == Projection::LongLat {
        e.short(GT_MODEL_TYPE, 2);
        encode_geographic(e, def);
        return;
    }
    e.short(GT_MODEL_TYPE, 1);
    encode_geographic(e, def);
    e.short(PROJECTED_TYPE, USER_DEFINED);
    e.short(PROJECTION, USER_DEFINED);
    e.short(PROJ_LINEAR_UNITS, 9001);
    match def.projection {
        // `parse_wkt` refuses Web Mercator, so no frame reaches a sink with it, and `encode`
        // keeps Transverse Mercator on the citation route, as it was written before this
        // library could transform it: that form is the one compared with GDAL, feet included.
        Projection::LongLat | Projection::WebMercator | Projection::TransverseMercator { .. } => {}
        Projection::Albers {
            lat_1,
            lat_2,
            lat_0,
            lon_0,
            x_0,
            y_0,
        } => {
            e.short(PROJ_TRANSFORM, CT_ALBERS);
            e.double(STD_PARALLEL_1, lat_1);
            e.double(STD_PARALLEL_2, lat_2);
            e.double(NAT_ORIGIN_LAT, lat_0);
            e.double(NAT_ORIGIN_LONG, lon_0);
            e.double(FALSE_EASTING, x_0);
            e.double(FALSE_NORTHING, y_0);
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
            if k_0 == 1.0 {
                e.short(PROJ_TRANSFORM, CT_LAMBERT_2SP);
                e.double(STD_PARALLEL_1, lat_1);
                e.double(STD_PARALLEL_2, lat_2);
                e.double(FALSE_ORIGIN_LAT, lat_0);
                e.double(FALSE_ORIGIN_LONG, lon_0);
                e.double(FALSE_ORIGIN_EASTING, x_0);
                e.double(FALSE_ORIGIN_NORTHING, y_0);
            } else {
                e.short(PROJ_TRANSFORM, CT_LAMBERT_1SP);
                e.double(NAT_ORIGIN_LAT, lat_0);
                e.double(NAT_ORIGIN_LONG, lon_0);
                e.double(SCALE_AT_NAT_ORIGIN, k_0);
                e.double(FALSE_EASTING, x_0);
                e.double(FALSE_NORTHING, y_0);
            }
        }
        Projection::Stereographic {
            lat_0,
            lon_0,
            k_0,
            lat_ts,
            x_0,
            y_0,
        } => {
            match lat_ts {
                Some(ts) => {
                    e.short(PROJ_TRANSFORM, CT_POLAR_STEREOGRAPHIC);
                    e.double(NAT_ORIGIN_LAT, ts);
                    e.double(STRAIGHT_VERT_POLE_LONG, lon_0);
                    e.double(SCALE_AT_NAT_ORIGIN, 1.0);
                }
                None => {
                    e.short(PROJ_TRANSFORM, CT_STEREOGRAPHIC);
                    e.double(CENTER_LAT, lat_0);
                    e.double(CENTER_LONG, lon_0);
                    e.double(SCALE_AT_NAT_ORIGIN, k_0);
                }
            }
            e.double(FALSE_EASTING, x_0);
            e.double(FALSE_NORTHING, y_0);
        }
    }
}

fn refuse(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "GeoTIFF CRS".to_string(),
        reason: reason.into(),
    }
}

/// A geographic WKT that does not state latitude first. GDAL takes one with no axes, as a `.prj`
/// usually has, as longitude first, and reads a geographic EPSG code as latitude first, and
/// `IsSame` tells them apart. Keys cannot say which; the citation route can.
fn not_latitude_first(wkt: &str) -> bool {
    wkt.trim_start().starts_with("GEOGCS[")
        && match (
            wkt.find("AXIS[\"Latitude\""),
            wkt.find("AXIS[\"Longitude\""),
        ) {
            (Some(latitude), Some(longitude)) => longitude < latitude,
            _ => true,
        }
}

/// Keys for a frame's CRS, or `None` for an undefined one, which writes no keys at all.
pub(crate) fn encode(crs: &Crs) -> Result<Option<EncodedKeys>, VortexError> {
    let mut e = Encoder::default();
    e.short(GT_RASTER_TYPE, 1);
    match crs {
        Crs::Undefined => return Ok(None),
        Crs::Proj(_) => return Err(refuse("a PROJ string has no GeoTIFF form here")),
        Crs::Named(NamedCrs::Shg) => {
            encode_numeric(&mut e, &CrsDef::shg());
            e.ascii.push((GT_CITATION, "SHG".to_string()));
        }
        Crs::Named(NamedCrs::Hrap) => {
            encode_numeric(&mut e, &CrsDef::hrap());
            e.ascii.push((GT_CITATION, "HRAP".to_string()));
        }
        Crs::Wkt(text) => {
            let trimmed = text.as_str();
            if !is_wkt1_crs(trimmed) {
                return Err(refuse(
                    "the frame's WKT is not a complete WKT1 PROJCS or GEOGCS",
                ));
            }
            if !trimmed.is_ascii() || trimmed.contains('|') {
                return Err(refuse(
                    "the frame's WKT has a character GeoTIFF text cannot hold",
                ));
            }
            let citation = format!("{PE_PREFIX}{trimmed}");
            if citation.len() + 1 > MAX_ASCII {
                return Err(refuse(format!(
                    "{} bytes of WKT is too long",
                    trimmed.len()
                )));
            }
            match CrsDef::parse_wkt(trimmed) {
                // Keys would drop a shift: see `same_crs`.
                Ok(def)
                    if !not_latitude_first(trimmed)
                        && !matches!(def.datum, Datum::Shifted(_))
                        && !matches!(def.projection, Projection::TransverseMercator { .. }) =>
                {
                    encode_numeric(&mut e, &def);
                    e.ascii.push((GT_CITATION, citation));
                }
                _ => {
                    // GDAL's route for what it cannot map, read through a fixed buffer.
                    if citation.len() >= GDAL_CITATION_BUFFER {
                        return Err(refuse(format!(
                            "{} bytes of WKT this library cannot express as keys is longer than \
                             GDAL's {GDAL_CITATION_BUFFER}-byte citation buffer",
                            trimmed.len()
                        )));
                    }
                    e.short(GT_MODEL_TYPE, USER_DEFINED);
                    e.ascii.push((PCS_CITATION, citation));
                }
            }
        }
    }
    Ok(Some(e.finish()))
}

#[cfg(test)]
mod stored_text_tests {
    use super::{geogcs_name_of_citation, renamed};

    #[test]
    fn a_citation_names_the_geogcs_as_gdal_parses_it() {
        let name = geogcs_name_of_citation;
        assert_eq!(
            name("GCS Name = NAD83|Ellipsoid = GRS 1980|Primem = Greenwich"),
            Some("NAD83")
        );
        assert_eq!(name("GCS_North_American_1983|Greenwich"), Some("Greenwich"));
        assert_eq!(name("NAD83"), Some("NAD83"));
        // Named fields, and no name among them: GDAL looks elsewhere, so this does not guess.
        assert_eq!(name("Datum = NAD83|Primem = Greenwich"), None);
    }

    #[test]
    fn only_the_first_of_a_node_is_renamed() {
        let text = "PROJCS[\"a\",GEOGCS[\"b\",X[\"GEOGCS[\"c\"]]";
        assert_eq!(
            renamed(text, "GEOGCS[\"", "n").unwrap(),
            "PROJCS[\"a\",GEOGCS[\"n\",X[\"GEOGCS[\"c\"]]"
        );
        assert_eq!(renamed(text, "NONE[\"", "n"), None);
    }
}

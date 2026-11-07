//! The independent review's P50 findings, each reproduced through the public API. The crafted files
//! state things GDAL's own writers do not, so they are built here byte by byte.

use dss_vortex::sink_raster::{write_asc, write_geotiff, AscOptions, TiffOptions};
use dss_vortex::source::raster::{CrsStatus, RasterFile, ReadOptions, UnknownCrs};
use dss_vortex::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
    VortexError,
};
use std::fs;
use std::path::{Path, PathBuf};

const MODEL: u16 = 1024;
const RASTER: u16 = 1025;
const GT_CITATION: u16 = 1026;
const GEOG: u16 = 2048;
const GEOG_CITATION: u16 = 2049;
const GEOG_DATUM: u16 = 2050;
const GEOG_ANGULAR: u16 = 2054;
const GEOG_ELLIPSOID: u16 = 2056;
const GEOG_A: u16 = 2057;
const GEOG_B: u16 = 2058;
const GEOG_RF: u16 = 2059;
const INTERP: u16 = 3059;
const PCS: u16 = 3072;
const PCS_CITATION: u16 = 3073;
const PROJECTION: u16 = 3074;
const TRANSFORM: u16 = 3075;
const LINEAR: u16 = 3076;
const STD1: u16 = 3078;
const STD2: u16 = 3079;
const NAT_LONG: u16 = 3080;
const NAT_LAT: u16 = 3081;
const FE: u16 = 3082;
const FN: u16 = 3083;
const SCALE: u16 = 3092;
const USER: u16 = 32767;

/// One geokey as the file states it.
#[derive(Clone)]
enum V {
    S(u16),
    D(f64),
    A(String),
}

type Tag = (u16, u16, u32, Vec<u8>);

fn le_doubles(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|d| d.to_le_bytes()).collect()
}

/// A little-endian classic TIFF holding `data` in one strip, with a north-up transform.
fn assemble(compression: u16, data: &[u8], extra: &[Tag]) -> Vec<u8> {
    let mut tags: Vec<Tag> = vec![
        (256, 4, 1, 2u32.to_le_bytes().to_vec()),
        (257, 4, 1, 2u32.to_le_bytes().to_vec()),
        (258, 3, 1, 32u16.to_le_bytes().to_vec()),
        (259, 3, 1, compression.to_le_bytes().to_vec()),
        (262, 3, 1, 1u16.to_le_bytes().to_vec()),
        (273, 4, 1, Vec::new()),
        (277, 3, 1, 1u16.to_le_bytes().to_vec()),
        (278, 4, 1, 2u32.to_le_bytes().to_vec()),
        (279, 4, 1, (data.len() as u32).to_le_bytes().to_vec()),
        (339, 3, 1, 3u16.to_le_bytes().to_vec()),
        (33550, 12, 3, le_doubles(&[1.0, 1.0, 0.0])),
        (33922, 12, 6, le_doubles(&[0.0, 0.0, 0.0, 10.0, 20.0, 0.0])),
    ];
    tags.extend_from_slice(extra);
    tags.sort_by_key(|t| t.0);
    let table = 8 + 2 + tags.len() * 12 + 4;
    let blob_size: usize = tags
        .iter()
        .filter(|t| t.3.len() > 4)
        .map(|t| t.3.len())
        .sum();
    let data_at = (table + blob_size) as u32;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(tags.len() as u16).to_le_bytes());
    let mut at = table as u32;
    let mut blobs = Vec::new();
    for (tag, kind, count, value) in &tags {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if *tag == 273 {
            out.extend_from_slice(&data_at.to_le_bytes());
        } else if value.len() <= 4 {
            let mut inline = value.clone();
            inline.resize(4, 0);
            out.extend_from_slice(&inline);
        } else {
            out.extend_from_slice(&at.to_le_bytes());
            at += value.len() as u32;
            blobs.extend_from_slice(value);
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&blobs);
    out.extend_from_slice(data);
    out
}

fn cell_bytes() -> Vec<u8> {
    [1.5f32, 2.5, 3.5, 4.5]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

/// A 2 x 2 float32 TIFF stating the given geokeys.
fn tiff(keys: &[(u16, V)]) -> Vec<u8> {
    let mut sorted = keys.to_vec();
    sorted.sort_by_key(|k| k.0);
    let (mut directory, mut doubles, mut ascii) = (
        vec![1u16, 1, 0, sorted.len() as u16],
        Vec::new(),
        String::new(),
    );
    for (id, value) in &sorted {
        match value {
            V::S(v) => directory.extend_from_slice(&[*id, 0, 1, *v]),
            V::D(v) => {
                directory.extend_from_slice(&[*id, 34736, 1, doubles.len() as u16]);
                doubles.push(*v);
            }
            V::A(text) => {
                directory.extend_from_slice(&[
                    *id,
                    34737,
                    text.len() as u16 + 1,
                    ascii.len() as u16,
                ]);
                ascii.push_str(text);
                ascii.push('|');
            }
        }
    }
    let mut tags: Vec<Tag> = Vec::new();
    if !sorted.is_empty() {
        let words: Vec<u8> = directory.iter().flat_map(|w| w.to_le_bytes()).collect();
        tags.push((34735, 3, directory.len() as u32, words));
    }
    if !doubles.is_empty() {
        tags.push((34736, 12, doubles.len() as u32, le_doubles(&doubles)));
    }
    if !ascii.is_empty() {
        let mut bytes = ascii.into_bytes();
        bytes.push(0);
        tags.push((34737, 2, bytes.len() as u32, bytes));
    }
    assemble(1, &cell_bytes(), &tags)
}

fn read(keys: &[(u16, V)]) -> Result<RasterFile, VortexError> {
    RasterFile::from_bytes("a.tif", &tiff(keys), None, ReadOptions::default())
}

fn refused(label: &str, keys: &[(u16, V)]) {
    match read(keys) {
        Err(VortexError::Refused { what, .. }) if what == "raster CRS" => {}
        Err(other) => panic!("{label}: another error: {other}"),
        Ok(file) => panic!("{label}: read as {:?}", file.frame().parts().crs),
    }
    // The caller's other choice is an explicit absence, never the competing statement.
    let options = ReadOptions {
        unknown_crs: UnknownCrs::Undefined,
        ..ReadOptions::default()
    };
    let file = RasterFile::from_bytes("a.tif", &tiff(keys), None, options).unwrap();
    assert_eq!(file.frame().parts().crs, Crs::Undefined, "{label}");
    assert!(
        matches!(file.crs_status(), CrsStatus::Unresolved { .. }),
        "{label}"
    );
}

fn epsg5070() -> Vec<(u16, V)> {
    vec![(MODEL, V::S(1)), (RASTER, V::S(1)), (PCS, V::S(5070))]
}

fn with(mut keys: Vec<(u16, V)>, more: &[(u16, V)]) -> Vec<(u16, V)> {
    keys.extend_from_slice(more);
    keys
}

#[test]
fn epsg_5070_is_shg_only_when_nothing_contradicts_it() {
    for (label, keys) in [
        ("code alone", epsg5070()),
        (
            "with what GDAL or Esri also state",
            with(
                epsg5070(),
                &[
                    (GEOG, V::S(4269)),
                    (GEOG_DATUM, V::S(6269)),
                    (GEOG_ELLIPSOID, V::S(7019)),
                    (GEOG_ANGULAR, V::S(9102)),
                    (LINEAR, V::S(9001)),
                    (TRANSFORM, V::S(11)),
                    (GEOG_A, V::D(6378137.0)),
                    (GEOG_RF, V::D(298.257222101)),
                ],
            ),
        ),
    ] {
        let file = read(&keys).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            file.frame().parts().crs,
            Crs::Named(NamedCrs::Shg),
            "{label}"
        );
    }
    for (label, more) in [
        (
            "the review's WGS84 and feet",
            vec![(GEOG, V::S(4326)), (LINEAR, V::S(9002))],
        ),
        ("WGS84", vec![(GEOG, V::S(4326))]),
        ("feet", vec![(LINEAR, V::S(9002))]),
        ("US feet", vec![(LINEAR, V::S(9003))]),
        ("WGS84 datum", vec![(GEOG_DATUM, V::S(6326))]),
        ("WGS84 ellipsoid", vec![(GEOG_ELLIPSOID, V::S(7030))]),
        ("a user-defined geographic CRS", vec![(GEOG, V::S(USER))]),
        ("another transform", vec![(TRANSFORM, V::S(1))]),
        ("another projection", vec![(PROJECTION, V::S(16014))]),
        ("radians", vec![(GEOG_ANGULAR, V::S(9101))]),
        ("another semi-major axis", vec![(GEOG_A, V::D(6378206.4))]),
        (
            "a citation naming WGS84",
            vec![(GEOG_CITATION, V::A("Datum = WGS84".to_string()))],
        ),
        (
            "WKT in the geographic citation",
            vec![(GEOG_CITATION, pe(&dss_vortex::crs::CrsDef::shg().to_wkt()))],
        ),
    ] {
        refused(label, &with(epsg5070(), &more));
    }
}

fn pe(text: &str) -> V {
    V::A(format!("ESRI PE String = {text}"))
}

const WGS84: &str = "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]";
const TM_FEET: &str = "PROJCS[\"NAD_1983_Florida_East_ftUS\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"false_easting\",656166.667],PARAMETER[\"false_northing\",0.0],PARAMETER[\"central_meridian\",-81.0],PARAMETER[\"scale_factor\",0.999941177],PARAMETER[\"latitude_of_origin\",24.3333333333333],UNIT[\"Foot_US\",0.30480060960121924]]";

/// What ArcGIS writes for `TM_FEET`: the shape of Vortex's `hms_cn_grid.tif`.
fn tm_keys() -> Vec<(u16, V)> {
    vec![
        (MODEL, V::S(1)),
        (RASTER, V::S(1)),
        (GEOG, V::S(4269)),
        (GEOG_DATUM, V::S(6269)),
        (GEOG_ELLIPSOID, V::S(7019)),
        (GEOG_ANGULAR, V::S(9102)),
        (GEOG_A, V::D(6378137.0)),
        (GEOG_RF, V::D(298.257222101)),
        (INTERP, V::S(1)),
        (PCS, V::S(USER)),
        (PROJECTION, V::S(USER)),
        (TRANSFORM, V::S(1)),
        (LINEAR, V::S(9003)),
        (NAT_LONG, V::D(-81.0)),
        (NAT_LAT, V::D(24.3333333333333)),
        (FE, V::D(656166.667)),
        (FN, V::D(0.0)),
        (SCALE, V::D(0.999941177)),
    ]
}

fn replaced(keys: Vec<(u16, V)>, id: u16, value: V) -> Vec<(u16, V)> {
    keys.into_iter()
        .map(|k| if k.0 == id { (id, value.clone()) } else { k })
        .collect()
}

#[test]
fn keys_that_cannot_be_checked_do_not_license_a_citations_wkt() {
    // The review's file: rasterio reads EPSG:32615 and this reader said WGS84.
    refused(
        "unsupported EPSG with a WGS84 citation",
        &[
            (MODEL, V::S(1)),
            (RASTER, V::S(1)),
            (PCS, V::S(32615)),
            (GT_CITATION, pe(WGS84)),
        ],
    );
    refused(
        "a supported code against a citation it disagrees with",
        &[
            (MODEL, V::S(2)),
            (GEOG, V::S(4269)),
            (GT_CITATION, pe(WGS84)),
        ],
    );
    refused(
        "readable keys and a citation that cannot be parsed",
        &[
            (MODEL, V::S(2)),
            (GEOG, V::S(4326)),
            (GT_CITATION, pe(TM_FEET)),
        ],
    );
    refused(
        "a geographic model with a projected citation",
        &[(MODEL, V::S(2)), (GT_CITATION, pe(TM_FEET))],
    );
    refused(
        "two citations that differ",
        &[
            (MODEL, V::S(USER)),
            (GT_CITATION, pe(WGS84)),
            (PCS_CITATION, pe(TM_FEET)),
        ],
    );
}

#[test]
fn a_user_defined_citation_still_reads_and_transverse_mercator_keys_are_compared() {
    let sink_route = [
        (MODEL, V::S(USER)),
        (RASTER, V::S(1)),
        (PCS_CITATION, pe(TM_FEET)),
    ];
    let file = read(&sink_route).unwrap();
    assert_eq!(file.frame().parts().crs, Crs::Wkt(TM_FEET.to_string()));

    let agreeing = with(tm_keys(), &[(PCS_CITATION, pe(TM_FEET))]);
    let file = read(&agreeing).unwrap();
    assert_eq!(file.frame().parts().crs, Crs::Wkt(TM_FEET.to_string()));

    for (label, id, value) in [
        ("central meridian", NAT_LONG, V::D(-82.0)),
        ("latitude of origin", NAT_LAT, V::D(24.0)),
        ("false easting", FE, V::D(0.0)),
        ("false northing", FN, V::D(1.0)),
        ("scale", SCALE, V::D(1.0)),
        ("metres", LINEAR, V::S(9001)),
        ("international feet", LINEAR, V::S(9002)),
        ("transform", TRANSFORM, V::S(11)),
        ("ellipsoid", GEOG_RF, V::D(298.257223563)),
        ("no interpretation key", INTERP, V::S(0)),
        ("a PCS code", PCS, V::S(2236)),
    ] {
        refused(
            label,
            &with(
                replaced(tm_keys(), id, value),
                &[(PCS_CITATION, pe(TM_FEET))],
            ),
        );
    }
    let wgs84_codes: Vec<(u16, V)> = replaced(
        replaced(
            replaced(tm_keys(), GEOG, V::S(4326)),
            GEOG_DATUM,
            V::S(6326),
        ),
        GEOG_ELLIPSOID,
        V::S(7030),
    )
    .into_iter()
    .filter(|k| k.0 != GEOG_A && k.0 != GEOG_RF)
    .collect();
    refused(
        "datum codes for WGS84",
        &with(wgs84_codes, &[(PCS_CITATION, pe(TM_FEET))]),
    );
}

/// `tm_keys` beside `TM_FEET` with one piece of its text replaced.
fn tm_with_text(from: &str, to: &str) -> Vec<(u16, V)> {
    assert!(TM_FEET.contains(from), "{from} is not in the text");
    with(tm_keys(), &[(PCS_CITATION, pe(&TM_FEET.replace(from, to)))])
}

const TM_DEGREE: &str = "UNIT[\"Degree\",0.0174532925199433]";
const TM_GEOG_END: &str = "UNIT[\"Degree\",0.0174532925199433]]";
const TM_FOOT: &str = "UNIT[\"Foot_US\",0.30480060960121924]";
const TM_SPHEROID: &str = "298.257222101]";

/// The re-check's file: numeric keys in degrees and a citation with another angular unit. GDAL reads
/// the keys, and this reader used to state the text and write it.
#[test]
fn transverse_mercator_text_whose_angular_unit_differs_from_the_keys_is_refused() {
    for (label, unit) in [
        (
            "the re-check's factor",
            "UNIT[\"Degree\",0.0157079632679490]",
        ),
        ("gradians", "UNIT[\"Grad\",0.0157079632679490]"),
        ("radians", "UNIT[\"Radian\",1.0]"),
        (
            "1e-11 from the degree",
            "UNIT[\"Degree\",0.0174532925299433]",
        ),
        (
            "the degree's factor under another name",
            "UNIT[\"Grad\",0.0174532925199433]",
        ),
    ] {
        refused(label, &tm_with_text(TM_DEGREE, unit));
    }
    // The same factor at the other precisions WKT prints it with.
    for unit in [
        "UNIT[\"Degree\",0.017453292519943295]",
        "UNIT[\"degree\",0.01745329251994328]",
    ] {
        let keys = tm_with_text(TM_DEGREE, unit);
        assert!(matches!(
            read(&keys).unwrap().frame().parts().crs,
            Crs::Wkt(_)
        ));
    }
}

/// A fact the keys have no way to state cannot be shown equal to them, so it does not pass.
#[test]
fn transverse_mercator_text_with_a_fact_keys_cannot_state_is_refused() {
    let authority = ",AUTHORITY[\"EPSG\",\"2236\"]";
    let root_end = format!("{TM_FOOT}]");
    for (label, from, to) in [
        (
            "a shift",
            TM_SPHEROID,
            format!("{TM_SPHEROID},TOWGS84[100,200,300]"),
        ),
        (
            "a zero shift",
            TM_SPHEROID,
            format!("{TM_SPHEROID},TOWGS84[0,0,0]"),
        ),
        (
            "seven shift terms",
            TM_SPHEROID,
            format!("{TM_SPHEROID},TOWGS84[0,0,0,0,0,0,0]"),
        ),
        (
            "a root authority",
            root_end.as_str(),
            format!("{TM_FOOT}{authority}]"),
        ),
        (
            "an authority on the datum",
            TM_SPHEROID,
            format!("{TM_SPHEROID},AUTHORITY[\"EPSG\",\"6269\"]"),
        ),
        (
            "an authority on the spheroid",
            "298.257222101",
            "298.257222101,AUTHORITY[\"EPSG\",\"7019\"]".to_string(),
        ),
        (
            "an authority on a parameter",
            "PARAMETER[\"false_northing\",0.0",
            "PARAMETER[\"false_northing\",0.0,AUTHORITY[\"EPSG\",\"8807\"]".to_string(),
        ),
        (
            "an authority on the unit",
            "0.30480060960121924",
            "0.30480060960121924,AUTHORITY[\"EPSG\",\"9003\"]".to_string(),
        ),
        (
            "an extension",
            root_end.as_str(),
            format!("{TM_FOOT},EXTENSION[\"PROJ4\",\"+proj=longlat\"]]"),
        ),
        (
            "northing first",
            root_end.as_str(),
            format!("{TM_FOOT},AXIS[\"Y\",NORTH],AXIS[\"X\",EAST]]"),
        ),
        (
            "westing",
            root_end.as_str(),
            format!("{TM_FOOT},AXIS[\"X\",WEST],AXIS[\"Y\",NORTH]]"),
        ),
        (
            "geographic axes",
            TM_GEOG_END,
            format!("{TM_DEGREE},AXIS[\"Lat\",NORTH],AXIS[\"Long\",EAST]]"),
        ),
        (
            "another meridian's name at zero",
            "PRIMEM[\"Greenwich\",0.0]",
            "PRIMEM[\"Paris\",0.0]".to_string(),
        ),
        (
            "a unit named as something else",
            TM_FOOT,
            "UNIT[\"Meter\",0.30480060960121924]".to_string(),
        ),
    ] {
        refused(label, &tm_with_text(from, &to));
    }
    // What GeoTIFF's projected keys mean anyway is the same statement.
    let east_north = tm_with_text(
        &root_end,
        &format!("{TM_FOOT},AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]]"),
    );
    assert!(matches!(
        read(&east_north).unwrap().frame().parts().crs,
        Crs::Wkt(_)
    ));
}

/// The same defect where `CrsDef` can parse the text: GDAL reads the keys, which cannot state a shift.
#[test]
fn numeric_keys_beside_text_that_adds_a_shift_are_refused() {
    let text = |datum_tail: &str| {
        format!(
            "PROJCS[\"a\",GEOGCS[\"g\",DATUM[\"D_Custom\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]{datum_tail}],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]"
        )
    };
    let keys = |wkt: &str| {
        vec![
            (MODEL, V::S(1)),
            (RASTER, V::S(1)),
            (GEOG, V::S(USER)),
            (GEOG_DATUM, V::S(USER)),
            (GEOG_ELLIPSOID, V::S(USER)),
            (GEOG_CITATION, V::A("Datum = D_Custom".to_string())),
            (GEOG_A, V::D(6378137.0)),
            (GEOG_RF, V::D(298.257222101)),
            (PCS, V::S(USER)),
            (PROJECTION, V::S(USER)),
            (TRANSFORM, V::S(11)),
            (LINEAR, V::S(9001)),
            (STD1, V::D(29.5)),
            (STD2, V::D(45.5)),
            (NAT_LAT, V::D(23.0)),
            (NAT_LONG, V::D(-96.0)),
            (GT_CITATION, pe(wkt)),
        ]
    };
    // The control: the same keys and text without the shift read.
    let plain = text("");
    assert_eq!(
        read(&keys(&plain)).unwrap().frame().parts().crs,
        Crs::Wkt(plain.clone())
    );
    refused("a shift beside keys", &keys(&text(",TOWGS84[100,200,300]")));
}

/// Text with a shift still reads and writes, by the route that keeps it.
#[test]
fn a_frame_with_a_shift_is_written_by_the_citation_route_and_reads_back() {
    let dir = std::env::temp_dir().join(format!("p50_shift_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let wkt = "PROJCS[\"a\",GEOGCS[\"g\",DATUM[\"D_Custom\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101],TOWGS84[100,200,300]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]";
    let source = dir.join("a.asc");
    fs::write(
        &source,
        "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n3 4\n",
    )
    .unwrap();
    fs::write(dir.join("a.prj"), wkt).unwrap();
    let frame = RasterFile::open(&source).unwrap();
    let target = dir.join("b.tif");
    write_geotiff(frame.frame(), &target, &TiffOptions::default()).unwrap();
    assert_eq!(
        RasterFile::open(&target).unwrap().frame().parts().crs,
        Crs::Wkt(wkt.to_string())
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// Open then write, the only way a refused source could reach a writer.
fn copy_through(source: &Path, target: &Path) -> Result<(), VortexError> {
    let file = RasterFile::open(source)?;
    if target.extension().is_some_and(|e| e == "asc") {
        write_asc(file.frame(), target, &AscOptions::default())
    } else {
        write_geotiff(file.frame(), target, &TiffOptions::default())
    }
}

#[test]
fn a_refused_source_leaves_either_writers_existing_target_untouched() {
    let dir = std::env::temp_dir().join(format!("p50_refused_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good.tif");
    let bad = dir.join("bad.tif");
    let keys = tm_with_text(TM_DEGREE, "UNIT[\"Degree\",0.0157079632679490]");
    fs::write(&bad, tiff(&keys)).unwrap();
    fs::write(
        &good,
        tiff(&with(tm_keys(), &[(PCS_CITATION, pe(TM_FEET))])),
    )
    .unwrap();

    // The same route does write when the source is good, so the refusal is what stops it.
    for name in ["control.tif", "control.asc"] {
        let target = dir.join(name);
        copy_through(&good, &target).unwrap();
        assert!(target.exists());
    }
    for name in ["old.tif", "old.asc"] {
        let target = dir.join(name);
        let sibling = target.with_extension("prj");
        fs::write(&target, b"ORIGINAL RASTER").unwrap();
        fs::write(&sibling, b"ORIGINAL PRJ").unwrap();
        let before: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        match copy_through(&bad, &target) {
            Err(VortexError::Refused { what, .. }) => assert_eq!(what, "raster CRS"),
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(fs::read(&target).unwrap(), b"ORIGINAL RASTER", "{name}");
        assert_eq!(fs::read(&sibling).unwrap(), b"ORIGINAL PRJ", "{name}");
        let after: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(before, after, "{name}: a file came or went");
        fs::remove_file(&target).unwrap();
        fs::remove_file(&sibling).unwrap();
    }
    fs::remove_dir_all(&dir).unwrap();
}

fn albers(extra: &[(u16, V)]) -> Vec<(u16, V)> {
    with(
        vec![
            (MODEL, V::S(1)),
            (GEOG, V::S(4269)),
            (PCS, V::S(USER)),
            (TRANSFORM, V::S(11)),
            (LINEAR, V::S(9001)),
            (STD1, V::D(29.5)),
            (STD2, V::D(45.5)),
            (NAT_LAT, V::D(23.0)),
            (NAT_LONG, V::D(-96.0)),
        ],
        extra,
    )
}

#[test]
fn a_key_nothing_consumes_is_refused() {
    assert_eq!(
        read(&albers(&[])).unwrap().frame().parts().crs,
        Crs::Named(NamedCrs::Shg)
    );
    refused("a scale on Albers", &albers(&[(SCALE, V::D(0.9))]));
    refused(
        "a false easting on a geographic model",
        &[(MODEL, V::S(2)), (GEOG, V::S(4326)), (FE, V::D(5.0))],
    );
    refused(
        "feet on a geographic model",
        &[(MODEL, V::S(2)), (GEOG, V::S(4326)), (LINEAR, V::S(9002))],
    );
}

#[test]
fn an_ellipsoid_alone_does_not_name_a_datum() {
    let grs80_code = vec![
        (MODEL, V::S(2)),
        (GEOG, V::S(USER)),
        (GEOG_DATUM, V::S(USER)),
        (GEOG_ELLIPSOID, V::S(7019)),
    ];
    refused("a GRS80 code and no datum", &grs80_code);
    refused(
        "WGS84 axes and no datum",
        &[
            (MODEL, V::S(2)),
            (GEOG, V::S(USER)),
            (GEOG_A, V::D(6378137.0)),
            (GEOG_RF, V::D(298.257223563)),
        ],
    );
    // A named one does, and a sphere has no datum to name.
    let named = with(
        grs80_code,
        &[(
            GEOG_CITATION,
            V::A("Datum = D_North_American_1983".to_string()),
        )],
    );
    assert!(read(&named).is_ok());
    assert!(read(&[
        (MODEL, V::S(2)),
        (GEOG, V::S(USER)),
        (GEOG_A, V::D(6371000.0)),
        (GEOG_B, V::D(6371000.0))
    ])
    .is_ok());
    refused(
        "a code and a citation for another datum",
        &[
            (MODEL, V::S(2)),
            (GEOG, V::S(4269)),
            (GEOG_CITATION, V::A("Datum = D_Other".to_string())),
        ],
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("p50_review_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

const GRID: &str = "ncols 2\nnrows 2\nxllcorner 10\nyllcorner 18\ncellsize 1\n1.5 2.5\n3.5 4.5\n";

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn frame(crs: Crs) -> GridFrame {
    GridFrame::new(GridFrameParts {
        cells: vec![1.5, 2.5, 3.5, 4.5],
        nx: 2,
        ny: 2,
        row_order: RowOrder::NorthToSouth,
        transform: GeoTransform::axis_aligned(10.0, 1.0, 20.0, -1.0),
        crs,
        nodata: None,
        time: FrameTime::Invariant,
        data_type: DataType::InstVal,
        units: String::new(),
        variable: String::new(),
        description: None,
    })
    .unwrap()
}

/// Sidecars GDAL applies, measured by `oracle/vortex/tests/test_sidecars.py`.
const APPLIED: &[&str] = &[
    "{name}.msk",
    "{name}.MSK",
    "{name}.aux",
    "{stem}.aux",
    "{name}.AUX",
    "{name}.aux.xml",
    "{name}.AUX.XML",
];
/// With an internal georeference GDAL takes no cell, mask or georeference from these.
const NOT_APPLIED: &[&str] = &[
    "{name}.ovr",
    "{name}.tfw",
    "{stem}.tfw",
    "{stem}.wld",
    "{stem}.tab",
    "{name}.xml",
];

fn sidecar(pattern: &str, name: &str, stem: &str) -> String {
    pattern.replace("{name}", name).replace("{stem}", stem)
}

#[test]
fn a_sidecar_gdal_applies_refuses_the_read() {
    let dir = scratch("read_side");
    for (name, stem, bytes) in [
        ("a.tif", "a", tiff(&[(MODEL, V::S(2)), (GEOG, V::S(4326))])),
        ("b.asc", "b", GRID.as_bytes().to_vec()),
    ] {
        let path = dir.join(name);
        fs::write(&path, &bytes).unwrap();
        let plain = RasterFile::open(&path).unwrap();
        for pattern in APPLIED {
            let side = dir.join(sidecar(pattern, name, stem));
            fs::write(&side, b"x").unwrap();
            match RasterFile::open(&path) {
                Err(VortexError::Refused { .. }) => {}
                Err(other) => panic!("{name} {pattern}: another error: {other}"),
                Ok(_) => panic!("{name} {pattern}: read"),
            }
            fs::remove_file(&side).unwrap();
        }
        for pattern in NOT_APPLIED {
            let side = dir.join(sidecar(pattern, name, stem));
            fs::write(&side, b"x").unwrap();
            let read = RasterFile::open(&path).unwrap_or_else(|e| panic!("{name} {pattern}: {e}"));
            assert_eq!(read.frame().cells(), plain.frame().cells());
            fs::remove_file(&side).unwrap();
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_sidecar_that_would_apply_to_the_written_file_refuses_the_write_and_changes_nothing() {
    let dir = scratch("write_side");
    let original = b"ORIGINAL".to_vec();
    for (name, stem) in [("a.tif", "a"), ("b.asc", "b")] {
        let path = dir.join(name);
        fs::write(&path, &original).unwrap();
        // A stale overview would be offered with the new file. The rest are the read's list.
        for pattern in APPLIED.iter().chain(&["{name}.ovr", "{name}.OVR"]) {
            let side = dir.join(sidecar(pattern, name, stem));
            fs::write(&side, b"side").unwrap();
            let before = names(&dir);
            let result = if name.ends_with("tif") {
                write_geotiff(&frame(Crs::Undefined), &path, &TiffOptions::default())
            } else {
                write_asc(&frame(Crs::Undefined), &path, &AscOptions::default())
            };
            assert!(
                matches!(result, Err(VortexError::Refused { .. })),
                "{name} {pattern}: {result:?}"
            );
            assert_eq!(fs::read(&path).unwrap(), original, "{name} {pattern}");
            assert_eq!(names(&dir), before, "{name} {pattern}");
            fs::remove_file(&side).unwrap();
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_at_the_prj_path_leaves_the_grid_alone() {
    // The review's reproduction: a normal write hit an I/O error after deleting the old grid.
    let dir = scratch("prj_dir");
    let path = dir.join("failure.asc");
    fs::write(&path, b"ORIGINAL GRID").unwrap();
    fs::create_dir(dir.join("failure.prj")).unwrap();
    let wkt = dss_vortex::crs::CrsDef::sphere_lonlat(6371000.0).to_wkt();
    let result = write_asc(&frame(Crs::Wkt(wkt)), &path, &AscOptions::default());
    assert!(result.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"ORIGINAL GRID");
    assert!(dir.join("failure.prj").is_dir());
    assert_eq!(names(&dir), ["failure.asc", "failure.prj"]);
    let _ = fs::remove_dir_all(&dir);
}

const BAD_WKT: &[&str] = &[
    // Passes every element check above these; GDAL 3.12.2 reads no CRS from either.
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563],TOWGS84[1,2]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433],AXIS[\"x\",UP]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433],BOGUS[\"y\"]]",
    "GEOGCS[\"bogus\"]",
    "GEOGCS[]",
    "PROJCS[\"x\"]",
    "GEOGCS[\"x\",DATUM[\"d\"],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",0,298]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,nope]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],UNIT[\"degree\",0.0174532925199433]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0]]",
    "GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0]]",
    "PROJCS[\"x\",GEOGCS[\"bogus\"],PROJECTION[\"Albers\"],UNIT[\"metre\",1]]",
    "PROJCS[\"x\",GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],UNIT[\"metre\",1]]",
    "PROJCS[\"x\",GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Albers\"]]",
    "PROJCS[\"x\",GEOGCS[\"x\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"a\",oops],UNIT[\"metre\",1]]",
];

#[test]
fn bracket_balanced_text_is_not_a_crs() {
    let dir = scratch("bad_wkt");
    for (i, text) in BAD_WKT.iter().enumerate() {
        let asc = dir.join(format!("g{i}.asc"));
        fs::write(&asc, GRID).unwrap();
        fs::write(asc.with_extension("prj"), text).unwrap();
        assert!(RasterFile::open(&asc).is_err(), "read: {text}");
        let file = RasterFile::from_bytes(
            "g.asc",
            GRID.as_bytes(),
            Some(text.as_bytes()),
            ReadOptions {
                unknown_crs: UnknownCrs::Undefined,
                ..ReadOptions::default()
            },
        )
        .unwrap();
        assert_eq!(file.frame().parts().crs, Crs::Undefined);

        let crs = Crs::Wkt(text.to_string());
        let out = dir.join(format!("w{i}.asc"));
        assert!(
            write_asc(&frame(crs.clone()), &out, &AscOptions::default()).is_err(),
            "asc: {text}"
        );
        assert!(!out.exists());
        let out = dir.join(format!("w{i}.tif"));
        assert!(
            write_geotiff(&frame(crs), &out, &TiffOptions::default()).is_err(),
            "tif: {text}"
        );
        assert!(!out.exists());
        assert!(
            read(&[(MODEL, V::S(USER)), (GT_CITATION, pe(text))]).is_err(),
            "citation: {text}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_lzw_chunk_must_end_at_its_end_code() {
    let cells = cell_bytes();
    // TIFF's LZW: MSB first, code size 8, early change.
    let stream = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
        .encode(&cells)
        .unwrap();
    let open = |bytes: &[u8]| {
        RasterFile::from_bytes(
            "a.tif",
            &assemble(5, bytes, &[]),
            None,
            ReadOptions {
                unknown_crs: UnknownCrs::Undefined,
                ..ReadOptions::default()
            },
        )
    };
    assert_eq!(
        open(&stream).unwrap().frame().cells(),
        [1.5f32, 2.5, 3.5, 4.5]
    );
    let mut padded = stream.clone();
    padded.push(0);
    assert!(open(&padded).is_ok(), "a zero pad byte after the end code");
    let mut no_end = stream.clone();
    no_end.pop();
    assert!(open(&no_end).is_err(), "no end code");
    // weezl reads ahead by a word, so a few bytes after the end code do not reach this check. They
    // cannot change a cell.
    let mut trailing = stream.clone();
    trailing.extend_from_slice(&[0xAB; 32]);
    assert!(open(&trailing).is_err(), "bytes after the end code");
}

#[test]
fn signed_byte_metadata_is_refused_where_the_gdal_versions_would_differ() {
    let meta = b"<GDALMetadata><Item name=\"PIXELTYPE\" domain=\"IMAGE_STRUCTURE\">SIGNEDBYTE</Item></GDALMetadata>\0".to_vec();
    let mut bytes = assemble(
        1,
        &cell_bytes()[..4],
        &[(42112, 2, meta.len() as u32, meta)],
    );
    // The same file as uint8, which the two bytes per row of a 2 x 2 grid fill.
    let entry = |tag: u16, bytes: &[u8]| {
        (0..u16::from_le_bytes([bytes[8], bytes[9]]) as usize)
            .map(|i| 10 + i * 12)
            .find(|e| u16::from_le_bytes([bytes[*e], bytes[e + 1]]) == tag)
            .unwrap()
    };
    let bits = entry(258, &bytes);
    bytes[bits + 8..bits + 10].copy_from_slice(&8u16.to_le_bytes());
    let format = entry(339, &bytes);
    bytes[format + 8..format + 10].copy_from_slice(&1u16.to_le_bytes());
    let error = RasterFile::from_bytes("a.tif", &bytes, None, ReadOptions::default())
        .expect_err("read")
        .to_string();
    assert!(error.contains("sample type"), "{error}");
}

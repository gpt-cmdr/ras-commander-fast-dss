//! Every committed raster against rasterio's recorded reading, on any platform, with no Python.
//! `oracle/vortex/compare_raster.py` is the live gate; `make_raster_fixtures.py` writes the record.

use dss_vortex::crs::CrsDef;
use dss_vortex::source::raster::{CrsStatus, RasterFile, ReadOptions, UnknownCrs};
use dss_vortex::{Crs, FrameTime, VortexError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/raster")
}

fn expected() -> BTreeMap<String, Value> {
    serde_json::from_str(&fs::read_to_string(dir().join("expected.json")).unwrap()).unwrap()
}

fn fnv1a(cells: &[f32]) -> String {
    let mut value: u64 = 0xCBF29CE484222325;
    for cell in cells {
        for byte in cell.to_bits().to_le_bytes() {
            value = (value ^ u64::from(byte)).wrapping_mul(0x100000001B3);
        }
    }
    format!("{value:016x}")
}

fn bits(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

#[test]
fn every_fixture_reads_as_recorded() {
    let all = expected();
    assert!(all.len() > 80, "only {} fixtures", all.len());
    let (mut read, mut refused) = (0, 0);
    for (name, want) in &all {
        let result = RasterFile::open(&dir().join(name));
        if let Some(reason) = want.get("refused") {
            let error = result.expect_err(name).to_string();
            assert!(
                error.contains(reason.as_str().unwrap()),
                "{name}: {error}, expected {reason}"
            );
            refused += 1;
            continue;
        }
        let file = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        let parts = file.frame().parts();
        assert_eq!(parts.nx as u64, want["nx"].as_u64().unwrap(), "{name}");
        assert_eq!(parts.ny as u64, want["ny"].as_u64().unwrap(), "{name}");
        assert_eq!(fnv1a(&parts.cells), want["cells_fnv"], "{name}");
        let t = parts.transform;
        let transform = [
            t.origin_x,
            t.dx,
            t.row_rotation,
            t.origin_y,
            t.column_rotation,
            t.dy,
        ]
        .map(bits);
        assert_eq!(
            Value::from(transform.to_vec()),
            want["transform"],
            "{name}: transform"
        );
        match (parts.nodata, want["nodata"].as_str()) {
            (None, None) => {}
            (Some(n), Some("nan")) => assert!(n.is_nan(), "{name}"),
            (Some(n), Some(text)) => assert_eq!(format!("{:08x}", n.to_bits()), text, "{name}"),
            other => panic!("{name}: nodata {other:?}"),
        }
        let (kind, wkt) = match &parts.crs {
            Crs::Undefined => ("undefined", String::new()),
            Crs::Wkt(text) => ("wkt", text.clone()),
            Crs::Proj(text) => ("proj", text.clone()),
            Crs::Named(_) => ("named", CrsDef::from_frame(&parts.crs).unwrap().to_wkt()),
        };
        assert_eq!(kind, want["crs_kind"], "{name}");
        assert_eq!(wkt, want["wkt"], "{name}");
        let time = match parts.time {
            FrameTime::Invariant => Value::Null,
            FrameTime::Instant(t) => serde_json::json!([t.0, t.1]),
            FrameTime::Period { start, end } => {
                serde_json::json!([start.0, start.1, end.0, end.1])
            }
        };
        assert_eq!(time, want["time"], "{name}: time");
        assert_eq!(
            format!("{:?}", parts.data_type),
            want["data_type"],
            "{name}"
        );
        assert_eq!(parts.units, want["units"], "{name}");
        assert_eq!(parts.variable, want["variable"], "{name}");
        assert_eq!(
            parts.description.as_deref().unwrap_or(""),
            want["description"],
            "{name}"
        );
        read += 1;
    }
    assert!(read > 50 && refused > 20, "{read} read, {refused} refused");
}

#[test]
fn vortex_written_trinity_has_what_rasterio_says() {
    let file = RasterFile::open(&dir().join("TrinityXMRG2DSS.tiff")).unwrap();
    let parts = file.frame().parts();
    assert_eq!((parts.nx, parts.ny), (1198, 1086));
    assert_eq!(parts.transform.origin_x, -1680000.0);
    assert_eq!(parts.transform.origin_y, 2274000.0);
    assert_eq!(parts.transform.dx, 2000.0);
    assert_eq!(parts.transform.dy, -2000.0);
    assert!(parts.nodata.is_some_and(f32::is_nan));
    assert_eq!(parts.crs, Crs::Named(dss_vortex::NamedCrs::Shg));
    let wet = parts.cells.iter().filter(|c| **c > 0.0).count();
    assert_eq!(wet, 21056, "the fixture is wet; rasterio counts this many");
}

/// EPSG UTM codes are read by the codes' own rule (P53b): `crs_utm.tif` states EPSG:26915, and
/// `every_fixture_reads_as_recorded` holds its cells and CRS to what rasterio reads.
#[test]
fn an_epsg_utm_code_reads_as_that_zone() {
    let file = RasterFile::open(&dir().join("crs_utm.tif")).unwrap();
    let def = CrsDef::from_frame(&file.frame().parts().crs).unwrap();
    assert_eq!(def.utm_zone(), Some((15, false)));
    assert_eq!(def.datum, dss_vortex::crs::Datum::Nad83);
    assert_eq!(
        Some(def),
        CrsDef::utm(
            dss_vortex::crs::Datum::Nad83,
            6378137.0,
            298.257222101,
            15,
            false
        )
    );
}

#[test]
fn an_unknown_crs_is_refused_or_explicitly_undefined_never_guessed() {
    for name in [
        "crs_web_mercator.tif",
        "crs_nad27.tif",
        "crs_feet.tif",
        "asc_bad_prj.asc",
    ] {
        let path = dir().join(name);
        let error = RasterFile::open(&path).unwrap_err();
        assert!(
            matches!(error, VortexError::Refused { .. }),
            "{name}: {error}"
        );
        let options = ReadOptions {
            unknown_crs: UnknownCrs::Undefined,
            ..ReadOptions::default()
        };
        let file = RasterFile::open_with(&path, options).unwrap();
        assert_eq!(file.frame().parts().crs, Crs::Undefined, "{name}");
        assert!(
            matches!(file.crs_status(), CrsStatus::Unresolved { .. }),
            "{name}: {:?}",
            file.crs_status()
        );
    }
}

#[test]
fn a_file_with_no_crs_says_so() {
    let file = RasterFile::open(&dir().join("crs_none.tif")).unwrap();
    assert_eq!(file.frame().parts().crs, Crs::Undefined);
    assert_eq!(file.crs_status(), &CrsStatus::NotInFile);
    let file = RasterFile::open(&dir().join("asc_int.asc")).unwrap();
    assert_eq!(file.frame().parts().crs, Crs::Undefined);
    assert_eq!(file.crs_status(), &CrsStatus::NotInFile);
}

#[test]
fn an_aux_xml_sidecar_is_refused() {
    let temp = std::env::temp_dir().join(format!("p50_aux_{}", std::process::id()));
    fs::create_dir_all(&temp).unwrap();
    let path = temp.join("a.tif");
    fs::copy(dir().join("strip_none.tif"), &path).unwrap();
    assert!(RasterFile::open(&path).is_ok());
    fs::write(temp.join("a.tif.aux.xml"), "<PAMDataset/>").unwrap();
    let error = RasterFile::open(&path).unwrap_err().to_string();
    assert!(error.contains(".aux.xml"), "{error}");
    fs::remove_dir_all(&temp).unwrap();
}

#[test]
fn bytes_and_a_path_read_the_same() {
    let path = dir().join("tile_deflate.tif");
    let by_path = RasterFile::open(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let by_bytes =
        RasterFile::from_bytes("tile_deflate.tif", &bytes, None, ReadOptions::default()).unwrap();
    assert_eq!(by_path.frame(), by_bytes.frame());
}

#[test]
fn a_tiff_named_asc_and_text_named_tif_are_refused() {
    let tiff = fs::read(dir().join("strip_none.tif")).unwrap();
    assert!(RasterFile::from_bytes("a.asc", &tiff, None, ReadOptions::default()).is_err());
    let asc = fs::read(dir().join("asc_float.asc")).unwrap();
    assert!(RasterFile::from_bytes("a.tif", &asc, None, ReadOptions::default()).is_err());
}

#[test]
fn the_limits_are_the_grib_readers() {
    let raster = dss_vortex::source::raster::Limits::default();
    let grib = dss_vortex::source::grib::Limits::default();
    assert_eq!(raster.max_cells, grib.max_cells);
    assert_eq!(raster.max_input_bytes, grib.max_input_bytes);
}

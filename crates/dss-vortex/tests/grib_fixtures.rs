//! Synthetic GRIB2 fixtures against Vortex 1.1.0's own decoded input, recorded in
//! `fixtures/public/grib/expected.json` and `*.f32` by `oracle/vortex/make_grib_fixtures.py`.
//! Bits are compared, so this is also the cross-platform determinism check for the decode layer.

use dss_vortex::source::grib::{Grib2File, Limits};
use dss_vortex::{Crs, DataType, FrameTime, VortexError};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib")
}

fn expected() -> Value {
    serde_json::from_str(&fs::read_to_string(dir().join("expected.json")).unwrap()).unwrap()
}

fn open(name: &str) -> Grib2File {
    Grib2File::open(&dir().join(name)).unwrap()
}

fn read_f32(path: &Path) -> Vec<u32> {
    let bytes = fs::read(path).unwrap();
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks.iter().map(|b| u32::from_le_bytes(*b)).collect()
}

fn assert_same_cells(name: &str, reference: &[u32], cells: &[f32]) {
    assert_eq!(reference.len(), cells.len(), "{name}");
    for (i, (&a, &b)) in reference.iter().zip(cells).enumerate() {
        let a_nan = f32::from_bits(a).is_nan();
        assert_eq!(a_nan, b.is_nan(), "{name} mask at {i}");
        if !a_nan {
            assert_eq!(a, b.to_bits(), "{name} bits at {i}");
        }
    }
}

fn java_time(seconds: i64) -> String {
    let (y, m, d) = dss_core::time::julian_to_ymd(seconds.div_euclid(86_400));
    let s = seconds.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}Z", s / 3600, s / 60 % 60)
}

#[test]
fn every_fixture_matches_vortex_exactly() {
    let expected = expected();
    let cases = expected["exact"].as_object().unwrap();
    assert!(cases.len() >= 13, "only {} fixtures", cases.len());
    for (name, case) in cases {
        let file = open(name);
        let reference = read_f32(&dir().join(format!("{name}.f32")));
        assert_same_cells(name, &reference, &file.decode_record(0).unwrap());

        let variable = case["shortName"].as_str().unwrap();
        let frame = file.frame(variable, 0).unwrap();
        assert_same_cells(name, &reference, frame.cells());
        let p = frame.parts();
        assert_eq!(p.nx as u64, case["nx"].as_u64().unwrap(), "{name}");
        assert_eq!(p.ny as u64, case["ny"].as_u64().unwrap(), "{name}");
        for (key, value) in [
            ("originX", p.transform.origin_x),
            ("originY", p.transform.origin_y),
            ("dx", p.transform.dx),
            ("dy", p.transform.dy),
        ] {
            let want: f64 = case[key].as_str().unwrap().parse().unwrap();
            assert_eq!(want.to_bits(), value.to_bits(), "{name} {key}");
        }
        match &p.crs {
            Crs::Wkt(wkt) => assert_eq!(wkt, case["wkt"].as_str().unwrap(), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(p.units, case["units"].as_str().unwrap(), "{name}");
        assert_eq!(
            p.description.as_deref(),
            case["description"].as_str(),
            "{name}"
        );
        let FrameTime::Instant(t) = p.time else {
            panic!("{name}: {:?}", p.time);
        };
        assert_eq!(
            java_time(t.0 * 86_400 + t.1),
            case["startTime"].as_str().unwrap(),
            "{name}"
        );
        assert_eq!(p.data_type, DataType::InstVal, "{name}");
    }
}

#[test]
fn ccsds_is_refused_as_vortex_refuses_it() {
    let file = open("ccsds.grib2");
    match file.decode_record(0) {
        Err(VortexError::Refused { what, .. }) => assert!(what.contains("5.42")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_jpeg2000_stream_vortex_would_truncate_is_refused() {
    for name in ["noise_b8.grib2", "jpeg_bitmap.grib2"] {
        let file = open(name);
        match file.decode_record(0) {
            Err(VortexError::Refused { reason, .. }) => {
                assert!(reason.contains("truncates"), "{reason}")
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn grib1_is_refused() {
    let mut bytes = b"GRIB\0\0\0\x01".to_vec();
    bytes.extend_from_slice(&[0; 32]);
    match Grib2File::from_bytes("x.grb", bytes, Limits::default()) {
        Err(VortexError::Refused { what, .. }) => assert_eq!(what, "GRIB1"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn value_code_calls_no_platform_maths() {
    const FORBIDDEN: &[&str] = &[
        ".sin(",
        ".cos(",
        ".tan(",
        ".asin(",
        ".acos(",
        ".atan(",
        ".atan2(",
        ".sinh(",
        ".cosh(",
        ".tanh(",
        ".exp(",
        ".exp2(",
        ".exp_m1(",
        ".ln(",
        ".ln_1p(",
        ".log(",
        ".log2(",
        ".log10(",
        ".powf(",
        ".powi(",
        ".hypot(",
        ".cbrt(",
        ".sin_cos(",
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![root.join("source"), root.join("products")];
    let mut checked = 0;
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).unwrap();
            for word in FORBIDDEN {
                assert!(!text.contains(word), "{} calls {word}", path.display());
            }
            checked += 1;
        }
    }
    assert!(checked >= 7, "only {checked} files");
}

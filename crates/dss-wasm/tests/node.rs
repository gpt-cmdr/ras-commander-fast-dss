//! P35 Node tests, run inside `wasm32-unknown-unknown`:
//!
//! ```text
//! wasm-pack test --node crates/dss-wasm
//! ```
//!
//! The read layer's digests inside WASM must equal the table recorded on the host
//! (`common/expected.rs`), and the JavaScript objects the exports return must carry
//! exactly the values `dss-core` reads from the same bytes.
#![cfg(target_arch = "wasm32")]

mod common;

use common::{dataset_key, digest, FIXTURES};
use dss_core::{GridReadOptions, TsKind, TsValues};
use dss_wasm::reader::{lossy, DssBuffer};
use dss_wasm::{dss_version, file_signature, is_hec_missing, DssFile};
use js_sys::{Array, Float32Array, Float64Array, Int32Array, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;

fn get(o: &JsValue, k: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(k)).expect("property")
}

fn num(o: &JsValue, k: &str) -> f64 {
    get(o, k)
        .as_f64()
        .unwrap_or_else(|| panic!("{k} is not a number"))
}

fn text(o: &JsValue, k: &str) -> String {
    get(o, k)
        .as_string()
        .unwrap_or_else(|| panic!("{k} is not a string"))
}

fn f64s(o: &JsValue, k: &str) -> Vec<f64> {
    get(o, k).dyn_into::<Float64Array>().expect(k).to_vec()
}

fn strings(o: &JsValue, k: &str) -> Vec<String> {
    get(o, k)
        .dyn_into::<Array>()
        .expect(k)
        .iter()
        .map(|v| v.as_string().expect("string"))
        .collect()
}

/// Typed array of the stored width against `TsValues`, bit for bit.
fn same_values(js: JsValue, want: &TsValues) -> bool {
    match want {
        TsValues::F32(x) => {
            let got = js
                .dyn_into::<Float32Array>()
                .expect("Float32Array")
                .to_vec();
            got.len() == x.len() && got.iter().zip(x).all(|(a, b)| a.to_bits() == b.to_bits())
        }
        TsValues::F64(x) => {
            let got = js
                .dyn_into::<Float64Array>()
                .expect("Float64Array")
                .to_vec();
            got.len() == x.len() && got.iter().zip(x).all(|(a, b)| a.to_bits() == b.to_bits())
        }
    }
}

fn kind_of(e: &JsValue) -> (String, String) {
    assert!(e.is_instance_of::<js_sys::Error>(), "not an Error: {e:?}");
    (text(e, "name"), text(e, "kind"))
}

#[wasm_bindgen_test]
fn digests_inside_wasm_equal_host_table() {
    for (name, bytes) in FIXTURES {
        let d = digest(bytes);
        let want = common::expected::EXPECTED
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| *d);
        assert_eq!(Some(d), want, "{name}");
    }
}

#[wasm_bindgen_test]
fn signature_exports() {
    for (name, bytes) in FIXTURES {
        let mut b = DssBuffer::new(bytes.to_vec()).unwrap();
        let v = dss_version(bytes).unwrap();
        assert_eq!(v, b.version(), "{name}");
        let s: JsValue = file_signature(bytes).unwrap().into();
        assert_eq!(num(&s, "version") as u8, v);
        assert_eq!(text(&s, "versionString"), b.signature().version_string);
        assert_eq!(text(&s, "byteOrder"), "little");
        assert_eq!(
            num(&s, "numberRecords") as i64,
            b.signature().number_records
        );
        let f = DssFile::new(bytes).unwrap();
        assert_eq!(f.version(), v);
        assert_eq!(f.version_string(), b.signature().version_string);
        // Catalog from the class equals the read layer's.
        let mut f = f;
        let c: JsValue = f.catalog().unwrap().into();
        let want = b.catalog().unwrap();
        assert_eq!(strings(&c, "pathname"), want.pathname);
        assert_eq!(strings(&c, "storedPathname"), want.stored_pathname);
        let rt = get(&c, "recordType")
            .dyn_into::<Int32Array>()
            .unwrap()
            .to_vec();
        assert_eq!(rt, want.record_type);
        assert_eq!(strings(&c, "status"), want.status);
        let lw: Vec<i64> = f64s(&c, "lastWriteMs").iter().map(|&x| x as i64).collect();
        assert_eq!(lw, want.last_write_ms);
    }
    assert!(is_hec_missing(-901.0) && is_hec_missing(-902.0));
    assert!(is_hec_missing(-(f32::MAX as f64)));
    assert!(!is_hec_missing(f64::NAN) && !is_hec_missing(0.0));
}

/// Every live record of every fixture through the exported class, against the read
/// layer (`dss-core`) on the same bytes.
#[wasm_bindgen_test]
fn js_objects_carry_dss_core_values() {
    let (mut nts, mut nds, mut npd, mut ngrid) = (0, 0, 0, 0);
    for (name, bytes) in FIXTURES {
        let mut f = DssFile::new(bytes).unwrap();
        let mut b = DssBuffer::new(bytes.to_vec()).unwrap();
        let cat = b.catalog().unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..cat.len() {
            if !matches!(cat.status[i], "live" | "alias") || cat.duplicate[i] {
                continue;
            }
            let p = cat.stored_pathname[i].clone();
            match cat.record_type[i] {
                100..=199 => {
                    let mut pairs = Vec::new();
                    pairs.push((f.read_record(&p).map(JsValue::from), b.read_record(&p)));
                    if dataset_key(&p).is_some_and(|k| seen.insert(k)) {
                        pairs.push((
                            f.read_dataset(&p, None, None, None, None, None)
                                .map(JsValue::from),
                            b.read_dataset(&p, None, true),
                        ));
                    }
                    for (k, (js, want)) in pairs.into_iter().enumerate() {
                        let (js, want) = match (js, want) {
                            (Ok(j), Ok(w)) => (j, w),
                            (Err(e), Err(w)) => {
                                assert_eq!(kind_of(&e).1, w.kind.as_str(), "{name} {p}");
                                continue;
                            }
                            _ => panic!("{name} {p}: export and read layer disagree"),
                        };
                        if k == 0 {
                            nts += 1;
                        } else {
                            nds += 1;
                        }
                        let ts = &want.ts;
                        assert_eq!(text(&js, "pathname"), lossy(&ts.pathname));
                        assert_eq!(num(&js, "recordType") as i32, ts.record_type);
                        let times: Vec<i64> =
                            f64s(&js, "times").iter().map(|&t| t as i64).collect();
                        assert_eq!(times, ts.times, "{name} {p} times");
                        assert!(same_values(get(&js, "values"), &ts.values), "{name} {p}");
                        match ts.kind {
                            TsKind::Regular { interval_seconds } => {
                                assert_eq!(get(&js, "regular"), JsValue::TRUE);
                                assert_eq!(num(&js, "intervalSeconds") as i64, interval_seconds);
                            }
                            TsKind::Irregular { .. } => {
                                assert_eq!(get(&js, "regular"), JsValue::FALSE);
                                assert!(get(&js, "intervalSeconds").is_null());
                                assert!(text(&js, "irregularBlock").starts_with("IR-"));
                            }
                        }
                        match &ts.quality {
                            Some(q) => {
                                let got = get(&js, "quality").dyn_into::<Int32Array>().unwrap();
                                assert_eq!(&got.to_vec(), q);
                            }
                            None => assert!(get(&js, "quality").is_null()),
                        }
                        assert_eq!(text(&js, "units"), ts.units);
                        assert_eq!(text(&js, "dataType"), ts.data_type);
                        assert_eq!(
                            num(&js, "timeOffsetSeconds") as i64,
                            want.time_offset_seconds
                        );
                        assert_eq!(num(&js, "blocksFound") as usize, want.blocks_found);
                    }
                }
                200..=299 => match (f.read_paired_data(&p), b.read_paired_data(&p)) {
                    (Ok(js), Ok(want)) => {
                        let js = JsValue::from(js);
                        let pd = &want.pd;
                        assert_eq!(num(&js, "numberOrdinates") as usize, pd.number_ordinates);
                        assert_eq!(num(&js, "numberCurves") as usize, pd.number_curves);
                        assert!(same_values(get(&js, "ordinates"), &pd.ordinates));
                        assert!(same_values(get(&js, "values"), &pd.values), "{name} {p}");
                        assert_eq!(strings(&js, "labels"), pd.labels);
                        assert_eq!(text(&js, "unitsIndependent"), pd.units_independent);
                        assert_eq!(text(&js, "typeDependent"), pd.type_dependent);
                        npd += 1;
                    }
                    (Err(e), Err(w)) => assert_eq!(kind_of(&e).1, w.kind.as_str()),
                    _ => panic!("{name} {p}: export and read layer disagree"),
                },
                400..=499 => {
                    let o = GridReadOptions::default();
                    match (f.read_grid(&p, None, None), b.read_grid(&p, &o)) {
                        (Ok(js), Ok(want)) => {
                            let js = JsValue::from(js);
                            let g = &want.grid;
                            assert_eq!(num(&js, "nx") as usize, g.nx);
                            assert_eq!(num(&js, "ny") as usize, g.ny);
                            let cells = get(&js, "data").dyn_into::<Float32Array>().unwrap();
                            let cells = cells.to_vec();
                            assert_eq!(cells.len(), g.nx * g.ny);
                            assert!(
                                cells
                                    .iter()
                                    .zip(&g.data)
                                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                                "{name} {p} cells"
                            );
                            assert_eq!(text(&js, "srsDefinition"), g.srs_definition);
                            assert_eq!(text(&js, "dataUnits"), g.data_units);
                            assert_eq!(num(&js, "nullValue") as f32, g.null_value);
                            assert_eq!(get(&js, "albers").is_null(), g.albers.is_none());
                            if let Some(a) = &g.albers {
                                let ja = get(&js, "albers");
                                assert_eq!(num(&ja, "centralMeridian") as f32, a.central_meridian);
                            }
                            match g.start_time {
                                Some(t) => assert_eq!(num(&js, "startTime") as i64, t),
                                None => assert!(get(&js, "startTime").is_null()),
                            }
                            ngrid += 1;
                        }
                        (Err(e), Err(w)) => assert_eq!(kind_of(&e).1, w.kind.as_str()),
                        _ => panic!("{name} {p}: export and read layer disagree"),
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        nts >= 80 && nds >= 50 && npd >= 30 && ngrid >= 50,
        "{nts} {nds} {npd} {ngrid}"
    );
}

#[wasm_bindgen_test]
fn errors_are_typed_js_errors() {
    let e = DssFile::new(b"not a dss file").err().unwrap();
    assert_eq!(kind_of(&e), ("DssFormatError".into(), "format".into()));
    let e = dss_version(&[]).err().unwrap();
    assert_eq!(kind_of(&e).1, "format");

    let (_, bytes) = FIXTURES
        .iter()
        .find(|(n, _)| *n == "synthetic_pd7.dss")
        .unwrap();
    let mut f = DssFile::new(bytes).unwrap();
    let e = f
        .read_record("/NO/SUCH/RECORD/01JAN2000/1HOUR/X/")
        .err()
        .unwrap();
    assert_eq!(
        kind_of(&e),
        ("DssPathnameNotFoundError".into(), "not_found".into())
    );
    let e = f.read_record("/BASIN/LOC/STAGE-DAMAGE///F/").err().unwrap();
    assert_eq!(
        kind_of(&e),
        ("DssUnsupportedRecordError".into(), "unsupported".into())
    );
    let e = f
        .read_dataset("/A/B/FLOW//1HOUR/X/", Some(0.0), None, None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");
    let e = f
        .read_dataset(
            "/A/B/FLOW//1HOUR/X/",
            Some(0.5),
            Some(10.0),
            None,
            None,
            None,
        )
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");

    let (_, bytes) = FIXTURES
        .iter()
        .find(|(n, _)| *n == "real_grid7.dss")
        .unwrap();
    let mut f = DssFile::new(bytes).unwrap();
    let mut b = DssBuffer::new(bytes.to_vec()).unwrap();
    let c = b.catalog().unwrap();
    let e = f
        .read_grid(&c.stored_pathname[0], Some(1.0), None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e), ("DssError".into(), "dss".into()));
}

/// P35 review: `maxCells` and `maxBlocks` never wrap; bad values are `invalid_argument`.
#[wasm_bindgen_test]
fn count_arguments_are_validated() {
    let (_, bytes) = FIXTURES
        .iter()
        .find(|(n, _)| *n == "real_grid7.dss")
        .unwrap();
    let mut f = DssFile::new(bytes).unwrap();
    let mut b = DssBuffer::new(bytes.to_vec()).unwrap();
    let p = b.catalog().unwrap().stored_pathname[0].clone();
    for bad in [
        -1.0,
        0.0,
        1.5,
        4_294_967_296.0,
        4_294_967_295.0,
        f64::NAN,
        f64::INFINITY,
    ] {
        let e = f.read_grid(&p, Some(bad), None).err().unwrap();
        assert_eq!(kind_of(&e).1, "invalid_argument", "maxCells {bad}");
    }
    // The largest accepted value reads, and the object stays usable after refusals.
    assert!(f
        .read_grid(&p, Some(dss_wasm::MAX_CELLS_ARG as f64), None)
        .is_ok());
    assert!(f.read_grid(&p, None, None).is_ok());

    let (_, bytes) = FIXTURES
        .iter()
        .find(|(n, _)| *n == "p21_hec_its7.dss")
        .unwrap();
    let mut f = DssFile::new(bytes).unwrap();
    let mut b = DssBuffer::new(bytes.to_vec()).unwrap();
    let c = b.catalog().unwrap();
    let p = c
        .stored_pathname
        .iter()
        .find(|p| p.to_ascii_uppercase().contains("/IR-DAY/"))
        .unwrap()
        .clone();
    // 2^40 seconds is about 35,000 years of daily blocks: refused before any read.
    let e = f
        .read_dataset(&p, Some(0.0), Some(1_099_511_627_776.0), None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");
    assert!(text(&e, "message").contains("maxBlocks"));
    for bad in [-1.0, 0.0, 2.5, f64::NAN] {
        let e = f
            .read_dataset(&p, Some(0.0), Some(86_400.0), None, Some(bad), None)
            .err()
            .unwrap();
        assert_eq!(kind_of(&e).1, "invalid_argument", "maxBlocks {bad}");
    }
    // A whole-dataset window: refused under an explicit 1-block limit, read by default.
    let whole = JsValue::from(f.read_dataset(&p, None, None, None, None, None).unwrap());
    let t = f64s(&whole, "times");
    let (s, e) = (t[0], t[t.len() - 1]);
    assert!(f
        .read_dataset(&p, Some(s), Some(e), Some(false), Some(1.0), None)
        .is_err());
    assert!(f
        .read_dataset(&p, Some(s), Some(e), Some(false), None, None)
        .is_ok());
}

//! P40 Node tests, run inside `wasm32-unknown-unknown` (`wasm-pack test --node
//! crates/dss-wasm`): the digests of the new routes equal the host's table, and the
//! JavaScript objects carry exactly what the read layer returns.
#![cfg(target_arch = "wasm32")]

mod common;

use common::routes::{open_mode, route_digest, MAX_BLOCKS, MAX_CELLS, ROUTE_FIXTURES};
use dss_core::TsValues;
use dss_wasm::reader::{lossy, DssBuffer};
use dss_wasm::DssFile;
use js_sys::{Array, Float32Array, Float64Array, Int32Array, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;

fn get(o: &JsValue, k: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(k)).expect("property")
}

fn num(o: &JsValue, k: &str) -> f64 {
    get(o, k).as_f64().unwrap_or_else(|| panic!("{k}"))
}

fn text(o: &JsValue, k: &str) -> String {
    get(o, k).as_string().unwrap_or_else(|| panic!("{k}"))
}

fn flag(o: &JsValue, k: &str) -> bool {
    get(o, k).as_bool().unwrap_or_else(|| panic!("{k}"))
}

fn ints(o: &JsValue, k: &str) -> Vec<i32> {
    get(o, k).dyn_into::<Int32Array>().expect(k).to_vec()
}

fn same_values(js: JsValue, want: &TsValues) -> bool {
    match want {
        TsValues::F32(x) => {
            let got = js.dyn_into::<Float32Array>().expect("f32").to_vec();
            got.len() == x.len() && got.iter().zip(x).all(|(a, b)| a.to_bits() == b.to_bits())
        }
        TsValues::F64(x) => {
            let got = js.dyn_into::<Float64Array>().expect("f64").to_vec();
            got.len() == x.len() && got.iter().zip(x).all(|(a, b)| a.to_bits() == b.to_bits())
        }
    }
}

fn kind_of(e: &JsValue) -> (String, String) {
    assert!(e.is_instance_of::<js_sys::Error>(), "not an Error: {e:?}");
    (text(e, "name"), text(e, "kind"))
}

fn open_js(bytes: &[u8], recover: bool) -> DssFile {
    if recover {
        DssFile::open_recovery(bytes).unwrap()
    } else {
        DssFile::new(bytes).unwrap()
    }
}

fn fixture(name: &str) -> &'static [u8] {
    ROUTE_FIXTURES.iter().find(|(n, _)| *n == name).unwrap().1
}

#[wasm_bindgen_test]
fn route_digests_inside_wasm_equal_host_table() {
    for (name, bytes) in ROUTE_FIXTURES {
        for recover in [false, true] {
            let d = route_digest(bytes, recover);
            let want = common::expected_routes::EXPECTED
                .iter()
                .find(|(n, r, _)| n == name && *r == recover)
                .map(|(_, _, d)| *d);
            assert_eq!(Some(d), want, "{name} recover={recover}");
        }
    }
}

fn check_profile(js: &JsValue, want: &dss_wasm::reader::ProfileData) {
    let p = &want.profile;
    assert_eq!(text(js, "pathname"), lossy(&p.pathname));
    assert_eq!(num(js, "recordType") as i32, p.record_type);
    assert_eq!(num(js, "numberTimes") as usize, p.times.len());
    assert_eq!(num(js, "numberDepths") as usize, p.depths.len());
    let t: Vec<i64> = get(js, "times")
        .dyn_into::<Float64Array>()
        .unwrap()
        .to_vec()
        .iter()
        .map(|&x| x as i64)
        .collect();
    assert_eq!(t, p.times);
    assert!(same_values(get(js, "depths"), &p.depths));
    assert!(same_values(get(js, "values"), &p.values));
    assert_eq!(p.values.len(), p.times.len() * p.depths.len());
    assert_eq!(text(js, "depthUnits"), p.depth_units);
    assert_eq!(text(js, "valueUnits"), p.value_units);
    assert_eq!(text(js, "dataType"), p.data_type);
    let m = &p.meta;
    assert_eq!(num(js, "precision") as i32, m.precision);
    assert_eq!(num(js, "intervalSeconds") as i64, m.interval.code);
    assert_eq!(num(js, "timeOffsetSeconds") as i64, m.time_offset_seconds);
    assert_eq!(num(js, "blocksFound") as usize, m.blocks_found);
    assert_eq!(
        num(js, "qualityElementSize") as usize,
        m.quality_element_size
    );
    match (&m.quality_words, m.quality_element_size) {
        (Some(w), q) if q > 0 => {
            assert_eq!(&ints(js, "qualityWords"), w);
            let first = ints(js, "quality");
            assert_eq!(first.len(), p.times.len());
            assert_eq!(first[0], w[0]);
        }
        _ => {
            assert!(get(js, "quality").is_null());
            assert!(get(js, "qualityWords").is_null());
        }
    }
    assert_eq!(ints(js, "userHeader"), m.user_header);
    assert_eq!(ints(js, "internalHeader"), m.internal_header.words);
}

fn check_location(js: &JsValue, want: &dss_wasm::reader::LocationData) {
    let l = &want.location;
    assert_eq!(text(js, "pathname"), lossy(&l.pathname));
    assert_eq!(num(js, "version") as u8, want.version);
    assert_eq!(num(js, "recordType") as i32, want.record_type);
    let c = get(js, "coordinates")
        .dyn_into::<Float64Array>()
        .unwrap()
        .to_vec();
    assert_eq!(
        c.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        [l.x, l.y, l.z].map(f64::to_bits).to_vec()
    );
    assert_eq!(num(js, "x"), l.x);
    assert_eq!(flag(js, "descriptorsStored"), l.descriptors_stored);
    assert_eq!(num(js, "coordinateSystem") as i32, l.coordinate_system_code);
    assert_eq!(num(js, "coordinateId") as i32, l.coordinate_id);
    assert_eq!(num(js, "horizontalUnits") as i32, l.horizontal_units_code);
    assert_eq!(num(js, "horizontalDatum") as i32, l.horizontal_datum_code);
    assert_eq!(num(js, "verticalUnits") as i32, l.vertical_units_code);
    assert_eq!(num(js, "verticalDatum") as i32, l.vertical_datum_code);
    match &l.time_zone_name {
        Some(z) => assert_eq!(&text(js, "timeZone"), z),
        None => assert!(get(js, "timeZone").is_null()),
    }
    match &l.supplemental {
        Some(b) => {
            let got = get(js, "supplemental").dyn_into::<Uint8Array>().unwrap();
            assert_eq!(&got.to_vec(), b);
            assert_eq!(text(js, "supplementalText"), l.supplemental_text().unwrap());
            assert_eq!(
                text(js, "supplementalEncoding"),
                l.supplemental_encoding().unwrap()
            );
        }
        None => {
            assert!(get(js, "supplemental").is_null());
            assert!(get(js, "supplementalText").is_null());
        }
    }
    match &want.values1 {
        Some(v) => assert_eq!(&ints(js, "values1"), v),
        None => assert!(get(js, "values1").is_null()),
    }
    match want.block_julian {
        Some(j) => assert_eq!(num(js, "blockJulian") as i64, j),
        None => assert!(get(js, "blockJulian").is_null()),
    }
}

/// Every route of every fixture through the exported class, against the read layer on the
/// same bytes, in both modes.
#[wasm_bindgen_test]
fn js_objects_carry_read_layer_values() {
    let (mut np, mut nl, mut na, mut nat, mut nerr) = (0, 0, 0, 0, 0);
    for (name, bytes) in ROUTE_FIXTURES {
        for recover in [false, true] {
            let mut f = open_js(bytes, recover);
            assert_eq!(f.recovery_mode(), recover);
            let mut b = open_mode(bytes, recover);
            let cat = b.catalog().unwrap();
            for i in 0..cat.len() {
                let pv = b.entry_provenance(i).unwrap();
                let js: JsValue = f.entry_provenance(i as f64).unwrap().into();
                assert_eq!(text(&js, "outcome"), pv.outcome(), "{name} row {i}");
                assert_eq!(text(&js, "hashChain"), pv.hash_chain);
                assert_eq!(text(&js, "status"), pv.status);
                assert_eq!(num(&js, "infoAddress") as u64, pv.info_address);
                assert_eq!(num(&js, "binEntryAddress") as u64, pv.bin_entry_address);
                assert_eq!(flag(&js, "recoverMode"), pv.recover_mode);
                assert_eq!(flag(&js, "reachableByLookup"), pv.reachable_by_lookup);
                match (f.read_record_at(i as f64), b.read_record_at(i)) {
                    (Ok(js), Ok((_, w))) => {
                        let js = JsValue::from(js);
                        let rec = get(&js, "record");
                        assert!(same_values(get(&rec, "values"), &w.ts.values), "{name} {i}");
                        assert_eq!(text(&get(&js, "provenance"), "outcome"), pv.outcome());
                        nat += 1;
                    }
                    (Err(e), Err(w)) => {
                        assert_eq!(
                            kind_of(&e),
                            (w.kind.error_name().to_string(), w.kind.as_str().to_string()),
                            "{name} row {i}"
                        );
                        if let Some(u) = &w.unavailable {
                            assert_eq!(text(&e, "problem"), u.problem);
                            assert_eq!(num(&e, "index") as usize, u.index);
                            assert_eq!(num(&e, "infoAddress") as u64, u.info_address);
                        }
                        nerr += 1;
                    }
                    _ => panic!("{name} row {i}: export and read layer disagree"),
                }
                if !matches!(cat.status[i], "live" | "alias") || cat.duplicate[i] {
                    continue;
                }
                let p = cat.stored_pathname[i].clone();
                if (100..200).contains(&cat.record_type[i]) {
                    match (
                        f.read_profile_record(&p, None),
                        b.read_profile_record(&p, MAX_CELLS),
                    ) {
                        (Ok(js), Ok(w)) => {
                            check_profile(&js.into(), &w);
                            np += 1;
                        }
                        (Err(e), Err(w)) => assert_eq!(kind_of(&e).1, w.kind.as_str()),
                        _ => panic!("{name} {p}: export and read layer disagree"),
                    }
                    match (
                        f.read_profile(&p, None, None, None, None, None),
                        b.read_profile(&p, None, true, MAX_BLOCKS, MAX_CELLS),
                    ) {
                        (Ok(js), Ok(w)) => {
                            check_profile(&js.into(), &w);
                            np += 1;
                        }
                        (Err(e), Err(w)) => assert_eq!(kind_of(&e).1, w.kind.as_str()),
                        _ => panic!("{name} {p}: export and read layer disagree"),
                    }
                }
                match (
                    f.read_associated_location(&p),
                    b.read_associated_location(&p),
                ) {
                    (Ok(js), Ok(Some(w))) => {
                        check_location(&js, &w);
                        na += 1;
                    }
                    (Ok(js), Ok(None)) => assert!(js.is_null()),
                    (Err(e), Err(w)) => assert_eq!(kind_of(&e).1, w.kind.as_str()),
                    _ => panic!("{name} {p}: export and read layer disagree"),
                }
                if cat.record_type[i] == dss_core::location::LOCATION {
                    let js = f.read_location(&p).unwrap();
                    check_location(&js.into(), &b.read_location(&p).unwrap());
                    nl += 1;
                }
            }
        }
    }
    assert!(
        np >= 8 && nl >= 10 && na >= 10 && nat >= 50 && nerr >= 10,
        "{np} {nl} {na} {nat} {nerr}"
    );
}

#[wasm_bindgen_test]
fn recovery_is_opt_in_from_javascript() {
    let bytes = fixture("synthetic_recovery7.dss");
    let mut d = DssFile::new(bytes).unwrap();
    let mut r = DssFile::open_recovery(bytes).unwrap();
    assert!(!d.recovery_mode() && r.recovery_mode());
    let rec = |c: JsValue| -> usize {
        get(&c, "recovered")
            .dyn_into::<Array>()
            .unwrap()
            .iter()
            .filter(|v| v.as_bool().unwrap())
            .count()
    };
    assert_eq!(rec(d.catalog().unwrap().into()), 0);
    assert!(rec(r.catalog().unwrap().into()) > 0);
}

#[wasm_bindgen_test]
fn profile_and_index_arguments_are_validated() {
    let bytes = fixture("synthetic_profile7.dss");
    let mut b = DssBuffer::new(bytes.to_vec()).unwrap();
    let c = b.catalog().unwrap();
    let p = (0..c.len())
        .find(|&i| matches!(c.record_type[i], 102 | 107))
        .map(|i| c.stored_pathname[i].clone())
        .unwrap();
    let mut f = DssFile::new(bytes).unwrap();
    for bad in [-1.0, 0.0, 1.5, 4_294_967_296.0, f64::NAN, f64::INFINITY] {
        let e = f.read_profile_record(&p, Some(bad)).err().unwrap();
        assert_eq!(kind_of(&e).1, "invalid_argument", "maxCells {bad}");
        let e = f
            .read_profile(&p, None, None, None, None, Some(bad))
            .err()
            .unwrap();
        assert_eq!(kind_of(&e).1, "invalid_argument", "maxCells {bad}");
    }
    // 2^32 is a valid maxBlocks (the limit goes up to 2^53 - 1), not a wrapped one.
    for bad in [-1.0, 0.0, 1.5, f64::NAN, f64::INFINITY] {
        let e = f
            .read_profile(&p, None, None, None, Some(bad), None)
            .err()
            .unwrap();
        assert_eq!(kind_of(&e).1, "invalid_argument", "maxBlocks {bad}");
    }
    // Refused small limits and a wide window, and the object stays usable.
    let e = f.read_profile_record(&p, Some(1.0)).err().unwrap();
    assert_eq!(
        kind_of(&e),
        ("DssValueError".into(), "invalid_argument".into())
    );
    let e = f
        .read_profile(&p, Some(0.0), Some(1_099_511_627_776.0), None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");
    assert!(text(&e, "message").contains("maxBlocks"));
    let e = f
        .read_profile(&p, Some(0.0), None, None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");
    assert!(f.read_profile(&p, None, None, None, None, None).is_ok());

    for (bad, kind) in [
        (0.5, "invalid_argument"),
        (f64::NAN, "invalid_argument"),
        (-1.0, "index"),
        (4_294_967_296.0, "index"),
        (1e9, "index"),
    ] {
        let e = f.read_record_at(bad).err().unwrap();
        assert_eq!(kind_of(&e).1, kind, "index {bad}");
        let e = f.entry_provenance(bad).err().unwrap();
        assert_eq!(kind_of(&e).1, kind, "index {bad}");
    }
    assert!(f.entry_provenance(0.0).is_ok());
}

/// The P35 exposure inside wasm32: two stored blocks whose `kbinDates` claim a century
/// returned 52,561,440 rows before the row limit.
#[wasm_bindgen_test]
fn read_dataset_row_limit_refuses_before_allocating() {
    use dss_core::dss7::Dss7File;
    let mut bytes = include_bytes!("../../../fixtures/public/p20_hec_rts7.dss").to_vec();
    let mut core = Dss7File::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let cat = core.catalog().unwrap();
    let i = cat
        .resolved_live()
        .into_iter()
        .find(|&i| matches!(cat.catalog.entries[i].record_type, 100 | 105))
        .unwrap();
    let p = lossy(&cat.catalog.entries[i].pathname);
    let key = common::dataset_key(&p).unwrap();
    let mut first = None;
    for j in cat.resolved_live() {
        let e = &cat.catalog.entries[j];
        if common::dataset_key(&lossy(&e.pathname)).as_deref() == Some(key.as_str()) {
            let r = core.lookup(&e.pathname).unwrap().unwrap();
            let at = r.bin_entry_address as usize * 8 + 6 * 8;
            let a = *first
                .get_or_insert_with(|| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
            bytes[at + 4..at + 8].copy_from_slice(&(a + 36_500).to_le_bytes());
        }
    }
    let mut f = DssFile::new(&bytes).unwrap();
    let e = f
        .read_dataset(&p, None, None, None, None, None)
        .err()
        .unwrap();
    assert_eq!(
        kind_of(&e),
        ("DssValueError".into(), "invalid_argument".into())
    );
    assert!(text(&e, "message").contains("maxRows"));
    // The limit cannot be switched off: -1 and friends are refused, and never wrap.
    for bad in [
        -1.0,
        0.0,
        1.5,
        4_294_967_296.0,
        4_294_967_295.0,
        f64::NAN,
        f64::INFINITY,
    ] {
        let e = f
            .read_dataset(&p, None, None, None, None, Some(bad))
            .err()
            .unwrap();
        assert_eq!(kind_of(&e).1, "invalid_argument", "maxRows {bad}");
    }
    // `dss-core` reads any spelling of the interval as the canonical E part: same refusal.
    let mut q: Vec<&str> = p.split('/').collect();
    q[5] = "1MinuteJUNK";
    let alias = q.join("/");
    let e = f
        .read_dataset(&alias, None, None, None, None, None)
        .err()
        .unwrap();
    assert!(text(&e, "message").contains("maxRows"));
    // A small explicit limit refuses too, and the object stays usable.
    let e = f
        .read_dataset(&p, None, None, None, None, Some(1000.0))
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");
    assert!(f.signature().is_ok());
}

/// Quality and note element sizes multiply every row: 8,192 days of 1-minute rows are under
/// 2^24 rows, but at 129 elements per row they exceed the whole wasm32 address space. A
/// time-series read of a profile is refused without assembling a hostile 90,000 depths.
#[wasm_bindgen_test]
fn expanded_metadata_and_profiles_refuse_inside_wasm() {
    use dss_core::dss7::Dss7File;
    let mut bytes = include_bytes!("../../../fixtures/public/p20_hec_rts7.dss").to_vec();
    let mut core = Dss7File::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let cat = core.catalog().unwrap();
    let i = cat
        .resolved_live()
        .into_iter()
        .find(|&i| matches!(cat.catalog.entries[i].record_type, 100 | 105))
        .unwrap();
    let p = lossy(&cat.catalog.entries[i].pathname);
    let key = common::dataset_key(&p).unwrap();
    let mut first = None;
    for j in cat.resolved_live() {
        let e = &cat.catalog.entries[j];
        if common::dataset_key(&lossy(&e.pathname)).as_deref() == Some(key.as_str()) {
            let r = core.lookup(&e.pathname).unwrap().unwrap();
            let at = r.bin_entry_address as usize * 8 + 6 * 8;
            let a = *first
                .get_or_insert_with(|| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
            bytes[at + 4..at + 8].copy_from_slice(&(a + 8_192).to_le_bytes());
            let h = r.info.internal_header.address as usize * 8;
            bytes[h + 11 * 4..h + 12 * 4].copy_from_slice(&64i32.to_le_bytes());
            bytes[h + 14 * 4..h + 15 * 4].copy_from_slice(&64i32.to_le_bytes());
        }
    }
    let mut f = DssFile::new(&bytes).unwrap();
    let e = f
        .read_dataset(&p, None, None, None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "invalid_argument");
    assert!(text(&e, "message").contains("x 129 elements"));

    let mut bytes = fixture("synthetic_profile7.dss").to_vec();
    let mut core = Dss7File::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let cat = core.catalog().unwrap();
    let live = cat.resolved_live();
    let mut q = None;
    for i in live {
        let e = &cat.catalog.entries[i];
        if matches!(e.record_type, 102 | 107) {
            let r = core.lookup(&e.pathname).unwrap().unwrap();
            let h = r.info.internal_header.address as usize * 8;
            bytes[h + 3 * 4..h + 4 * 4].copy_from_slice(&90_000i32.to_le_bytes());
            q = Some(lossy(&e.pathname));
        }
    }
    let mut f = DssFile::new(&bytes).unwrap();
    let e = f
        .read_dataset(&q.unwrap(), None, None, None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "unsupported");
}

#[wasm_bindgen_test]
fn dss6_refusals_are_typed() {
    let mut f = DssFile::new(fixture("synthetic_python6.dss")).unwrap();
    let mut b = DssBuffer::new(fixture("synthetic_python6.dss").to_vec()).unwrap();
    let p = b.catalog().unwrap().stored_pathname[0].clone();
    let e = f.read_location(&p).err().unwrap();
    assert_eq!(
        kind_of(&e),
        ("DssUnsupportedRecordError".into(), "unsupported".into())
    );
    let e = f.read_profile_record(&p, None).err().unwrap();
    assert_eq!(kind_of(&e).1, "unsupported");
    let e = f
        .read_profile(&p, None, None, None, None, None)
        .err()
        .unwrap();
    assert_eq!(kind_of(&e).1, "unsupported");
}

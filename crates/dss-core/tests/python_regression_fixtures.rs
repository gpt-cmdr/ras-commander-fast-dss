//! Synthetic Python regression fixtures. No HEC software or external data is used.
//! To regenerate, set DSS_PY_FIXTURE_OUT to a scratch directory and run this test,
//! then copy synthetic_python{6,7}.dss into fixtures/public/.
//!
//! P15 added location data: DSS6 series whose blocks agree or disagree on their header
//! coordinates, a Latin-1 B part, and a DSS7 location record (type 20) with a Latin-1
//! B part. Time-series headers zero words 13-24 unless coordinates are given.

mod dss6_ts_support;
mod dss7_common;

use dss6_ts_support::{f32w, f64w, rts_header, secs, ts_file, Payload, Rec};
use dss_core::dss6::{DatasetOptions, Dss6File, TsLayout};
use dss_core::dss7::{info::key, Dss7File, TsReadOptions};
use dss_core::TsValues;

const PATTERN: &str = "/A/B/FLOW/TS-PATTERN/1HOUR/PAT/";
const ORDINARY: [&str; 2] = [
    "/TS-BASIN/B/FLOW/01JAN2000/1HOUR/F/",
    "/A/B/FLOW/01JAN2000/1HOUR/TS-OBS/",
];
const VALUES: [f32; 5] = [-901.0, 10.0, 20.0, 30.0, -902.0];

/// DSS6 location series (P15): `(pathname bytes, coordinates, descriptors)`. Blocks of
/// `AGREE` share one location; blocks of `DISAGREE` do not. `É` (0xC9) is Latin-1.
pub fn location_blocks6() -> Vec<(Vec<u8>, [f64; 3], [i32; 6])> {
    let a = [-97.25, 30.5, 612.5];
    let d = [1, 0, 3, 1, 1, 0];
    vec![
        (b"/P15 LOC/AGREE/FLOW/01JAN2000/1HOUR/OBS/".to_vec(), a, d),
        (b"/P15 LOC/AGREE/FLOW/01FEB2000/1HOUR/OBS/".to_vec(), a, d),
        (
            b"/P15 LOC/DISAGREE/FLOW/01JAN2000/1HOUR/OBS/".to_vec(),
            a,
            d,
        ),
        (
            b"/P15 LOC/DISAGREE/FLOW/01FEB2000/1HOUR/OBS/".to_vec(),
            [-97.5, 30.5, 612.5],
            d,
        ),
        (
            b"/P15 LOC/GAGE \xC9/FLOW/01JAN2000/1HOUR/OBS/".to_vec(),
            [10.0, 20.0, 30.0],
            [4, 15, 2, 3, 2, 1],
        ),
    ]
}

/// RTS header with coordinates in words 13-18 and descriptors in 19-24
/// (`heclib_f/src/zsrtsi6.f` lines 878-887).
fn rts_header_with_location(c: [f64; 3], d: [i32; 6]) -> Vec<i32> {
    let mut h = rts_header(0, "CFS", "INST-VAL", 24);
    for (i, v) in c.iter().enumerate() {
        let w = f64w(*v);
        h[12 + 2 * i] = w[0];
        h[13 + 2 * i] = w[1];
    }
    h[18..24].copy_from_slice(&d);
    h
}

fn synthetic6() -> Vec<u8> {
    let mut recs = vec![Rec::live(PATTERN, 101)];
    recs.extend(ORDINARY.iter().map(|p| Rec::live(p, 100)));
    let locs = location_blocks6();
    for (path, _, _) in &locs {
        let mut r = Rec::live("/X/X/X/X/X/X/", 100);
        r.path = path.clone();
        recs.push(r);
    }
    let mut f = ts_file(&recs, *b"6-YN");
    for (path, c, d) in &locs {
        // Days in the block (01JAN / 01FEB 2000, 1HOUR in monthly blocks).
        let n = if path.windows(5).any(|w| w == b"01FEB") {
            696
        } else {
            744
        };
        f.attach_bytes(
            path,
            &Payload {
                internal_header: rts_header_with_location(*c, *d),
                data: (0..n).map(|i| f32w(i as f32)).collect(),
                kilnda: n,
                ..Default::default()
            },
        );
    }
    for rec in recs.iter().take(1 + ORDINARY.len()) {
        // heclib_f/src/zrrtpa6.f:zrrtpa6 reads KINDAT raw pattern values;
        // zrrtsb6.f:ZRRTSB6 uses KILNDA slots for ordinary regular blocks.
        let pattern = rec.path == PATTERN.as_bytes();
        let data = if pattern {
            VALUES.iter().map(|&v| f32w(v)).collect()
        } else {
            (0..744).map(|i| f32w(i as f32)).collect()
        };
        f.attach(
            std::str::from_utf8(&rec.path).unwrap(),
            &Payload {
                internal_header: rts_header(0, "CFS", "INST-VAL", 24),
                data,
                kilnda: if pattern { 5 } else { 744 },
                ..Default::default()
            },
        );
    }
    f.finish()
}

fn synthetic7() -> Vec<u8> {
    use dss7_common::{build, set_word, Rec, Spec};
    let mut loc = Rec::live("/X/X/Location Info////", 20);
    loc.path = LOCATION7.to_vec();
    let built = build(&Spec {
        recs: vec![Rec::live(PATTERN, 101), loc],
        ..Default::default()
    });
    let mut bytes = built.bytes;
    let info = built.info_addr[0] as usize;
    // heclib_c/src/Internal/zwriteNew.c:zwriteNew and zinit.c:zinit:
    // info addresses count int64 words; area lengths count int32 words.
    // Internal/ztsRetrievePattern.c:ztsRetrievePattern reads raw values1 floats,
    // sized by numberData (no compression or quality). The absent header defaults
    // to a zero time offset; no metadata is needed for these window regressions.
    let data_addr = bytes.len() / 8 - 1; // replace old EOF with payload
    bytes.truncate(data_addr * 8);
    for value in VALUES {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(&mut bytes, info + key::VALUES1_ADDRESS, data_addr as i64);
    set_word(&mut bytes, info + key::VALUES1_NUMBER, VALUES.len() as i64);
    set_word(&mut bytes, info + key::NUMBER_DATA, VALUES.len() as i64);
    set_word(&mut bytes, info + key::ALLOCATED_SIZE, 6);
    // Location record (P15), layout of Public/zlocationStore.c:zlocationStore():
    // values1 = x, y, z doubles, six codes, time-zone name; values2 = supplemental text.
    let loc_info = built.info_addr[1] as usize;
    bytes.truncate(eof * 8);
    let mut v1: Vec<u8> = Vec::new();
    for v in [5.5f64, 6.5, 7.5] {
        v1.extend_from_slice(&v.to_le_bytes());
    }
    for c in [1i32, 0, 3, 1, 1, 2] {
        v1.extend_from_slice(&c.to_le_bytes());
    }
    v1.extend_from_slice(b"UTC\0");
    let v1_ints = v1.len() / 4;
    let v2 = b"verticalDatum:NGVD29;\0\0\0".to_vec();
    let v2_ints = v2.len() / 4;
    let v1_addr = bytes.len() / 8;
    bytes.extend_from_slice(&v1);
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let v2_addr = bytes.len() / 8;
    bytes.extend_from_slice(&v2);
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(&mut bytes, loc_info + key::VALUES1_ADDRESS, v1_addr as i64);
    set_word(&mut bytes, loc_info + key::VALUES1_NUMBER, v1_ints as i64);
    set_word(&mut bytes, loc_info + key::VALUES2_ADDRESS, v2_addr as i64);
    set_word(&mut bytes, loc_info + key::VALUES2_NUMBER, v2_ints as i64);
    set_word(&mut bytes, loc_info + key::NUMBER_DATA, v1_ints as i64);
    set_word(
        &mut bytes,
        loc_info + key::ALLOCATED_SIZE,
        (v1_ints + v2_ints) as i64,
    );
    set_word(&mut bytes, 5, eof as i64);
    bytes
}

/// Latin-1 (`É` = 0xC9) location record in `synthetic_python7.dss` (P15).
const LOCATION7: &[u8] = b"/P15 LOC/GAGE \xC9/Location Info////";

#[test]
fn synthetic_python_fixtures() {
    let b6 = synthetic6();
    let f6 = Dss6File::from_bytes(&b6).unwrap();
    for p in ORDINARY {
        let record = f6.read_ts_record(p.as_bytes()).unwrap();
        assert_eq!(record.layout, TsLayout::Regular);
        let dataset = f6
            .read_ts_dataset(p.as_bytes(), &DatasetOptions::whole())
            .unwrap();
        assert_eq!(dataset.series.times, record.times);
    }
    let pattern = f6.read_ts_record(PATTERN.as_bytes()).unwrap();
    assert_eq!(pattern.layout, TsLayout::RegularPattern);
    assert_eq!(pattern.values, TsValues::F32(VALUES.to_vec()));
    let b7 = synthetic7();
    let mut f7 = Dss7File::from_bytes(&b7).unwrap();
    let pattern = f7
        .read_ts(PATTERN.as_bytes(), &TsReadOptions::default())
        .unwrap();
    assert!(pattern.meta.pattern);
    assert_eq!(pattern.ts.values, TsValues::F32(VALUES.to_vec()));
    assert_eq!(pattern.ts.times[0], secs(3000, 1, 1, 0, 0));
    // P15 location data.
    let loc = f7.read_location(LOCATION7).unwrap().location;
    assert_eq!((loc.x, loc.y, loc.z), (5.5, 6.5, 7.5));
    assert_eq!(loc.vertical_datum_code, 2);
    assert_eq!(loc.time_zone_name.as_deref(), Some("UTC"));
    assert_eq!(loc.supplemental_vertical_datum().as_deref(), Some("NGVD29"));
    assert!(f7
        .read_associated_location(b"/P15 LOC/GAGE \xC9/FLOW/01JAN2000/1HOUR/OBS/")
        .unwrap()
        .is_some());
    for (path, c, d) in location_blocks6() {
        let r = f6.read_ts_record(&path).unwrap();
        assert_eq!(r.coordinates, Some(c));
        assert_eq!(r.coordinate_descriptors, Some(d));
    }
    // Header words 13-24 are zero without coordinates: no location (HEC LCOORDS false).
    for p in ORDINARY {
        let r = f6.read_ts_record(p.as_bytes()).unwrap();
        assert_eq!(r.internal_header.len(), 24);
        assert!(r.internal_header[12..].iter().all(|&w| w == 0));
        assert_eq!(r.coordinates, None);
    }
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_python6.dss"), b6).unwrap();
        std::fs::write(out.join("synthetic_python7.dss"), b7).unwrap();
    }
}

#[test]
fn dss6_seconds_window_rounds_both_bounds_forward() {
    let bytes = synthetic6();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let base = secs(2000, 1, 2, 0, 0);
    for (start, end, expected) in [
        (base + 1, base + 3601, vec![base + 3600, base + 7200]),
        (base + 1, base + 1, vec![base + 3600]),
        (base, base + 3600, vec![base, base + 3600]),
    ] {
        let d = f
            .read_ts_dataset(ORDINARY[0].as_bytes(), &DatasetOptions::window(start, end))
            .unwrap();
        assert_eq!(d.series.times, expected);
    }
    assert!(f
        .read_ts_dataset(
            ORDINARY[0].as_bytes(),
            &DatasetOptions::window(base + 2, base + 1)
        )
        .is_err());
}

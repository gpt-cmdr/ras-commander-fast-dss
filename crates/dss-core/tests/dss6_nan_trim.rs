//! P30: the DSS6 dataset trim keeps NaN, as heclib's `ztsTrim` does.
//!
//! Sources (hec-dss fork tag `7-IU-15`): `heclib_c/src/Internal/ztsTrim.c` lines 83-170
//! trim leading and trailing values for which `zisMissingFloat` / `zisMissingDouble` is
//! true; `heclib_c/src/Utilities/zmissingFlag.c` lines 115-145 make those -FLT_MAX
//! (`UNDEFINED_FLOAT`, `UNDEFINED_DOUBLE = -(double)FLT_MAX`, `headers/heclib.h` lines
//! 21-22), -901 and -902. A stored NaN compares unequal to all three, so it is a value.
//! `Public/ztsRetrieve.c` lines 618-626 trim regular data only.
//!
//! Fixture `synthetic_nan_trim6.dss`, generated here (no HEC software; regenerate with
//! `DSS_PY_FIXTURE_OUT=<dir>`; the test fails when the committed bytes differ). All names
//! and values are invented.

mod dss6_ts_support;

use dss6_ts_support::{f32w, f64w, rts_header, secs, ts_file, Payload, Rec};
use dss_core::dss6::dataset::trim_missing;
use dss_core::dss6::{DatasetOptions, Dss6File, NoDataPolicy, TsReadOptions};
use dss_core::timeseries::TsValues;
use dss_core::{is_hec_missing, is_missing, TimeSeries, TsKind};
use std::path::PathBuf;

/// Float block: -902, -FLT_MAX, -901, NaN, 4.0 ..= 361.0, NaN, -FLT_MAX, -902, -901.
const A_2000: &str = "/P30/GAGE A/FLOW/01JAN2000/1DAY/OBS/";
/// The same layout as a REAL*8 (type 105) block.
const D_2000: &str = "/P30/GAGE D/FLOW/01JAN2000/1DAY/OBS/";
/// Two blocks: 2000 = -901 x 10, NaN, 11.0 ..= 364.0, NaN (last slot); 2001 all -FLT_MAX.
const B_2000: &str = "/P30/GAGE B/FLOW/01JAN2000/1DAY/OBS/";
const B_2001: &str = "/P30/GAGE B/FLOW/01JAN2001/1DAY/OBS/";
/// Every value NaN.
const N_2000: &str = "/P30/GAGE N/FLOW/01JAN2000/1DAY/OBS/";

const FIXTURE: &str = "synthetic_nan_trim6.dss";

/// Slot layout of A and D, as f64 (widened exactly for the float block).
fn edge_layout() -> Vec<f64> {
    let mut v: Vec<f64> = (0..366).map(f64::from).collect();
    v[0] = -902.0;
    v[1] = -f64::from(f32::MAX);
    v[2] = -901.0;
    v[3] = f64::NAN;
    v[362] = f64::NAN;
    v[363] = -f64::from(f32::MAX);
    v[364] = -902.0;
    v[365] = -901.0;
    v
}

fn b_2000() -> Vec<f32> {
    let mut v: Vec<f32> = (0..366).map(|i| i as f32).collect();
    for x in v.iter_mut().take(10) {
        *x = -901.0;
    }
    v[10] = f32::NAN;
    v[365] = f32::NAN;
    v
}

fn payload(data: Vec<i32>, n: i32) -> Payload {
    Payload {
        internal_header: rts_header(0, "CFS", "PER-AVER", 24),
        data,
        kilnda: n,
        ..Default::default()
    }
}

fn floats(v: &[f32]) -> Vec<i32> {
    v.iter().map(|&x| f32w(x)).collect()
}

fn file() -> Vec<u8> {
    let recs = vec![
        Rec::live(A_2000, 100),
        Rec::live(D_2000, 105),
        Rec::live(B_2000, 100),
        Rec::live(B_2001, 100),
        Rec::live(N_2000, 100),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    let a: Vec<f32> = edge_layout().iter().map(|&x| x as f32).collect();
    f.attach(A_2000, &payload(floats(&a), 366));
    // RTD: REAL*8 values, low word first (`zrrtsb6.f`, dss6/ts.rs 105 branch).
    let d: Vec<i32> = edge_layout().into_iter().flat_map(f64w).collect();
    f.attach(D_2000, &payload(d, 366));
    f.attach(B_2000, &payload(floats(&b_2000()), 366));
    f.attach(B_2001, &payload(floats(&vec![-f32::MAX; 365]), 365));
    f.attach(N_2000, &payload(floats(&vec![f32::NAN; 366]), 366));
    f.finish()
}

fn public(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name);
    std::fs::read(&p).expect("public fixture missing")
}

fn whole(trim: bool, policy: NoDataPolicy) -> DatasetOptions {
    DatasetOptions {
        trim_missing: trim,
        read: TsReadOptions::default().with_no_data(policy),
        ..DatasetOptions::whole()
    }
}

fn dataset(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').collect();
    format!(
        "/{}/{}/{}//{}/{}/",
        parts[1], parts[2], parts[3], parts[5], parts[6]
    )
}

#[test]
fn hec_missing_test_is_zis_missing() {
    // zmissingFlag.c lines 115-145: -FLT_MAX (as float, and widened for doubles),
    // -901, -902; NaN is not missing. is_missing (marker-to-NaN mapping) is unchanged.
    for v in [-901.0, -902.0, -f64::from(f32::MAX)] {
        assert!(is_hec_missing(v) && is_missing(v), "{v}");
    }
    assert!(!is_hec_missing(f64::NAN));
    assert!(is_missing(f64::NAN));
    assert!(!is_hec_missing(-f64::MAX) && !is_missing(-f64::MAX));
}

#[test]
fn trim_keeps_leading_and_trailing_nan() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    for (p, wide) in [(A_2000, false), (D_2000, true)] {
        for policy in [NoDataPolicy::Return, NoDataPolicy::Refuse] {
            let d = f
                .read_ts_dataset(dataset(p).as_bytes(), &whole(true, policy))
                .unwrap();
            assert_eq!(matches!(d.series.values, TsValues::F64(_)), wide, "{p}");
            let v = d.series.values.to_f64();
            // Slots 3 ..= 362 kept: 360 values, NaN at both ends; 6 trimmed.
            assert_eq!(v.len(), 360, "{p}");
            assert_eq!(d.report.trimmed, 6, "{p}");
            assert!(v[0].is_nan() && v[359].is_nan(), "{p}");
            assert_eq!(v[1], 4.0);
            assert_eq!(v[358], 361.0);
            assert_eq!(d.series.times[0], secs(2000, 1, 5, 0, 0), "{p}");
            assert_eq!(d.series.times[359], secs(2000, 12, 29, 0, 0), "{p}");
            let u = f
                .read_ts_dataset(dataset(p).as_bytes(), &whole(false, policy))
                .unwrap();
            assert_eq!(u.series.len(), 366, "{p}");
            assert_eq!(u.report.trimmed, 0);
        }
    }
}

#[test]
fn trailing_nan_kept_across_a_trimmed_block() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let d = f
        .read_ts_dataset(dataset(B_2000).as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(d.report.blocks_found, 2);
    let v = d.series.values.to_f64();
    // 2000 slots 10 ..= 365 kept (356); the 2001 -FLT_MAX block and 10 x -901 trimmed.
    assert_eq!(v.len(), 356);
    assert_eq!(d.report.trimmed, 10 + 365);
    assert!(v[0].is_nan() && v[355].is_nan());
    assert_eq!(*d.series.times.last().unwrap(), secs(2001, 1, 1, 0, 0));
}

#[test]
fn all_nan_block_is_kept_whole() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    for policy in [NoDataPolicy::Return, NoDataPolicy::Refuse] {
        let d = f
            .read_ts_dataset(dataset(N_2000).as_bytes(), &whole(true, policy))
            .unwrap();
        assert_eq!(d.series.len(), 366);
        assert_eq!(d.report.trimmed, 0);
        assert!(d.series.values.to_f64().iter().all(|v| v.is_nan()));
    }
}

#[test]
fn trim_missing_on_a_series() {
    // The public function, quality kept aligned.
    let mut ts = TimeSeries {
        pathname: b"/A/B/C//1DAY/F/".to_vec(),
        record_type: 100,
        kind: TsKind::Regular {
            interval_seconds: 86_400,
        },
        times: (1..=7).collect(),
        values: TsValues::F32(vec![
            -901.0,
            f32::NAN,
            1.0,
            -902.0,
            f32::NAN,
            -f32::MAX,
            -902.0,
        ]),
        quality: Some((1..=7).collect()),
        units: String::new(),
        data_type: String::new(),
        time_zone: None,
    };
    assert_eq!(trim_missing(&mut ts), 3);
    assert_eq!(ts.times, vec![2, 3, 4, 5]);
    assert_eq!(ts.quality, Some(vec![2, 3, 4, 5]));
    let v = ts.values.to_f64();
    assert!(v[0].is_nan() && v[3].is_nan());
    assert_eq!(&v[1..3], &[1.0, -902.0]);
}

#[test]
fn committed_fixture_matches_generator() {
    let bytes = file();
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(FIXTURE), &bytes).unwrap();
    }
    assert_eq!(
        public(FIXTURE),
        bytes,
        "committed fixture differs from the generator"
    );
}

//! DSS6 regular blocks whose stored values are all missing (P28 addition, g54 triage).
//!
//! HEC reports "no data" for such a block. Sources: hec-dss fork tag `7-IU-15`
//! (`34ffe0c`, the release whose DSS6 code Monolith 3.3.27's `javaHeclib` carries):
//! `heclib/heclib_f/src/zrrtsi.f:ZRRTSI6` sets `LFOUND` only for a value that is neither
//! -901 nor -902 (lines 618-649; floats line 631, doubles line 645) and line 651 sets
//! `ISTAT = 4`; `heclib/heclib_c/src/Dss6/ztsRetrieveReg6.c` lines 255-259 return
//! `STATUS_RECORD_NOT_FOUND` (-1). Monolith 3.3.27 `HecTimeSeries.read` returns -2 for -1
//! (bytecode 327-334). DSS7 differs: hec-dss fork main `6591930`,
//! `heclib/heclib_c/src/Internal/ztsRetrieveReg7.c` lines 641 and 699-700, treat the block
//! as found.
//!
//! The engine returns the block from a record read with its stored values (raw -901/-902;
//! the Python layer maps them to NaN). A dataset read keeps every slot with
//! `trim_missing = false` and, by default (`DatasetOptions::whole`), trims leading and
//! trailing missing values as heclib's `ztsTrim` does, so a dataset made only of
//! all-missing blocks has no rows (docs/format/dss6.md §7.4).
//!
//! `fixtures/public/synthetic_all_missing6.dss` is generated here (no HEC software;
//! regenerate with `DSS_PY_FIXTURE_OUT=<dir>`); the test fails when the committed bytes
//! differ from the generator. All names and values are invented.

mod dss6_ts_support;

use dss6_ts_support::{f32w, f64w, rts_header, ts_file, Payload, Rec};
use dss_core::dss6::{DatasetOptions, Dss6File};
use dss_core::timeseries::TsValues;
use std::path::PathBuf;

/// RTS (float) daily block of 2000 (366 values), every value -901.
const M_2000: &str = "/P28/GAGE M/FLOW/01JAN2000/1DAY/OBS/";
/// RTS daily block of 2001 (365 values) with data: the same dataset mixes both, as in g54.
const M_2001: &str = "/P28/GAGE M/FLOW/01JAN2001/1DAY/OBS/";
/// RTS block of 2000, -901 and -902 alternating; the dataset's only block.
const Q_2000: &str = "/P28/GAGE Q/FLOW/01JAN2000/1DAY/OBS/";
/// RTD (type 105, REAL*8) block of 2000, every value -901 (`zrrtsi.f` line 645 path).
const R_2000: &str = "/P28/GAGE R/FLOW/01JAN2000/1DAY/OBS/";
/// RTS block of 2000 with quality (`KIQUAL = 1`), every value -901, quality 3.
const W_2000: &str = "/P28/GAGE W/FLOW/01JAN2000/1DAY/OBS/";
/// RTS block of 2000, every value -902.
const N_2000: &str = "/P28/GAGE N/FLOW/01JAN2000/1DAY/OBS/";
/// RTS block of 2000, every value -FLT_MAX: not a -901/-902 marker for `zrrtsi.f`, so
/// HEC reports the block as found (the Python layer still maps the values to NaN).
const X_2000: &str = "/P28/GAGE X/FLOW/01JAN2000/1DAY/OBS/";

fn payload(h: Vec<i32>, data: Vec<i32>, n: i32, kiqual: i32) -> Payload {
    Payload {
        internal_header: h,
        data,
        kilnda: n,
        kiqual,
        ..Default::default()
    }
}

fn block(f: &mut dss6_ts_support::TsFile, path: &str, values: Vec<f32>) {
    let n = values.len() as i32;
    let data = values.into_iter().map(f32w).collect();
    f.attach(
        path,
        &payload(rts_header(0, "CFS", "PER-AVER", 24), data, n, 0),
    );
}

fn file() -> Vec<u8> {
    let recs = vec![
        Rec::live(M_2000, 100),
        Rec::live(M_2001, 100),
        Rec::live(Q_2000, 100),
        Rec::live(R_2000, 105),
        Rec::live(W_2000, 100),
        Rec::live(N_2000, 100),
        Rec::live(X_2000, 100),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    block(&mut f, M_2000, vec![-901.0; 366]);
    block(&mut f, M_2001, (0..365).map(|i| i as f32).collect());
    block(
        &mut f,
        Q_2000,
        (0..366)
            .map(|i| if i % 2 == 0 { -901.0 } else { -902.0 })
            .collect(),
    );
    block(&mut f, N_2000, vec![-902.0; 366]);
    block(&mut f, X_2000, vec![-f32::MAX; 366]);
    // RTD: REAL*8 values, low word first (`zrrtsb6.f`, dss6/ts.rs 105 branch).
    let d: Vec<i32> = (0..366).flat_map(|_| f64w(-901.0)).collect();
    f.attach(
        R_2000,
        &payload(rts_header(0, "CFS", "PER-AVER", 24), d, 366, 0),
    );
    // RTS with quality: (REAL*4 value, INTEGER*4 quality) interleaved (`zrrtsb6.f`
    // lines 186-217).
    let q: Vec<i32> = (0..366).flat_map(|_| [f32w(-901.0), 3]).collect();
    f.attach(
        W_2000,
        &payload(rts_header(0, "CFS", "PER-AVER", 24), q, 366, 1),
    );
    f.finish()
}

/// Values as f64 (float and double records alike).
fn vals(v: &TsValues) -> Vec<f64> {
    match v {
        TsValues::F32(x) => x.iter().map(|&a| f64::from(a)).collect(),
        TsValues::F64(x) => x.clone(),
    }
}

fn is_marker(v: f64) -> bool {
    v == -901.0 || v == -902.0
}

#[test]
fn all_missing_blocks_are_returned() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let r = f.read_ts_record(M_2000.as_bytes()).unwrap();
    assert_eq!(r.len(), 366);
    assert!(vals(&r.values).iter().all(|&v| v == -901.0));
    let r = f.read_ts_record(Q_2000.as_bytes()).unwrap();
    assert_eq!(r.len(), 366);
    assert!(vals(&r.values).iter().all(|&v| is_marker(v)));
    assert_eq!(vals(&r.values)[1], -902.0);
    // RTD (doubles).
    let r = f.read_ts_record(R_2000.as_bytes()).unwrap();
    assert!(matches!(r.values, TsValues::F64(_)));
    assert_eq!(r.len(), 366);
    assert!(vals(&r.values).iter().all(|&v| v == -901.0));
    // With quality: values and the stored quality words.
    let r = f.read_ts_record(W_2000.as_bytes()).unwrap();
    assert_eq!(r.len(), 366);
    assert!(vals(&r.values).iter().all(|&v| v == -901.0));
    assert_eq!(r.quality.as_deref(), Some(&[3; 366][..]));
    // All -902.
    let r = f.read_ts_record(N_2000.as_bytes()).unwrap();
    assert!(vals(&r.values).iter().all(|&v| v == -902.0));
    // -FLT_MAX (HEC: found) is kept bit for bit.
    let r = f.read_ts_record(X_2000.as_bytes()).unwrap();
    assert!(vals(&r.values).iter().all(|&v| v == -f64::from(f32::MAX)));
}

#[test]
fn dataset_with_a_data_block() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let path = b"/P28/GAGE M/FLOW//1DAY/OBS/";
    // Untrimmed: the all-missing block is read and its slots keep the stored -901.
    let untrimmed = DatasetOptions {
        trim_missing: false,
        ..DatasetOptions::whole()
    };
    let d = f.read_ts_dataset(path, &untrimmed).unwrap();
    assert_eq!(d.report.blocks_found, 2);
    assert_eq!(d.report.filled_no_data, 0);
    let v = vals(&d.series.values);
    assert_eq!(v.len(), 366 + 365);
    assert!(v[..366].iter().all(|&x| x == -901.0));
    assert_eq!(v[366], 0.0);
    assert_eq!(v[366 + 364], 364.0);
    // Default whole-dataset read: leading missing values trimmed (heclib `ztsTrim`); the
    // block is still found.
    let d = f.read_ts_dataset(path, &DatasetOptions::whole()).unwrap();
    assert_eq!(d.report.blocks_found, 2);
    assert_eq!(d.report.trimmed, 366);
    assert_eq!(vals(&d.series.values).len(), 365);
}

#[test]
fn dataset_of_only_all_missing_blocks() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let untrimmed = DatasetOptions {
        trim_missing: false,
        ..DatasetOptions::whole()
    };
    for p in [
        &b"/P28/GAGE Q/FLOW//1DAY/OBS/"[..],
        b"/P28/GAGE R/FLOW//1DAY/OBS/",
        b"/P28/GAGE W/FLOW//1DAY/OBS/",
        b"/P28/GAGE N/FLOW//1DAY/OBS/",
    ] {
        let name = String::from_utf8_lossy(p);
        // trim_missing = false: every slot, markers kept.
        let d = f.read_ts_dataset(p, &untrimmed).unwrap();
        assert_eq!(d.report.blocks_found, 1, "{name}");
        let v = vals(&d.series.values);
        assert_eq!(v.len(), 366, "{name}");
        assert!(v.iter().all(|&x| is_marker(x)), "{name}");
        // Default: `ztsTrim` removes every value, so no rows (not an error).
        let d = f.read_ts_dataset(p, &DatasetOptions::whole()).unwrap();
        assert_eq!(d.report.blocks_found, 1, "{name}");
        assert_eq!(d.report.trimmed, 366, "{name}");
        assert!(d.series.times.is_empty(), "{name}");
        assert_eq!(vals(&d.series.values).len(), 0, "{name}");
    }
}

#[test]
fn committed_fixture_matches_generator() {
    let bytes = file();
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_all_missing6.dss"), &bytes).unwrap();
    }
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_all_missing6.dss");
    let on_disk = std::fs::read(&committed).expect("public fixture missing");
    assert_eq!(
        on_disk, bytes,
        "committed fixture differs from the generator"
    );
}

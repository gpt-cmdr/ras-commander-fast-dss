//! P76: the time stamps the regular writers accept
//! for `times=` (`value_times`, `value_times6`) are where the stored record reads back.
//!
//! For every regular interval, DSS7 and DSS6, and many starts (every day of 2019 and 2020 for
//! the calendar intervals, starts in the week holding 1 January of decade blocks, leap-year
//! starts in 1904, 2000, 2016, 2020 and 2024), a series is written from the start and read
//! back as a whole dataset. When `value_times` accepts the start, its stamps must equal the
//! read-back stamps exactly; when it refuses, the read-back must hold two values at one time
//! or lose a value. HEC's own placement is checked separately against HEC Monolith
//! (`tests/python/test_p76_calendar.py` and its fixtures).

use std::path::{Path, PathBuf};

use dss_core::dss6::write as w6;
use dss_core::dss6::{DatasetIndex, DatasetOptions, Dss6File};
use dss_core::dss7::ts::TsReadOptions;
use dss_core::dss7::write::{self as w7, RtsInput, RtsValues, SyncMode, WriteOptions};
use dss_core::dss7::Dss7File;
use dss_core::time::ymd_to_julian;
use dss_core::timeseries::TsValues;

const DAY: i64 = 86_400;

/// A temporary root of this test's own (tests of one binary run in parallel, so
/// a root shared by the process must not be removed by one of them).
fn dir(test: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "p76_calendar_{}_{test}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn t(y: i64, m: u32, d: u32, secs: i64) -> i64 {
    ymd_to_julian(y, m, d) * DAY + secs
}

fn starts(e: &str) -> Vec<i64> {
    let mut v = Vec::new();
    let calendar = matches!(e, "1Week" | "Tri-Month" | "Semi-Month" | "1Month" | "1Year");
    if calendar {
        let j0 = ymd_to_julian(2019, 1, 1);
        for d in 0..731 {
            v.push((j0 + d) * DAY);
        }
        v.push(t(2020, 6, 30, 12 * 3600));
        v.push(t(2020, 2, 1, 6 * 3600));
        for y in [1904, 2000, 2016, 2020, 2024] {
            for (m, d) in [(2, 28), (2, 29), (3, 1), (3, 2), (3, 3), (6, 15), (12, 31)] {
                v.push(t(y, m, d, 0));
            }
        }
        // Weeks holding 1 January of a decade block (and 1950, 2040, whose 1 January is a
        // Sunday).
        for y in [1950, 1980, 1990, 2000, 2010, 2020, 2030, 2040, 2100] {
            for d in -7..7 {
                v.push((ymd_to_julian(y, 1, 1) + d) * DAY);
            }
        }
    } else {
        for (y, m, d) in [(2020, 2, 28), (2020, 2, 29), (2019, 12, 31), (2021, 1, 1)] {
            for s in [0, 900, 3600, 6 * 3600, 22 * 3600 + 900, DAY] {
                v.push(t(y, m, d, s));
            }
        }
    }
    v
}

const E_PARTS: [&str; 16] = [
    "1Minute",
    "5Minute",
    "10Minute",
    "15Minute",
    "30Minute",
    "1Hour",
    "2Hour",
    "3Hour",
    "6Hour",
    "12Hour",
    "1Day",
    "1Week",
    "Tri-Month",
    "Semi-Month",
    "1Month",
    "1Year",
];

fn opts() -> WriteOptions {
    WriteOptions {
        sync: SyncMode::None,
        ..WriteOptions::default()
    }
}

fn write(v: u8, f: &Path, path: &[u8], start: i64, n: usize) {
    let vals: Vec<f32> = (0..n).map(|i| i as f32 + 1.0).collect();
    let input = RtsInput::new(path, start, RtsValues::F32(&vals), b"CFS", b"INST-VAL");
    if v == 7 {
        w7::write_rts(f, &input, &opts()).unwrap();
    } else {
        w6::write_rts6(f, &input, &opts()).unwrap();
    }
}

type Case = (String, i64, usize, Result<Vec<i64>, String>);

/// Write every case, then read every dataset back with one catalog walk: the read stamps.
fn run(v: u8, d: &Path) -> Vec<(Case, Vec<i64>)> {
    let f = d.join(format!("cal{v}.dss"));
    if v == 7 {
        w7::create_dss7_with(&f, SyncMode::None).unwrap();
    } else {
        w6::create_dss6_with(&f, SyncMode::None).unwrap();
    }
    let mut cases: Vec<Case> = Vec::new();
    for e in E_PARTS {
        for start in starts(e) {
            for n in [1usize, 3, 13] {
                let e_stored = if v == 6 {
                    w6::regular_e_part6(e.as_bytes()).unwrap().1.to_string()
                } else {
                    e.to_string()
                };
                let path = format!("/P76/CAL/FLOW//{e_stored}/K{}/", cases.len());
                let vt = if v == 7 {
                    w7::value_times(path.as_bytes(), start, n)
                } else {
                    w6::value_times6(path.as_bytes(), start, n)
                };
                write(v, &f, path.as_bytes(), start, n);
                cases.push((path, start, n, vt.map_err(|e| e.to_string())));
            }
        }
    }
    let mut out = Vec::with_capacity(cases.len());
    if v == 7 {
        let mut file = Dss7File::open(&f).unwrap();
        let cat = file.catalog().unwrap();
        let o = TsReadOptions {
            window: None,
            trim_missing: true,
        };
        for c in cases {
            let times = file
                .read_ts_with_catalog(&cat, c.0.as_bytes(), &o)
                .unwrap()
                .ts
                .times;
            out.push((c, times));
        }
    } else {
        let file = Dss6File::open(&f).unwrap();
        let index = DatasetIndex::build(&file).unwrap();
        for c in cases {
            let times = file
                .read_ts_dataset_indexed(&index, c.0.as_bytes(), &DatasetOptions::whole())
                .unwrap()
                .series
                .times;
            out.push((c, times));
        }
    }
    out
}

fn check(v: u8, d: &Path) -> (usize, usize) {
    let (mut accepted, mut refused) = (0, 0);
    for ((path, start, n, vt), read) in run(v, d) {
        match vt {
            Ok(times) => {
                accepted += 1;
                assert_eq!(times, read, "DSS{v} {path} start {start} n {n}");
            }
            Err(err) => {
                refused += 1;
                let moved = read.len() != n || read.windows(2).any(|w| w[1] <= w[0]);
                // DSS6 also refuses where HEC's chained read (from the first kept slot) would
                // stamp a value away from its own period, though this crate reads
                // it at its own period's stamp.
                let hec_moves = v == 6 && err.contains("HEC-DSS would read value");
                assert!(
                    moved || hec_moves,
                    "DSS{v} {path} start {start} n {n}: refused ({err}) but read back {read:?}"
                );
            }
        }
    }
    (accepted, refused)
}

#[test]
fn p76_value_times_are_the_read_back_stamps_dss7() {
    let d = dir("dss7");
    let (a, r) = check(7, &d);
    eprintln!("DSS7: {a} accepted, {r} refused");
    assert!(a > 5_000 && r > 0);
}

#[test]
fn p76_value_times_are_the_read_back_stamps_dss6() {
    let d = dir("dss6");
    let (a, r) = check(6, &d);
    eprintln!("DSS6: {a} accepted, {r} refused");
    assert!(a > 5_000);
}

/// Missing values. Leading (1, 2, 3), trailing, interior and leading+trailing
/// missing values (HEC's missing markers) in series of every calendar interval, DSS7 and DSS6,
/// from every 3rd day of 2019-2020 and the leap-year starts. For every series `times=` would
/// accept (`value_times_from` with the first valid index), each finite value reads back at its
/// accepted stamp. For DSS6 every series also reads the same finite (time, value) pairs trimmed
/// and untrimmed: a value's stamp does not depend on the other values.
fn missing_patterns(v: u8, d: &Path) -> (usize, usize) {
    let f = d.join(format!("miss{v}.dss"));
    if v == 7 {
        w7::create_dss7_with(&f, SyncMode::None).unwrap();
    } else {
        w6::create_dss6_with(&f, SyncMode::None).unwrap();
    }
    let missing = if v == 7 { -f32::MAX } else { -901.0 };
    let patterns: [&[usize]; 6] = [&[0], &[0, 1], &[0, 1, 2], &[7, 8], &[2, 5], &[0, 8]];
    let mut cases = Vec::new();
    let mut k = 0usize;
    for e in ["1Week", "Tri-Month", "Semi-Month", "1Month", "1Year"] {
        let e_stored = if v == 6 {
            w6::regular_e_part6(e.as_bytes()).unwrap().1.to_string()
        } else {
            e.to_string()
        };
        for (si, start) in starts(e).into_iter().enumerate() {
            if si % 3 != 0 && si < 731 {
                continue;
            }
            for miss in patterns {
                let n = 9usize;
                let mut vals: Vec<f32> = (0..n).map(|i| i as f32 + 1.0).collect();
                for &i in miss {
                    vals[i] = missing;
                }
                let first_valid = (0..n).find(|&i| vals[i] != missing).unwrap_or(n);
                let path = format!("/P76/MISS/FLOW//{e_stored}/K{k}/");
                k += 1;
                let vt = if v == 7 {
                    w7::value_times_from(path.as_bytes(), start, n, first_valid)
                } else {
                    w6::value_times6_from(path.as_bytes(), start, n, first_valid)
                };
                let input = RtsInput::new(
                    path.as_bytes(),
                    start,
                    RtsValues::F32(&vals),
                    b"CFS",
                    b"INST-VAL",
                );
                if v == 7 {
                    w7::write_rts(&f, &input, &opts()).unwrap();
                } else {
                    w6::write_rts6(&f, &input, &opts()).unwrap();
                }
                cases.push((path, vals, first_valid, vt.ok()));
            }
        }
    }
    let finite = |times: &[i64], vals: &TsValues| -> Vec<(i64, f32)> {
        let v: Vec<f32> = match vals {
            TsValues::F32(v) => v.clone(),
            TsValues::F64(v) => v.iter().map(|&x| x as f32).collect(),
        };
        times
            .iter()
            .zip(v)
            .filter(|(_, x)| *x > -900.0)
            .map(|(t, x)| (*t, x))
            .collect()
    };
    let (mut accepted, mut refused) = (0, 0);
    let mut f7 = (v == 7).then(|| Dss7File::open(&f).unwrap());
    let cat7 = f7.as_mut().map(|x| x.catalog().unwrap());
    let f6 = (v == 6).then(|| Dss6File::open(&f).unwrap());
    let idx6 = f6.as_ref().map(|x| DatasetIndex::build(x).unwrap());
    let mut read = |path: &str, trim: bool| -> Vec<(i64, f32)> {
        if let (Some(file), Some(cat)) = (f7.as_mut(), cat7.as_ref()) {
            let o = TsReadOptions {
                window: None,
                trim_missing: trim,
            };
            let ts = file
                .read_ts_with_catalog(cat, path.as_bytes(), &o)
                .unwrap()
                .ts;
            finite(&ts.times, &ts.values)
        } else {
            let (file, idx) = (f6.as_ref().unwrap(), idx6.as_ref().unwrap());
            let mut o = DatasetOptions::whole();
            o.trim_missing = trim;
            let ts = file
                .read_ts_dataset_indexed(idx, path.as_bytes(), &o)
                .unwrap()
                .series;
            finite(&ts.times, &ts.values)
        }
    };
    for (path, vals, first_valid, vt) in cases {
        let trimmed = read(&path, true);
        if v == 6 {
            assert_eq!(
                trimmed,
                read(&path, false),
                "DSS6 {path}: trimmed and untrimmed differ"
            );
        }
        match vt {
            Some(times) => {
                accepted += 1;
                let want: Vec<(i64, f32)> = (first_valid..vals.len())
                    .filter(|&i| vals[i] > -900.0)
                    .map(|i| (times[i - first_valid], vals[i]))
                    .collect();
                assert_eq!(trimmed, want, "DSS{v} {path}: accepted times");
            }
            None => refused += 1,
        }
    }
    (accepted, refused)
}

#[test]
fn p76_missing_values_read_at_their_accepted_times_dss7() {
    let d = dir("missing7");
    let (a, r) = missing_patterns(7, &d);
    eprintln!("DSS7 missing patterns: {a} accepted, {r} refused");
    assert!(a > 1_000);
}

#[test]
fn p76_missing_values_read_at_their_accepted_times_dss6() {
    let d = dir("missing6");
    let (a, r) = missing_patterns(6, &d);
    eprintln!("DSS6 missing patterns: {a} accepted, {r} refused");
    assert!(a > 1_000 && r > 0);
}

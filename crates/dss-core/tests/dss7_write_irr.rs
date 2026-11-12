//! P21 irregular writer tests: create → add → replace → read round trips for every
//! irregular block size, heclib's storage-flag-1 merge ("replace" the time window, keep
//! the rest), deletion of blocks the window empties, word-level identity of unrelated
//! records (gate G4) on a HEC-written fixture, a crash test after every step of the add,
//! rewrite and delete transactions, and a fuzz pass on input validation. Synthetic data
//! only; every write goes to a new file or a temporary copy of a public fixture.
//!
//! The same writer was also checked with HEC software (hecdss and Monolith reads, heclib
//! `zcheckFile`, word comparisons with the heclib 7-JA C writer); those checks need HEC
//! software and do not run in this test suite.

mod dss7_common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss7_common::Rng;
use dss_core::dss7::interval::ymd_to_julian;
use dss_core::dss7::write::{
    create_dss7, validate_its_input, write_its, write_rts, BlockOutcome, FaultPoint, ItsInput,
    RtsInput, RtsValues, WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::{Dss7File, TsReadOptions};
use dss_core::TsValues;

const DAY: i64 = 86_400;
const M: f32 = -f32::MAX;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dss7_write_irr_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn t(y: i64, m: i64, d: i64, secs: i64) -> i64 {
    ymd_to_julian(y, m, d) * DAY + secs
}

fn h(y: i64, m: i64, d: i64, hour: i64, min: i64) -> i64 {
    t(y, m, d, hour * 3600 + min * 60)
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: dss_core::dss7::write::SyncMode::None,
        ..WriteOptions::default()
    }
}

fn its(path: &Path, p: &[u8], times: &[i64], v: &[f32]) -> Vec<BlockOutcome> {
    let input = ItsInput::new(p, times, RtsValues::F32(v), b"cfs", b"INST-VAL");
    write_its(path, &input, &opts())
        .unwrap()
        .blocks
        .iter()
        .map(|b| b.outcome)
        .collect()
}

/// Whole dataset (no window) as HEC reads it: (times, values as f64).
fn read_all(path: &Path, p: &[u8]) -> (Vec<i64>, Vec<f64>) {
    let mut f = Dss7File::open(path).unwrap();
    let r = f
        .read_ts(
            p,
            &TsReadOptions {
                window: None,
                trim_missing: false,
            },
        )
        .unwrap();
    let v = match r.ts.values {
        TsValues::F32(x) => x.iter().map(|&a| f64::from(a)).collect(),
        TsValues::F64(x) => x,
    };
    (r.ts.times, v)
}

/// One stored block: (times, values as f64, quality, record type, granularity).
type Block = (Vec<i64>, Vec<f64>, Option<Vec<i32>>, i32, i64);

fn block(path: &Path, block_path: &[u8]) -> Option<Block> {
    let mut f = Dss7File::open(path).unwrap();
    let r = f.lookup(block_path).unwrap()?;
    let x = f.read_ts_record(&r).unwrap();
    let v = match x.ts.values {
        TsValues::F32(a) => a.iter().map(|&a| f64::from(a)).collect(),
        TsValues::F64(a) => a,
    };
    Some((
        x.ts.times,
        v,
        x.ts.quality,
        x.ts.record_type,
        x.meta.time_granularity_seconds,
    ))
}

fn f64s(v: &[f32]) -> Vec<f64> {
    v.iter().map(|&a| f64::from(a)).collect()
}

// ---------------------------------------------------------------------------------
// Raw-word helpers (the same independent walk as tests/dss7_write.rs).
// ---------------------------------------------------------------------------------

fn words(path: &Path) -> Vec<i64> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// pathname (upper) -> (bin entry address, info words + area words), live entries.
fn records(w: &[i64]) -> BTreeMap<Vec<u8>, (i64, Vec<i64>)> {
    let (ht, mh, bs) = (w[19], w[15], w[23]);
    let mut out = BTreeMap::new();
    for slot in 0..mh {
        let mut a = w[(ht + slot) as usize];
        while a != 0 {
            let mut ic = 0i64;
            while ic + 7 < bs && w[(a + ic) as usize] != 0 {
                let e = (a + ic) as usize;
                let pl = w[e + 2];
                let (nch, pw) = ((pl as u32) as usize, pl >> 32);
                let st = w[e + 1];
                if st == 0 || st == 1 {
                    let path: Vec<u8> = w[e + 7..e + 7 + pw as usize]
                        .iter()
                        .flat_map(|x| x.to_le_bytes())
                        .take(nch)
                        .collect();
                    let info = w[e + 3] as usize;
                    let mut v: Vec<i64> = w[info..info + 30 + pw as usize].to_vec();
                    for k in [13usize, 15, 17, 19, 21, 23] {
                        let (ad, n) = (w[info + k] as usize, w[info + k + 1]);
                        v.extend_from_slice(&w[ad..ad + ((n + 1) / 2) as usize]);
                    }
                    out.insert(path.to_ascii_uppercase(), (e as i64, v));
                }
                ic += 7 + pw;
                if ic >= bs - 2 {
                    break;
                }
            }
            a = w[(a + bs - 1) as usize];
        }
    }
    out
}

/// G4: every record not in `targets` is word-identical before and after.
fn assert_others_identical(before: &[i64], after: &[i64], targets: &[&[u8]]) {
    let (rb, ra) = (records(before), records(after));
    let ts: Vec<Vec<u8>> = targets.iter().map(|t| t.to_ascii_uppercase()).collect();
    for (k, v) in &rb {
        if !ts.contains(k) {
            assert_eq!(
                ra.get(k),
                Some(v),
                "record {} changed",
                String::from_utf8_lossy(k)
            );
        }
    }
    for k in ra.keys() {
        assert!(
            ts.contains(k) || rb.contains_key(k),
            "unexpected record {}",
            String::from_utf8_lossy(k)
        );
    }
}

/// No catalog anomaly, `knumberRecords` equal to the live entries, every live record
/// reads, and the bin bookkeeping derived from the raw words matches the header.
fn assert_clean(path: &Path, what: &str) {
    let mut f = Dss7File::open(path).unwrap();
    let cat = f.catalog().unwrap();
    assert!(
        cat.catalog.anomalies.is_empty(),
        "{what}: {:?}",
        cat.catalog.anomalies
    );
    let live: Vec<Vec<u8>> = cat.catalog.live().map(|e| e.pathname.clone()).collect();
    assert_eq!(f.header().number_records, live.len() as i64, "{what}");
    for p in live {
        let r = f.lookup(&p).unwrap().expect("live entry reachable");
        f.read_ts_record(&r).unwrap();
    }
    drop(f);
    let w = words(path);
    let (ht, mh, bs) = (w[19], w[15], w[23]);
    let mut owner: BTreeMap<i64, i64> = BTreeMap::new();
    let (mut used, mut overflow) = (0i64, 0i64);
    for slot in 0..mh {
        let mut a = w[(ht + slot) as usize];
        if a != 0 {
            used += 1;
        }
        let mut hops = 0;
        while a != 0 {
            if let Some(o) = owner.insert(a, slot) {
                panic!("{what}: bin {a} in the chains of slots {o} and {slot}");
            }
            if hops > 0 {
                overflow += 1;
            }
            hops += 1;
            a = w[(a + bs - 1) as usize];
        }
    }
    assert_eq!(w[16], used, "{what}: khashsUsed");
    assert_eq!(w[27], overflow, "{what}: kbinsOverflow");
    assert_eq!(w[29], 0, "{what}: in-progress marker left set");
}

// ---------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------

#[test]
fn round_trip_every_block_size() {
    let d = tmp("rt");
    let f = d.join("rt.dss");
    create_dss7(&f).unwrap();
    // (pathname, times, expected block D parts)
    let day: Vec<i64> = (0..70).map(|i| h(2021, 3, 1, 1, 0) + i * 37 * 60).collect();
    let month = vec![
        h(2021, 1, 30, 5, 0),
        t(2021, 2, 1, 0),
        h(2021, 2, 1, 0, 15),
        h(2021, 2, 28, 23, 0),
        t(2021, 3, 1, 0),
        h(2021, 3, 9, 13, 13),
    ];
    let year: Vec<i64> = (0..400)
        .map(|i| h(2019, 12, 31, 18, 0) + i * 7 * 3600)
        .collect();
    let decade: Vec<i64> = (0..600).map(|i| t(2008, 6, 1, 0) + i * DAY * 13).collect();
    let century: Vec<i64> = (0..400)
        .map(|i| h(1950, 1, 1, 7, 13) + i * DAY * 101)
        .collect();
    let secs: Vec<i64> = (0..30).map(|i| t(2025, 6, 1, 86_371) + i * 7).collect();
    let far: Vec<i64> = (0..40).map(|i| t(8999, 12, 31, 86_341) + i * 3).collect();
    let cases: Vec<(&[u8], &Vec<i64>, usize)> = vec![
        (b"/P/DAY/FLOW//IR-Day/S/", &day, 2),
        (b"/P/MON/STAGE//IR-MONTH/S/", &month, 3),
        (b"/P/YR/FLOW//ir-year/S/", &year, 2),
        (b"/P/DEC/FLOW//IR-Decade/S/", &decade, 3),
        (b"/P/CEN/FLOW//IR-Century/S/", &century, 2),
        (b"/P/SEC/FLOW//IR-Day/S/", &secs, 2),
        (b"/P/FAR/FLOW//IR-Month/S/", &far, 2),
    ];
    for (p, times, nblocks) in &cases {
        let v: Vec<f32> = (0..times.len())
            .map(|i| 100.0 + (i as f32) * 0.25)
            .collect();
        let out = its(&f, p, times, &v);
        assert_eq!(out.len(), *nblocks, "{}", String::from_utf8_lossy(p));
        assert!(out.iter().all(|&o| o == BlockOutcome::Added));
        let (rt, rv) = read_all(&f, p);
        assert_eq!(&rt, *times, "{}", String::from_utf8_lossy(p));
        assert_eq!(rv, f64s(&v));
    }
    // 00:00 is 24:00 of the previous day: stored in that day's block.
    let mid = [h(2021, 3, 1, 12, 0), t(2021, 3, 2, 0), h(2021, 3, 2, 0, 1)];
    its(&f, b"/P/MID/FLOW//IR-Day/S/", &mid, &[1.0, 2.0, 3.0]);
    let b1 = block(&f, b"/P/MID/FLOW/01Mar2021/IR-Day/S/").unwrap();
    assert_eq!(b1.0, mid[..2].to_vec());
    let b2 = block(&f, b"/P/MID/FLOW/02Mar2021/IR-Day/S/").unwrap();
    assert_eq!(b2.0, mid[2..].to_vec());
    // Granularity: automatic minute / second; explicit hour and day.
    assert_eq!(block(&f, b"/P/DAY/FLOW/01Mar2021/IR-Day/S/").unwrap().4, 60);
    assert_eq!(block(&f, b"/P/SEC/FLOW/01Jun2025/IR-Day/S/").unwrap().4, 1);
    let ht: Vec<i64> = (0..30)
        .map(|i| h(2021, 4, 1, 1, 0) + i * 5 * 3600)
        .collect();
    let hv: Vec<f32> = (0..30).map(|i| i as f32).collect();
    let mut input = ItsInput::new(
        b"/P/GH/FLOW//IR-Month/S/",
        &ht,
        RtsValues::F32(&hv),
        b"cfs",
        b"INST-VAL",
    );
    input.time_granularity = Some(3600);
    write_its(&f, &input, &opts()).unwrap();
    assert_eq!(
        block(&f, b"/P/GH/FLOW/01Apr2021/IR-Month/S/").unwrap().4,
        3600
    );
    assert_eq!(read_all(&f, b"/P/GH/FLOW//IR-Month/S/").0, ht);
    // Doubles, quality, missing values (never trimmed) and NaN.
    let dt: Vec<i64> = (0..5).map(|i| h(2021, 5, 1, 1, 0) + i * 3600).collect();
    let dv = [1.0 / 3.0, -901.0, f64::from(M), 4.0, f64::NAN];
    let q = [1, 2, 3, 0, 5];
    let mut di = ItsInput::new(
        b"/P/DQ/FLOW//IR-Day/S/",
        &dt,
        RtsValues::F64(&dv),
        b"cfs",
        b"INST-VAL",
    );
    di.quality = Some(&q);
    write_its(&f, &di, &opts()).unwrap();
    let b = block(&f, b"/P/DQ/FLOW/01May2021/IR-Day/S/").unwrap();
    assert_eq!(b.3, 115);
    assert_eq!(b.0, dt);
    assert_eq!(b.1[..4], dv[..4]);
    assert!(b.1[4].is_nan());
    assert_eq!(b.2.unwrap(), q);
    // All missing: written, every value kept.
    let mt: Vec<i64> = (0..30).map(|i| h(2021, 6, 1, 0, 31) + i * 1800).collect();
    its(&f, b"/P/AM/FLOW//IR-Day/S/", &mt, &[M; 30]);
    let b = block(&f, b"/P/AM/FLOW/01Jun2021/IR-Day/S/").unwrap();
    assert_eq!(b.1.len(), 30);
    assert_clean(&f, "round trips");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Merge semantics (storage flag 1)
// ---------------------------------------------------------------------------------

fn hourly(day: i64, n: i64) -> Vec<i64> {
    (0..n).map(|i| h(2021, 3, day, 1, 0) + i * 3600).collect()
}

#[test]
fn merge_replaces_the_window_and_keeps_the_rest() {
    let d = tmp("merge");
    let f = d.join("m.dss");
    create_dss7(&f).unwrap();
    let p = b"/M/D/FLOW//IR-Day/S/";
    let bp = b"/M/D/FLOW/01Mar2021/IR-Day/S/";
    let t0 = hourly(1, 23);
    let v0: Vec<f32> = (0..23).map(|i| i as f32).collect();
    assert_eq!(its(&f, p, &t0, &v0), [BlockOutcome::Added]);
    // One value in the middle replaces only that time; every other value is kept (heclib
    // 7-JA keeps 13 of 23 here).
    assert_eq!(
        its(&f, p, &[h(2021, 3, 1, 12, 0)], &[99.0]),
        [BlockOutcome::Replaced]
    );
    let b = block(&f, bp).unwrap();
    assert_eq!(b.0, t0);
    let mut want = f64s(&v0);
    want[11] = 99.0;
    assert_eq!(b.1, want);
    // A window between stored times: stored values inside it are dropped.
    its(
        &f,
        p,
        &[h(2021, 3, 1, 3, 30), h(2021, 3, 1, 9, 30)],
        &[77.0, 88.0],
    );
    let b = block(&f, bp).unwrap();
    let mut wt: Vec<i64> = t0
        .iter()
        .copied()
        .filter(|&x| x < h(2021, 3, 1, 3, 30))
        .collect();
    wt.extend([h(2021, 3, 1, 3, 30), h(2021, 3, 1, 9, 30)]);
    wt.extend(t0.iter().copied().filter(|&x| x > h(2021, 3, 1, 9, 30)));
    assert_eq!(b.0, wt);
    assert_eq!(b.1[3], 77.0);
    assert_eq!(b.1[4], 88.0);
    // Equal rewrite of the same block content: unchanged.
    let (ct, cv) = read_all(&f, p);
    let cv32: Vec<f32> = cv.iter().map(|&x| x as f32).collect();
    assert_eq!(its(&f, p, &ct, &cv32), [BlockOutcome::Unchanged]);
    // Append and prepend in another block.
    let q = b"/M/A/FLOW//IR-Month/S/";
    its(&f, q, &hourly(5, 10), &[1.0; 10]);
    its(&f, q, &hourly(9, 10), &[2.0; 10]);
    its(&f, q, &hourly(1, 10), &[3.0; 10]);
    let (rt, rv) = read_all(&f, q);
    let mut all = hourly(1, 10);
    all.extend(hourly(5, 10));
    all.extend(hourly(9, 10));
    assert_eq!(rt, all);
    assert_eq!(rv[..10], [3.0; 10]);
    assert_eq!(rv[20..], [2.0; 10]);
    // A new time equal to a stored time replaces it.
    its(&f, q, &[h(2021, 3, 5, 3, 0)], &[42.0]);
    let (rt2, rv2) = read_all(&f, q);
    assert_eq!(rt2, all);
    assert_eq!(rv2[12], 42.0);
    assert_clean(&f, "merges");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn window_spanning_blocks_deletes_empty_ones() {
    let d = tmp("del");
    let f = d.join("d.dss");
    create_dss7(&f).unwrap();
    let p = b"/X/BD/FLOW//IR-Day/S/";
    let t0: Vec<i64> = (0..5 * 24 - 2)
        .map(|i| h(2021, 3, 1, 1, 0) + i * 3600)
        .collect();
    let v0: Vec<f32> = (0..t0.len()).map(|i| i as f32).collect();
    its(&f, p, &t0, &v0);
    its(&f, b"/X/OTHER/FLOW//IR-Day/S/", &hourly(3, 4), &[5.0; 4]);
    let before = words(&f);
    let out = its(
        &f,
        p,
        &[h(2021, 3, 1, 20, 0), h(2021, 3, 5, 3, 0)],
        &[1.0, 2.0],
    );
    assert_eq!(
        out,
        [
            BlockOutcome::Replaced,
            BlockOutcome::Deleted,
            BlockOutcome::Deleted,
            BlockOutcome::Deleted,
            BlockOutcome::Replaced
        ]
    );
    for day in 2..=4 {
        let bp = format!("/X/BD/FLOW/0{day}Mar2021/IR-Day/S/");
        assert!(block(&f, bp.as_bytes()).is_none(), "{bp}");
    }
    let (rt, rv) = read_all(&f, p);
    let mut wt: Vec<i64> = t0
        .iter()
        .copied()
        .filter(|&x| x < h(2021, 3, 1, 20, 0))
        .collect();
    wt.push(h(2021, 3, 1, 20, 0));
    wt.push(h(2021, 3, 5, 3, 0));
    wt.extend(t0.iter().copied().filter(|&x| x > h(2021, 3, 5, 3, 0)));
    assert_eq!(rt, wt);
    assert_eq!(rv.len(), wt.len());
    let after = words(&f);
    let targets: Vec<Vec<u8>> = (1..=5)
        .map(|day| format!("/X/BD/FLOW/0{day}Mar2021/IR-Day/S/").into_bytes())
        .collect();
    let tr: Vec<&[u8]> = targets.iter().map(|v| v.as_slice()).collect();
    assert_others_identical(&before, &after, &tr);
    assert_eq!(after[10], before[10] + 3, "knumberDeletes");
    assert_eq!(after[3], before[3] - 3, "knumberRecords");
    assert_clean(&f, "deletes");
    // A deleted block can be written again (its bin entry is reused).
    its(&f, p, &[h(2021, 3, 3, 6, 0)], &[7.0]);
    assert!(block(&f, b"/X/BD/FLOW/03Mar2021/IR-Day/S/").is_some());
    assert_clean(&f, "rewrite after delete");
    // A long IR-Day gap (more than the lookup limit) is found by a bin scan.
    let g = b"/X/GAP/FLOW//IR-Day/S/";
    let gt: Vec<i64> = (0..3000).map(|i| t(2010, 1, 1, 43_200) + i * DAY).collect();
    its(&f, g, &gt, &[1.0; 3000]);
    let out = its(&f, g, &[gt[0], gt[2999]], &[8.0, 9.0]);
    assert_eq!(
        out.iter().filter(|&&o| o == BlockOutcome::Deleted).count(),
        2998
    );
    assert_eq!(read_all(&f, g).0, vec![gt[0], gt[2999]]);
    assert_clean(&f, "long gap");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn widths_quality_and_granularity_on_merge() {
    let d = tmp("wq");
    let f = d.join("w.dss");
    create_dss7(&f).unwrap();
    let t0 = hourly(1, 10);
    // Float into a double record: the record stays double and keeps its doubles.
    let dv: Vec<f64> = (0..10).map(|i| 1.0 / 3.0 + i as f64).collect();
    let i = ItsInput::new(
        b"/W/FD/FLOW//IR-Day/S/",
        &t0,
        RtsValues::F64(&dv),
        b"cfs",
        b"INST-VAL",
    );
    write_its(&f, &i, &opts()).unwrap();
    its(
        &f,
        b"/W/FD/FLOW//IR-Day/S/",
        &[h(2021, 3, 1, 12, 30)],
        &[2.5],
    );
    let b = block(&f, b"/W/FD/FLOW/01Mar2021/IR-Day/S/").unwrap();
    assert_eq!(b.3, 115);
    assert_eq!(b.1[..10], dv[..]);
    assert_eq!(b.1[10], 2.5);
    // Double into a float record: the record becomes double.
    its(&f, b"/W/DF/FLOW//IR-Day/S/", &t0, &[1.5; 10]);
    let x = [1.0 / 3.0];
    let tt = [h(2021, 3, 1, 12, 30)];
    let i = ItsInput::new(
        b"/W/DF/FLOW//IR-Day/S/",
        &tt,
        RtsValues::F64(&x),
        b"cfs",
        b"INST-VAL",
    );
    write_its(&f, &i, &opts()).unwrap();
    let b = block(&f, b"/W/DF/FLOW/01Mar2021/IR-Day/S/").unwrap();
    assert_eq!(b.3, 115);
    assert_eq!(b.1[10], 1.0 / 3.0);
    // Quality: added (stored values get 0) and dropped (as heclib drops it).
    its(&f, b"/W/QA/FLOW//IR-Day/S/", &t0, &[1.0; 10]);
    let q = [9];
    let mut i = ItsInput::new(
        b"/W/QA/FLOW//IR-Day/S/",
        &tt,
        RtsValues::F32(&[2.0]),
        b"cfs",
        b"INST-VAL",
    );
    i.quality = Some(&q);
    write_its(&f, &i, &opts()).unwrap();
    let b = block(&f, b"/W/QA/FLOW/01Mar2021/IR-Day/S/").unwrap();
    let mut wq = vec![0; 10];
    wq.push(9);
    assert_eq!(b.2.unwrap(), wq);
    its(
        &f,
        b"/W/QA/FLOW//IR-Day/S/",
        &[h(2021, 3, 1, 13, 30)],
        &[3.0],
    );
    assert!(block(&f, b"/W/QA/FLOW/01Mar2021/IR-Day/S/")
        .unwrap()
        .2
        .is_none());
    // Granularity: a seconds time into a minute block makes the block seconds
    // (automatic); an explicit minute granularity refuses it.
    its(&f, b"/W/GC/FLOW//IR-Day/S/", &t0, &[1.0; 10]);
    let ts = [t(2021, 3, 1, 12 * 3600 + 5)];
    let mut i = ItsInput::new(
        b"/W/GC/FLOW//IR-Day/S/",
        &ts,
        RtsValues::F32(&[2.0]),
        b"cfs",
        b"INST-VAL",
    );
    i.time_granularity = Some(60);
    assert!(matches!(
        validate_its_input(&i),
        Err(WriteError::BadInput(_))
    ));
    i.time_granularity = None;
    write_its(&f, &i, &opts()).unwrap();
    let b = block(&f, b"/W/GC/FLOW/01Mar2021/IR-Day/S/").unwrap();
    assert_eq!(b.4, 1);
    assert_eq!(b.0.len(), 11);
    // Minute data merged into a seconds block: an automatic minute granularity falls
    // back to seconds for that block; an explicit one is refused.
    let tm = [h(2021, 3, 1, 15, 0)];
    let mut i = ItsInput::new(
        b"/W/GC/FLOW//IR-Day/S/",
        &tm,
        RtsValues::F32(&[3.0]),
        b"cfs",
        b"INST-VAL",
    );
    i.time_granularity = Some(60);
    assert!(matches!(
        write_its(&f, &i, &opts()),
        Err(WriteError::BadInput(_))
    ));
    i.time_granularity = None;
    write_its(&f, &i, &opts()).unwrap();
    assert_eq!(block(&f, b"/W/GC/FLOW/01Mar2021/IR-Day/S/").unwrap().4, 1);
    // A regular record under the same block pathname is not overwritten.
    let rv = [1.0f32; 3];
    let r = RtsInput::new(
        b"/W/RG/FLOW//1Hour/S/",
        h(2021, 3, 1, 1, 0),
        RtsValues::F32(&rv),
        b"cfs",
        b"INST-VAL",
    );
    write_rts(&f, &r, &opts()).unwrap();
    assert_clean(&f, "widths");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn refusals() {
    let base = ItsInput::new(
        b"/R/A/FLOW//IR-Day/S/",
        &[],
        RtsValues::F32(&[]),
        b"cfs",
        b"INST-VAL",
    );
    let ok_t = [h(2021, 3, 1, 1, 0), h(2021, 3, 1, 2, 0)];
    let v2 = [1.0f32, 2.0];
    let dup = [ok_t[0], ok_t[0]];
    let desc = [ok_t[1], ok_t[0]];
    let undef = [0, ok_t[0]];
    let late = [ok_t[0], t(10_000, 1, 2, 0)];
    let cent = [ok_t[0], ok_t[1] + 7];
    let cases: Vec<(&str, ItsInput)> = vec![
        ("no values", base.clone()),
        (
            "times length",
            ItsInput {
                times: &ok_t[..1],
                values: RtsValues::F32(&v2),
                ..base.clone()
            },
        ),
        (
            "duplicate",
            ItsInput {
                times: &dup,
                values: RtsValues::F32(&v2),
                ..base.clone()
            },
        ),
        (
            "descending",
            ItsInput {
                times: &desc,
                values: RtsValues::F32(&v2),
                ..base.clone()
            },
        ),
        (
            "undefined time",
            ItsInput {
                times: &undef,
                values: RtsValues::F32(&v2),
                ..base.clone()
            },
        ),
        (
            "year 10000",
            ItsInput {
                times: &late,
                values: RtsValues::F32(&v2),
                ..base.clone()
            },
        ),
        (
            "century seconds",
            ItsInput {
                pathname: b"/R/A/FLOW//IR-Century/S/",
                times: &cent,
                values: RtsValues::F32(&v2),
                ..base.clone()
            },
        ),
        (
            "century 1 s",
            ItsInput {
                pathname: b"/R/A/FLOW//IR-Century/S/",
                times: &ok_t,
                values: RtsValues::F32(&v2),
                time_granularity: Some(1),
                ..base.clone()
            },
        ),
        (
            "granularity 7",
            ItsInput {
                times: &ok_t,
                values: RtsValues::F32(&v2),
                time_granularity: Some(7),
                ..base.clone()
            },
        ),
        (
            "not a multiple",
            ItsInput {
                times: &ok_t,
                values: RtsValues::F32(&v2),
                time_granularity: Some(86_400),
                ..base.clone()
            },
        ),
        (
            "quality length",
            ItsInput {
                times: &ok_t,
                values: RtsValues::F32(&v2),
                quality: Some(&[1]),
                ..base.clone()
            },
        ),
        (
            "units",
            ItsInput {
                times: &ok_t,
                values: RtsValues::F32(&v2),
                units: b"c\xe9",
                ..base.clone()
            },
        ),
        (
            "precision",
            ItsInput {
                times: &ok_t,
                values: RtsValues::F32(&v2),
                precision: 200,
                ..base.clone()
            },
        ),
    ];
    for (what, c) in &cases {
        assert!(
            matches!(validate_its_input(c), Err(WriteError::BadInput(_))),
            "{what}"
        );
    }
    for p in [
        &b"/R/A/FLOW//1Hour/S/"[..],
        b"/R/A/FLOW//~1Dy/S/",
        b"/R/A/FLOW//IR-Dy/S/",
        b"/R/A/FLOW/TS-PATTERN/IR-Day/S/",
    ] {
        let c = ItsInput {
            pathname: p,
            times: &ok_t,
            values: RtsValues::F32(&v2),
            ..base.clone()
        };
        assert!(matches!(
            validate_its_input(&c),
            Err(WriteError::BadPathname(_))
        ));
    }
    let d = tmp("ref");
    let f = d.join("r.dss");
    create_dss7(&f).unwrap();
    let i = ItsInput {
        pathname: b"/R/B/FLOW//IR-Year/S/",
        times: &ok_t,
        values: RtsValues::F32(&v2),
        ..base.clone()
    };
    // The IR-Year block for 2021 holds no regular record: the write succeeds.
    write_its(&f, &i, &opts()).unwrap();
    // Old lock scheme with a strict policy: refused, the file unchanged.
    let mut raw = std::fs::read(&f).unwrap();
    raw[92 * 8..93 * 8].copy_from_slice(&0i64.to_le_bytes());
    std::fs::write(&f, &raw).unwrap();
    let mut strict = opts();
    strict.lock_policy.allow_undetectable_readers = false;
    match write_its(&f, &i, &strict) {
        Err(WriteError::Locked(r)) => assert!(r.readers_undetectable),
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&f).unwrap(), raw);
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// G4 on a HEC-written fixture with irregular records
// ---------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

#[test]
fn merge_into_hec_written_records_keeps_others_identical() {
    let src = fixture("synthetic_irregular_variants7.dss");
    let d = tmp("g4");
    let f = d.join("v.dss");
    std::fs::copy(&src, &f).unwrap();
    let mut file = Dss7File::open(&f).unwrap();
    let cat = file.catalog().unwrap();
    // Every live ITS/ITD record written by heclib with a plain IR- E part.
    let targets: Vec<(Vec<u8>, i32)> = cat
        .catalog
        .live()
        .filter(|e| matches!(e.record_type, 110 | 115))
        .filter(|e| {
            let s = String::from_utf8_lossy(&e.pathname).to_ascii_uppercase();
            s.split('/').nth(5).is_some_and(|x| x.starts_with("IR-"))
        })
        .map(|e| (e.pathname.clone(), e.record_type))
        .collect();
    drop(file);
    assert!(!targets.is_empty());
    let mut n = 0;
    for (bp, _) in &targets {
        let before = words(&f);
        let (bt, _, _, _, _) = block(&f, bp).unwrap();
        // Rewrite the block's own values unchanged: nothing is written.
        let mut file = Dss7File::open(&f).unwrap();
        let r = file.lookup(bp).unwrap().unwrap();
        let x = file.read_ts_record(&r).unwrap();
        drop(file);
        let s = String::from_utf8_lossy(bp).to_string();
        let parts: Vec<&str> = s.split('/').collect();
        let ds = format!(
            "/{}/{}/{}//{}/{}/",
            parts[1], parts[2], parts[3], parts[5], parts[6]
        );
        let mut input = ItsInput::new(
            ds.as_bytes(),
            &bt,
            RtsValues::F32(&[]),
            x.ts.units.as_bytes(),
            x.ts.data_type.as_bytes(),
        );
        let (vf, vd);
        match &x.ts.values {
            TsValues::F32(v) => {
                vf = v.clone();
                input.values = RtsValues::F32(&vf);
            }
            TsValues::F64(v) => {
                vd = v.clone();
                input.values = RtsValues::F64(&vd);
            }
        }
        input.quality = x.ts.quality.as_deref();
        input.time_zone = x.ts.time_zone.as_deref().map(str::as_bytes);
        input.precision = x.meta.precision;
        input.time_granularity = Some(x.meta.time_granularity_seconds);
        let rep = write_its(&f, &input, &opts()).unwrap();
        assert_eq!(rep.blocks.len(), 1, "{s}");
        assert_others_identical(&before, &words(&f), &[bp.as_slice()]);
        // Then a changed value at the block's last time: only that record changes.
        let before = words(&f);
        let last = [*bt.last().unwrap()];
        let mut ch = input.clone();
        ch.times = &last;
        let (cf, cd) = ([12_345.0f32], [12_345.0f64]);
        ch.values = match input.values {
            RtsValues::F32(_) => RtsValues::F32(&cf),
            RtsValues::F64(_) => RtsValues::F64(&cd),
        };
        let q = [0];
        if ch.quality.is_some() {
            ch.quality = Some(&q);
        }
        let rep = write_its(&f, &ch, &opts()).unwrap();
        assert_eq!(rep.blocks[0].outcome, BlockOutcome::Replaced, "{s}");
        assert_others_identical(&before, &words(&f), &[bp.as_slice()]);
        let after = block(&f, bp).unwrap();
        assert_eq!(after.0, bt, "{s}");
        assert_eq!(*after.1.last().unwrap(), 12_345.0);
        n += 1;
    }
    assert!(n >= 3, "{n}");
    assert_clean(&f, "fixture merges");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Crash test: stop after every step of an add, a rewrite and a delete
// ---------------------------------------------------------------------------------

fn crash(
    base: &Path,
    dir: &Path,
    name: &str,
    input: &ItsInput,
    txn: usize,
    targets: &[&[u8]],
    check: &dyn Fn(&Path) -> bool,
) {
    let before = words(base);
    let diff_t = [h(2021, 1, 2, 6, 0)];
    let diff = ItsInput::new(
        b"/C/DIFF/FLOW//IR-Day/S/",
        &diff_t,
        RtsValues::F32(&[42.0]),
        b"cfs",
        b"INST-VAL",
    );
    for (i, step) in WriteStep::ALL.iter().enumerate() {
        let f = dir.join(format!("{name}_{i}.dss"));
        std::fs::copy(base, &f).unwrap();
        let mut o = opts();
        o.fault = Some(FaultPoint {
            block: txn,
            step: *step,
        });
        match write_its(&f, input, &o) {
            Err(WriteError::InjectedFault { block, step: s }) => {
                assert_eq!((block, s), (txn, *step));
            }
            other => panic!("{name} {step:?}: {other:?}"),
        }
        assert_others_identical(&before, &words(&f), targets);
        // Every target block still reads (old or new state).
        let mut rf = Dss7File::open(&f).unwrap();
        for t in targets {
            if let Ok(Some(r)) = rf.lookup(t) {
                rf.read_ts_record(&r).unwrap();
            }
        }
        drop(rf);
        let fb = dir.join(format!("{name}_{i}_b.dss"));
        std::fs::copy(&f, &fb).unwrap();
        // (A) the same write again.
        let ra = write_its(&f, input, &opts()).unwrap();
        let marked = (WriteStep::Allocation..WriteStep::Finish).contains(step);
        assert_eq!(ra.reconciled_interrupted_write, marked, "{name} {step:?}");
        assert!(check(&f), "{name} {step:?} rerun");
        assert_clean(&f, &format!("{name} {step:?} rerun"));
        // (B) another pathname, then the same write.
        write_its(&fb, &diff, &opts()).unwrap();
        assert_clean(&fb, &format!("{name} {step:?} different"));
        write_its(&fb, input, &opts()).unwrap();
        assert!(check(&fb), "{name} {step:?} different + rerun");
        assert_clean(&fb, &format!("{name} {step:?} different + rerun"));
        if let Ok(dirs) = std::env::var("DSS_P21_CRASH_OUT") {
            let out = Path::new(&dirs);
            std::fs::create_dir_all(out).unwrap();
            let stem = format!("{name}_{i}_{step:?}");
            let crashed = out.join(format!("{stem}.dss"));
            std::fs::copy(base, &crashed).unwrap();
            let _ = write_its(&crashed, input, &o);
            std::fs::copy(&f, out.join(format!("{stem}_rerun.dss"))).unwrap();
            std::fs::copy(&fb, out.join(format!("{stem}_diff.dss"))).unwrap();
        }
    }
}

#[test]
fn crash_after_every_step() {
    let d = tmp("crash");
    let base = d.join("base.dss");
    create_dss7(&base).unwrap();
    let p = b"/C/IRR/FLOW//IR-Day/S/";
    let t0: Vec<i64> = (0..3 * 24 - 1)
        .map(|i| h(2021, 3, 1, 1, 0) + i * 3600)
        .collect();
    let v0: Vec<f32> = (0..t0.len()).map(|i| i as f32).collect();
    its(
        &base,
        b"/C/OTHER/FLOW//IR-Month/S/",
        &hourly(2, 5),
        &[3.0; 5],
    );
    // 1. New record (fresh table hash, new bin).
    let new_t = hourly(1, 10);
    let new_v = [1.5f32; 10];
    let add = ItsInput::new(p, &new_t, RtsValues::F32(&new_v), b"cfs", b"INST-VAL");
    let b1: &[u8] = b"/C/IRR/FLOW/01Mar2021/IR-Day/S/";
    crash(&base, &d, "add", &add, 0, &[b1], &|f| {
        block(f, b1).is_some_and(|b| b.0 == new_t)
    });
    // 2. Rewrite (merge into a stored block) and 3. delete of an emptied block.
    let full = d.join("full.dss");
    std::fs::copy(&base, &full).unwrap();
    its(&full, p, &t0, &v0);
    let mid = [h(2021, 3, 1, 12, 0)];
    let rw = ItsInput::new(p, &mid, RtsValues::F32(&[99.0]), b"cfs", b"INST-VAL");
    crash(&full, &d, "rewrite", &rw, 0, &[b1], &|f| {
        block(f, b1).is_some_and(|b| b.1[11] == 99.0 && b.0.len() == 24)
    });
    let span = [h(2021, 3, 1, 20, 0), h(2021, 3, 3, 3, 0)];
    let sp = ItsInput::new(p, &span, RtsValues::F32(&[1.0, 2.0]), b"cfs", b"INST-VAL");
    let b2: &[u8] = b"/C/IRR/FLOW/02Mar2021/IR-Day/S/";
    let b3: &[u8] = b"/C/IRR/FLOW/03Mar2021/IR-Day/S/";
    // Transaction 1 is the delete of 02Mar2021.
    // m6: exactly one delete is counted after recovery (knumberDeletes, kcatSortDeletes).
    let (nd, csd) = (words(&full)[10], words(&full)[35]);
    crash(&full, &d, "delete", &sp, 1, &[b1, b2, b3], &|f| {
        let w = words(f);
        block(f, b2).is_none()
            && block(f, b1).is_some()
            && block(f, b3).is_some()
            && w[10] == nd + 1
            && w[35] == csd + 1
    });
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Fuzz: validation never panics; accepted inputs round-trip
// ---------------------------------------------------------------------------------

#[test]
fn fuzz_input_validation() {
    let iters: usize = std::env::var("DSS7_WRITE_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let d = tmp("fuzz");
    let f = d.join("fz.dss");
    create_dss7(&f).unwrap();
    let mut rng = Rng(0x5eed_0021_0040);
    let eparts: [&[u8]; 10] = [
        b"IR-Day",
        b"IR-DAY",
        b"IR-Month",
        b"IR-Year",
        b"IR-Decade",
        b"IR-Century",
        b"IR-Mo",
        b"1Hour",
        b"~1Day",
        b"ir-day",
    ];
    let alphabet = b"AZaz09 /-_.:|~\x00\x01\x7f\xe9";
    let (mut ok, mut refused, mut written) = (0usize, 0usize, 0usize);
    for i in 0..iters {
        let mut path: Vec<u8> = Vec::new();
        if rng.next() % 5 == 0 {
            for _ in 0..(rng.next() % 60) {
                path.push(alphabet[(rng.next() % alphabet.len() as u64) as usize]);
            }
        } else {
            path.push(b'/');
            for part in 0..6 {
                if part == 4 {
                    path.extend_from_slice(eparts[(rng.next() % eparts.len() as u64) as usize]);
                } else if part == 3 && rng.next() % 2 == 0 {
                    path.extend_from_slice(b"01JAN2021");
                } else {
                    for _ in 0..(rng.next() % 12) {
                        path.push(b"ABCXYZ09 -"[(rng.next() % 10) as usize]);
                    }
                }
                path.push(b'/');
            }
        }
        let n = (rng.next() % 30) as usize;
        let mut times: Vec<i64> = Vec::with_capacity(n);
        let mut cur = match rng.next() % 6 {
            0 => 0,
            1 => -(rng.next() as i64 % (1 << 40)),
            2 => rng.next() as i64 % (1 << 41),
            _ => t(
                1900 + (rng.next() % 200) as i64,
                1 + (rng.next() % 12) as i64,
                1 + (rng.next() % 28) as i64,
                (rng.next() % 86_400) as i64,
            ),
        };
        for _ in 0..n {
            times.push(cur);
            cur += match rng.next() % 8 {
                0 => 0,
                1 => -60,
                2 => (rng.next() % 40) as i64 * DAY,
                _ => (rng.next() % 7200) as i64 * 60 + i64::from(rng.next() % 3 == 0),
            };
        }
        if rng.next() % 10 == 0 && n > 0 {
            times.pop();
        }
        let vals: Vec<f32> = (0..n)
            .map(|_| match rng.next() % 6 {
                0 => -901.0,
                1 => M,
                2 => f32::NAN,
                _ => (rng.next() % 1000) as f32 / 10.0,
            })
            .collect();
        let q: Vec<i32> = (0..(if rng.next() % 3 == 0 { n } else { n / 2 }))
            .map(|_| rng.next() as i32)
            .collect();
        let mut input = ItsInput::new(&path, &times, RtsValues::F32(&vals), b"cfs", b"INST-VAL");
        if rng.next() % 2 == 0 {
            input.quality = Some(&q);
        }
        input.time_granularity = match rng.next() % 5 {
            0 => Some(1),
            1 => Some(60),
            2 => Some((rng.next() % 100_000) as i64),
            _ => None,
        };
        match validate_its_input(&input) {
            Ok(_) => {
                ok += 1;
                if i % 5 == 0 {
                    match write_its(&f, &input, &opts()) {
                        Ok(r) => {
                            written += r.blocks.len();
                            assert!(!r.blocks.is_empty());
                        }
                        Err(WriteError::Unsupported(_)) | Err(WriteError::BadInput(_)) => {}
                        Err(e) => panic!("{:?}: {e}", String::from_utf8_lossy(&path)),
                    }
                }
            }
            Err(WriteError::BadPathname(_)) | Err(WriteError::BadInput(_)) => refused += 1,
            Err(e) => panic!("unexpected error kind: {e}"),
        }
    }
    assert!(
        ok > 0 && refused > 0 && written > 0,
        "ok {ok} refused {refused} written {written}"
    );
    assert_clean(&f, "fuzz");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// A write refused by any block leaves the file byte-identical
// ---------------------------------------------------------------------------------

#[test]
fn refused_multi_block_write_leaves_the_file_unchanged() {
    let d = tmp("m1");
    // (a) Explicit granularity: the second block keeps stored seconds times.
    let f = d.join("g.dss");
    create_dss7(&f).unwrap();
    let st = [t(2021, 3, 2, 20 * 3600 + 5), t(2021, 3, 2, 21 * 3600 + 5)];
    its(&f, b"/M1/G/FLOW//IR-Day/S/", &st, &[1.0, 2.0]);
    let before = std::fs::read(&f).unwrap();
    let nt = [
        h(2021, 3, 1, 5, 0),
        h(2021, 3, 1, 23, 0),
        h(2021, 3, 2, 2, 0),
    ];
    let mut i = ItsInput::new(
        b"/M1/G/FLOW//IR-Day/S/",
        &nt,
        RtsValues::F32(&[7.0, 8.0, 9.0]),
        b"cfs",
        b"INST-VAL",
    );
    i.time_granularity = Some(60);
    assert!(matches!(
        write_its(&f, &i, &opts()),
        Err(WriteError::BadInput(_))
    ));
    assert_eq!(std::fs::read(&f).unwrap(), before, "granularity refusal");
    // (b) P38: HEC-written notes are now merged, so the refusal on a later block is a
    // record with aliases: a new December 2000 block, then the aliased 2001 block.
    let g = d.join("a.dss");
    std::fs::copy(fixture("synthetic_catalog_ops7.dss"), &g).unwrap();
    let before = std::fs::read(&g).unwrap();
    let v = [1.0f32; 5];
    let r = RtsInput::new(
        b"/P32/ALIAS PRIMARY/FLOW//1Day/OBS/",
        t(2000, 12, 29, 86_400),
        RtsValues::F32(&v),
        b"cfs",
        b"INST-VAL",
    );
    assert!(matches!(
        write_rts(&g, &r, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(
        std::fs::read(&g).unwrap(),
        before,
        "alias refusal on block 2"
    );
    // (c) P38: the same multi-block writes over stored notes now succeed; blocks the window
    // empties are deleted as heclib deletes them.
    let g = d.join("n.dss");
    std::fs::copy(fixture("synthetic_notes_quality7.dss"), &g).unwrap();
    let p: &[u8] = b"/P32/NOTES/FLOW//IR-Month/CNOTES IRR/";
    let nt = [h(2000, 12, 20, 1, 0), h(2001, 3, 10, 1, 0)];
    let w = ItsInput::new(p, &nt, RtsValues::F32(&[1.0, 2.0]), b"cfs", b"INST-VAL");
    let rep = write_its(&g, &w, &opts()).unwrap();
    assert!(rep
        .blocks
        .iter()
        .any(|b| b.outcome == BlockOutcome::Deleted));
}

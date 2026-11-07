//! P20 writer tests: create → add → replace → read round trips, word-level byte identity
//! of unrelated records (gate G4), a crash test that stops the write after every step of
//! the transaction (P20 §4), and a fuzz pass on input validation. Synthetic data only;
//! every write goes to a new file or a temporary copy of a public fixture.
//!
//! HEC oracle checks of the same writer (hecdss, Monolith, heclib `zcheckFile`, HEC-written
//! word comparisons) are local-only: `evidence/P20/REPORT.md`.

mod dss7_common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss7_common::{bald_eagle, Rng};
use dss_core::dss7::interval::ymd_to_julian;
use dss_core::dss7::write::{
    create_dss7, validate_rts_input, write_rts, BlockOutcome, FaultPoint, RtsInput, RtsValues,
    WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::{CatalogOptions, Dss7File, TsReadOptions};
use dss_core::TsValues;

const DAY: i64 = 86_400;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dss7_write_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn t(y: i64, m: i64, d: i64, secs: i64) -> i64 {
    ymd_to_julian(y, m, d) * DAY + secs
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: false,
        ..WriteOptions::default()
    }
}

fn write(path: &Path, p: &[u8], start: i64, v: RtsValues<'_>) -> Vec<BlockOutcome> {
    let input = RtsInput::new(p, start, v, b"cfs", b"INST-VAL");
    write_rts(path, &input, &opts())
        .unwrap()
        .blocks
        .iter()
        .map(|b| b.outcome)
        .collect()
}

fn read_all(path: &Path, p: &[u8]) -> dss_core::dss7::Dss7Ts {
    let mut f = Dss7File::open(path).unwrap();
    f.read_ts(
        p,
        &TsReadOptions {
            window: None,
            trim_missing: true,
        },
    )
    .unwrap()
}

fn f32s(v: &TsValues) -> Vec<f32> {
    match v {
        TsValues::F32(x) => x.clone(),
        TsValues::F64(x) => panic!("expected floats, got {} doubles", x.len()),
    }
}

// ---------------------------------------------------------------------------------
// Raw-word snapshot, independent of the writer: header, and per live entry the bin
// entry words and the info + area words.
// ---------------------------------------------------------------------------------

fn words(path: &Path) -> Vec<i64> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// pathname (upper) -> (bin entry address, info words + area words).
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

/// G4: every record except `target` is word-identical before and after.
fn assert_others_identical(before: &[i64], after: &[i64], target: &[u8]) {
    let (rb, ra) = (records(before), records(after));
    let t = target.to_ascii_uppercase();
    for (k, v) in &rb {
        if *k != t {
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
            *k == t || rb.contains_key(k),
            "unexpected record {}",
            String::from_utf8_lossy(k)
        );
    }
}

// ---------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------

#[test]
fn create_then_round_trip_intervals() {
    let d = tmp("rt");
    let f = d.join("rt.dss");
    create_dss7(&f).unwrap();
    assert!(matches!(create_dss7(&f), Err(WriteError::AlreadyExists(_))));
    let cases: &[(&[u8], i64, usize, usize)] = &[
        (
            b"/RT/A/FLOW//1Minute/S/",
            t(2021, 3, 1, 23 * 3600 + 60),
            1500,
            2,
        ),
        (b"/RT/B/FLOW//15Minute/S/", t(2021, 1, 30, 900), 400, 2),
        (b"/RT/C/FLOW//1Hour/S/", t(2020, 12, 30, 3600), 120, 2),
        (b"/RT/D/FLOW//1Day/S/", t(2019, 12, 16, 0), 800, 4),
        (b"/RT/E/FLOW//1Month/S/", t(2018, 2, 1, 0), 40, 2),
        (
            b"/RT/F/FLOW//15Second/S/",
            t(2025, 6, 1, 23 * 3600 + 58 * 60),
            30,
            2,
        ),
        (b"/RT/G/FLOW//1Year/S/", t(1991, 1, 1, 0), 30, 2),
        (b"/RT/H/FLOW//1Day/S/", t(2199, 1, 2, 0), 30, 1),
    ];
    for &(p, start, n, blocks) in cases {
        let v: Vec<f32> = (0..n)
            .map(|i| 100.0 + (i as f32 / 7.0).sin() * 50.0)
            .collect();
        let out = write(&f, p, start, RtsValues::F32(&v));
        assert_eq!(
            out,
            vec![BlockOutcome::Added; blocks],
            "{}",
            String::from_utf8_lossy(p)
        );
        let r = read_all(&f, p);
        assert_eq!(f32s(&r.ts.values), v, "{}", String::from_utf8_lossy(p));
        assert_eq!(r.ts.times[0], start, "{}", String::from_utf8_lossy(p));
        let expect = dss_core::dss7::write::value_times(p, start, n).unwrap();
        assert_eq!(r.ts.times, expect);
    }
    // Doubles, quality, offsets.
    let p = b"/RT/Q/FLOW//1Hour/S/";
    let v: Vec<f64> = (0..50).map(|i| f64::from(i) / 3.0).collect();
    let q: Vec<i32> = (0..50).map(|i| if i % 5 == 0 { 5 } else { 3 }).collect();
    let mut input = RtsInput::new(
        p,
        t(2021, 2, 1, 1800),
        RtsValues::F64(&v),
        b"ft",
        b"INST-VAL",
    );
    input.quality = Some(&q);
    write_rts(&f, &input, &opts()).unwrap();
    let r = read_all(&f, p);
    let TsValues::F64(got) = &r.ts.values else {
        panic!()
    };
    assert_eq!(&got[..50], &v[..]);
    assert_eq!(&r.ts.quality.as_ref().unwrap()[..50], &q[..]);
    assert_eq!(r.meta.time_offset_seconds, 1800);
    assert_eq!(r.ts.times[0], t(2021, 2, 1, 1800));
    // The file opens and every entry is consistent for a recovery-mode catalog walk.
    let mut file = Dss7File::open(&f).unwrap();
    let cat = file.catalog_with(&CatalogOptions::default()).unwrap();
    assert!(
        cat.catalog.anomalies.is_empty(),
        "{:?}",
        cat.catalog.anomalies
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn replace_merge_unchanged_and_all_missing() {
    let d = tmp("rep");
    let f = d.join("r.dss");
    create_dss7(&f).unwrap();
    let p = b"/R/M/FLOW//1Hour/S/";
    let base: Vec<f32> = (0..100).map(|i| i as f32).collect();
    let s0 = t(2021, 3, 1, 3600);
    assert_eq!(
        write(&f, p, s0, RtsValues::F32(&base)),
        vec![BlockOutcome::Added]
    );
    let w0 = words(&f);
    assert_eq!(
        write(&f, p, s0, RtsValues::F32(&base)),
        vec![BlockOutcome::Unchanged]
    );
    assert_eq!(words(&f), w0, "an equal rewrite must not touch the file");
    assert_eq!(
        write(
            &f,
            p,
            t(2021, 3, 2, 5 * 3600),
            RtsValues::F32(&[9.0, 8.0, 7.0])
        ),
        vec![BlockOutcome::Replaced]
    );
    let mut expect = base.clone();
    expect[28..31].copy_from_slice(&[9.0, 8.0, 7.0]);
    assert_eq!(f32s(&read_all(&f, p).ts.values), expect);
    // All missing over the whole stored range: HEC (copyEmptyRecords = 1) keeps one
    // missing value at position 0.
    let m = vec![-f32::MAX; 100];
    assert_eq!(
        write(&f, p, s0, RtsValues::F32(&m)),
        vec![BlockOutcome::Replaced]
    );
    let mut file = Dss7File::open(&f).unwrap();
    let rec = file
        .lookup(b"/R/M/FLOW/01Mar2021/1Hour/S/")
        .unwrap()
        .unwrap();
    assert_eq!(rec.info.number_data, 1);
    assert_eq!(rec.info.version, 3);
    let h = file.header().clone();
    assert!(h.dead_words > 0);
    assert_eq!(h.number_expansions, 2);
    assert_eq!(h.number_records, 1);
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// G4 on a public fixture copy
// ---------------------------------------------------------------------------------

#[test]
fn unrelated_records_are_byte_identical() {
    let Some(src) = bald_eagle() else { return };
    let d = tmp("g4");
    let f = d.join("be.dss");
    std::fs::copy(&src, &f).unwrap();
    let before = words(&f);
    let p = b"/P20/NEW/FLOW//1Hour/SYN/";
    let v: Vec<f32> = (0..48).map(|i| i as f32).collect();
    write(&f, p, t(2021, 1, 1, 3600), RtsValues::F32(&v));
    let after = words(&f);
    assert_others_identical(&before, &after, b"/P20/NEW/FLOW/01Jan2021/1Hour/SYN/");
    // Outside the header and the appended space only the table slot or a bin word changed.
    let n = before.len() - 1; // the old EOF word is reused
    let changed: Vec<usize> = (100..n).filter(|&i| before[i] != after[i]).collect();
    assert!(changed.len() <= 7 + 5 + 2, "{changed:?}");
    // Replace an existing BaldEagle record with changed values.
    let mut file = Dss7File::open(&f).unwrap();
    let cat = file.catalog().unwrap();
    let e = cat
        .catalog
        .live()
        .find(|e| e.record_type == 100)
        .expect("an RTS record")
        .pathname
        .clone();
    let rec = file.lookup(&e).unwrap().unwrap();
    let r = file.read_ts_record(&rec).unwrap();
    let start = r.ts.times[0];
    drop(file);
    let mut vals = f32s(&r.ts.values);
    for x in vals.iter_mut().take(5) {
        *x = 1234.5;
    }
    let before = words(&f);
    let input = RtsInput::new(&e, start, RtsValues::F32(&vals), b"cfs", b"FLOW");
    let rep = write_rts(&f, &input, &opts()).unwrap();
    assert_eq!(rep.blocks[0].outcome, BlockOutcome::Replaced);
    let after = words(&f);
    assert_others_identical(&before, &after, &e);
    let mut file = Dss7File::open(&f).unwrap();
    let rec2 = file.lookup(&e).unwrap().unwrap();
    assert_eq!(
        f32s(&file.read_ts_record(&rec2).unwrap().ts.values)[..5],
        [1234.5; 5]
    );
    // The old copy is MOVED, the new one is live.
    assert_eq!(after[rec.info_address as usize + 1], 10);
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Crash test: stop after each step, then (A) rerun the same write and (B) write a
// different pathname and then the same write. Both must leave a clean file.
// ---------------------------------------------------------------------------------

fn values_at(path: &Path, block_path: &[u8]) -> Option<Vec<f32>> {
    let mut f = Dss7File::open(path).unwrap();
    f.lookup(block_path)
        .unwrap()
        .map(|r| f32s(&f.read_ts_record(&r).unwrap().ts.values))
}

fn contains(got: &[f32], v: &[f32]) -> bool {
    got.windows(v.len()).any(|w| w == v)
}

/// The file has no catalog anomaly: no chain shares a bin with another, no table slot
/// points at an empty or unlinked bin, `knumberRecords` equals the live entries, and
/// every live record reads.
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
    assert_bin_counters(&words(path), what);
}

/// Bin bookkeeping from the raw words (dss7.md §2.2, §3.3-§3.5), independent of the
/// writer: `khashsUsed`, `kbinsOverflow`, `ktotalBins`, `kbinsRemainInBlock`, no bin
/// in two chains, every bin before the cursor in use, and the cursor bin empty.
fn assert_bin_counters(w: &[i64], what: &str) {
    let (ht, mh, bpb, bs, first, next) = (w[19], w[15], w[21], w[23], w[24], w[25]);
    let mut owner: BTreeMap<i64, i64> = BTreeMap::new();
    let (mut used, mut overflow) = (0i64, 0i64);
    for t in 0..mh {
        let mut a = w[(ht + t) as usize];
        if a != 0 {
            used += 1;
        }
        let mut hops = 0;
        while a != 0 {
            if let Some(o) = owner.insert(a, t) {
                panic!("{what}: bin {a} in the chains of slots {o} and {t}");
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
    // Walk the blocks to the cursor.
    let mut block = first;
    let mut handed = 0i64;
    loop {
        let end = block + bs * bpb;
        if (block..=end).contains(&next) {
            let k = (next - block) / bs;
            handed += k;
            assert_eq!(w[22], bpb - k, "{what}: kbinsRemainInBlock");
            for b in 0..k {
                assert!(
                    w[(block + b * bs) as usize] != 0,
                    "{what}: empty bin before the cursor"
                );
            }
            if k < bpb {
                assert!(
                    w[next as usize..(next + bs) as usize]
                        .iter()
                        .all(|&x| x == 0),
                    "{what}: cursor bin in use"
                );
            }
            break;
        }
        handed += bpb;
        block = w[end as usize];
        assert!(block != 0, "{what}: cursor outside the block chain");
    }
    assert_eq!(w[26], handed, "{what}: ktotalBins");
}

struct Case<'a> {
    name: &'a str,
    pathname: &'a [u8],
    block_path: &'a [u8],
    start: i64,
    values: &'a [f32],
    /// First step after which the new values are visible.
    visible_from: WriteStep,
}

fn crash_case(base: &Path, dir: &Path, c: &Case) {
    let before = words(base);
    let old = values_at(base, c.block_path);
    let input = RtsInput::new(
        c.pathname,
        c.start,
        RtsValues::F32(c.values),
        b"cfs",
        b"INST-VAL",
    );
    let diff_v = [42.0f32, 43.0];
    let diff = RtsInput::new(
        b"/C/DIFF/FLOW//1Day/S/",
        t(2021, 1, 2, 0),
        RtsValues::F32(&diff_v),
        b"cfs",
        b"INST-VAL",
    );
    // A pathname that hashes into the crashed record's chain.
    let chain_block = same_table_hash(c.block_path, 1)[0].clone();
    let chain_ds = dataset(&chain_block);
    let chain_v = [55.0f32];
    let chain_w = RtsInput::new(
        chain_ds.as_bytes(),
        t(2021, 1, 2, 0),
        RtsValues::F32(&chain_v),
        b"cfs",
        b"INST-VAL",
    );
    let out = std::env::var("DSS_P20_CRASH_OUT").ok();
    for (i, step) in WriteStep::ALL.iter().enumerate() {
        let f = dir.join(format!("{}_{i}.dss", c.name));
        std::fs::copy(base, &f).unwrap();
        let mut o = opts();
        o.fault = Some(FaultPoint {
            block: 0,
            step: *step,
        });
        match write_rts(&f, &input, &o) {
            Err(WriteError::InjectedFault { block: 0, step: s }) => assert_eq!(s, *step),
            other => panic!("{} {step:?}: {other:?}", c.name),
        }
        // The interrupted file: other records untouched, the record old or new.
        assert_others_identical(&before, &words(&f), c.block_path);
        let now = values_at(&f, c.block_path);
        if *step >= c.visible_from {
            assert!(
                contains(&now.expect("published record"), c.values),
                "{} {step:?}",
                c.name
            );
        } else {
            assert_eq!(
                now, old,
                "{} {step:?}: the previous state must be intact",
                c.name
            );
        }
        let fb = dir.join(format!("{}_{i}_b.dss", c.name));
        std::fs::copy(&f, &fb).unwrap();
        // (A) The same write again.
        let ra = write_rts(&f, &input, &opts()).unwrap();
        let marked = (WriteStep::Allocation..WriteStep::Finish).contains(step);
        assert_eq!(
            ra.reconciled_interrupted_write, marked,
            "{} {step:?}",
            c.name
        );
        assert!(contains(&values_at(&f, c.block_path).unwrap(), c.values));
        assert_clean(&f, &format!("{} {step:?} rerun", c.name));
        // (B) A different pathname (fresh table hash), one in the crashed record's
        // chain, then the same write.
        write_rts(&fb, &diff, &opts()).unwrap();
        assert_clean(&fb, &format!("{} {step:?} different pathname", c.name));
        write_rts(&fb, &chain_w, &opts()).unwrap();
        assert_clean(&fb, &format!("{} {step:?} same-chain pathname", c.name));
        write_rts(&fb, &input, &opts()).unwrap();
        assert_clean(&fb, &format!("{} {step:?} different + rerun", c.name));
        assert!(contains(&values_at(&fb, c.block_path).unwrap(), c.values));
        assert!(contains(
            &values_at(&fb, b"/C/DIFF/FLOW/01Jan2021/1Day/S/").unwrap(),
            &diff_v
        ));
        if let Some(dir) = &out {
            // Local oracle: interrupted, rerun and different-pathname files.
            std::fs::create_dir_all(dir).unwrap();
            let stem = format!("{}_{i}_{step:?}", c.name);
            let crashed = Path::new(dir).join(format!("{stem}.dss"));
            std::fs::copy(base, &crashed).unwrap();
            let _ = write_rts(&crashed, &input, &o);
            std::fs::copy(&f, Path::new(dir).join(format!("{stem}_rerun.dss"))).unwrap();
            std::fs::copy(&fb, Path::new(dir).join(format!("{stem}_diff.dss"))).unwrap();
            // HEC C writer input for the same interrupted file: a fresh table hash and a
            // pathname in the crashed record's chain (evidence/P20/scripts/p20hec.c).
            let spec = format!(
                "STORE float /C/HECFRESH/FLOW//1Day/S/ 01Jan2021 2400 cfs INST-VAL 2 0 0\n\
                 3f800000 40000000\n\
                 STORE float {chain_ds} 01Jan2021 2400 cfs INST-VAL 1 0 0\n40400000\n"
            );
            std::fs::write(Path::new(dir).join(format!("{stem}.hecspec")), spec).unwrap();
        }
    }
}

/// Block pathnames of `/C/H<n>/FLOW//1Day/S/` whose table hash equals `target`'s.
fn same_table_hash(target: &[u8], n: usize) -> Vec<String> {
    let th = dss_core::dss7::table_hash(target, 8192);
    (0u32..)
        .map(|i| format!("/C/H{i}/FLOW/01Jan2021/1Day/S/"))
        .filter(|p| p.as_bytes() != target && dss_core::dss7::table_hash(p.as_bytes(), 8192) == th)
        .take(n)
        .collect()
}

fn dataset(block: &str) -> String {
    block.replace("/01Jan2021/", "//")
}

#[test]
fn crash_after_every_step() {
    let d = tmp("crash");
    let v: Vec<f32> = (0..30).map(|i| 10.0 + i as f32).collect();
    // New record in a fresh table hash (new bin from the current block).
    let base = d.join("base.dss");
    create_dss7(&base).unwrap();
    write(
        &base,
        b"/C/OTHER/FLOW//1Day/S/",
        t(2021, 1, 2, 0),
        RtsValues::F32(&[1.0, 2.0]),
    );
    let fresh = Case {
        name: "new",
        pathname: b"/C/NEW/FLOW//1Hour/S/",
        block_path: b"/C/NEW/FLOW/01Mar2021/1Hour/S/",
        start: t(2021, 3, 1, 3600),
        values: &v,
        visible_from: WriteStep::Link,
    };
    crash_case(&base, &d, &fresh);
    // Rewrite (merge) of an existing record.
    write(
        &base,
        b"/C/NEW/FLOW//1Hour/S/",
        t(2021, 3, 1, 3600),
        RtsValues::F32(&[0.5; 60]),
    );
    let w: Vec<f32> = (0..5).map(|i| 77.0 + i as f32).collect();
    crash_case(
        &base,
        &d,
        &Case {
            name: "replace",
            start: t(2021, 3, 2, 3600),
            values: &w,
            visible_from: WriteStep::Publish,
            ..fresh
        },
    );
    // An entry added to an existing bin (same table hash as /C/OTHER).
    let other_block = b"/C/OTHER/FLOW/01Jan2021/1Day/S/";
    let same = same_table_hash(other_block, 40);
    let inbin = same[0].clone();
    let inbin_ds = dataset(&inbin);
    crash_case(
        &base,
        &d,
        &Case {
            name: "inbin",
            pathname: inbin_ds.as_bytes(),
            block_path: inbin.as_bytes(),
            start: t(2021, 1, 2, 0),
            values: &[5.0, 6.0],
            visible_from: WriteStep::Publish,
        },
    );
    // An overflow bin: fill the chain of that table hash until the next entry needs an
    // overflow bin (checked on a scratch copy), then crash on that entry.
    let ovf = d.join("ovf.dss");
    std::fs::copy(&base, &ovf).unwrap();
    let probe = d.join("ovf_probe.dss");
    let mut target = None;
    for p in &same[1..] {
        std::fs::copy(&ovf, &probe).unwrap();
        let before = Dss7File::open(&probe).unwrap().header().bins_overflow;
        write(
            &probe,
            dataset(p).as_bytes(),
            t(2021, 1, 2, 0),
            RtsValues::F32(&[8.0]),
        );
        if Dss7File::open(&probe).unwrap().header().bins_overflow > before {
            target = Some(p.clone());
            break;
        }
        write(
            &ovf,
            dataset(p).as_bytes(),
            t(2021, 1, 2, 0),
            RtsValues::F32(&[3.0]),
        );
    }
    let last = target.expect("an entry that needs an overflow bin");
    let last_ds = dataset(&last);
    crash_case(
        &ovf,
        &d,
        &Case {
            name: "overflow",
            pathname: last_ds.as_bytes(),
            block_path: last.as_bytes(),
            start: t(2021, 1, 2, 0),
            values: &[8.0],
            visible_from: WriteStep::Link,
        },
    );
    // A new bin block: fill the 32 bins of the first block, then crash in the 33rd.
    let full = d.join("full.dss");
    create_dss7(&full).unwrap();
    for i in 0..32 {
        let p = format!("/C/B{i:02}/FLOW//1Day/S/");
        write(
            &full,
            p.as_bytes(),
            t(2021, 1, 2, 0),
            RtsValues::F32(&[i as f32]),
        );
    }
    crash_case(
        &full,
        &d,
        &Case {
            name: "block",
            pathname: b"/C/B32/FLOW//1Day/S/",
            block_path: b"/C/B32/FLOW/01Jan2021/1Day/S/",
            start: t(2021, 1, 2, 0),
            values: &[32.0],
            visible_from: WriteStep::Link,
        },
    );
    // On a public fixture copy.
    if let Some(src) = bald_eagle() {
        let be = d.join("be.dss");
        std::fs::copy(&src, &be).unwrap();
        // The table slot may already hold a bin there: then the entry is visible from
        // Publish, otherwise from Link.
        let w = words(&be);
        let th = dss_core::dss7::table_hash(fresh.block_path, w[15]);
        let visible_from = if w[(w[19] + th) as usize] != 0 {
            WriteStep::Publish
        } else {
            WriteStep::Link
        };
        crash_case(
            &be,
            &d,
            &Case {
                name: "be",
                visible_from,
                ..fresh
            },
        );
    }
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Fuzz: input validation never panics; accepted inputs round-trip
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
    let mut rng = Rng(0x5eed_0020_0030);
    let eparts: [&[u8]; 12] = [
        b"1Hour",
        b"1HOUR",
        b"15Minute",
        b"1Day",
        b"1Month",
        b"1Year",
        b"15Second",
        b"1Week",
        b"IR-Day",
        b"1Hou",
        b"~1Day",
        b"Semi-Month",
    ];
    let alphabet = b"AZaz09 /-_.:|~\x00\x01\x7f\xe9";
    let (mut ok, mut refused, mut written) = (0usize, 0usize, 0usize);
    for i in 0..iters {
        let mut path: Vec<u8> = Vec::new();
        if rng.next() % 4 == 0 {
            for _ in 0..(rng.next() % 60) {
                path.push(alphabet[(rng.next() % alphabet.len() as u64) as usize]);
            }
        } else {
            path.push(b'/');
            for part in 0..6 {
                let len = (rng.next() % 12) as usize;
                if part == 4 {
                    path.extend_from_slice(eparts[(rng.next() % eparts.len() as u64) as usize]);
                } else if part == 3 && rng.next() % 2 == 0 {
                    path.extend_from_slice(b"01JAN2021");
                } else {
                    for _ in 0..len {
                        path.push(b"ABCXYZ09 -"[(rng.next() % 10) as usize]);
                    }
                }
                path.push(b'/');
            }
        }
        let n = (rng.next() % 40) as usize;
        let vals: Vec<f32> = (0..n)
            .map(|_| match rng.next() % 6 {
                0 => -901.0,
                1 => -f32::MAX,
                2 => f32::NAN,
                _ => (rng.next() % 1000) as f32 / 10.0,
            })
            .collect();
        let start = match rng.next() % 5 {
            0 => 0,
            1 => -(rng.next() as i64 % (1 << 40)),
            2 => rng.next() as i64 % (1 << 41),
            _ => t(
                2000 + (rng.next() % 50) as i64,
                1 + (rng.next() % 12) as i64,
                1 + (rng.next() % 28) as i64,
                (rng.next() % 86_400) as i64,
            ),
        };
        let q: Vec<i32> = (0..(if rng.next() % 3 == 0 { n } else { n / 2 }))
            .map(|_| rng.next() as i32)
            .collect();
        let units: &[u8] = if rng.next() % 10 == 0 {
            b"c\xe9"
        } else {
            b"cfs"
        };
        let mut input = RtsInput::new(&path, start, RtsValues::F32(&vals), units, b"INST-VAL");
        if rng.next() % 2 == 0 {
            input.quality = Some(&q);
        }
        match validate_rts_input(&input) {
            Ok(()) => {
                ok += 1;
                if i % 7 == 0 {
                    match write_rts(&f, &input, &opts()) {
                        Ok(r) => {
                            written += r.blocks.len();
                            assert!(!r.blocks.is_empty());
                        }
                        Err(WriteError::Unsupported(_)) => {}
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
    let mut file = Dss7File::open(&f).unwrap();
    let cat = file.catalog().unwrap();
    assert!(cat.catalog.anomalies.is_empty());
    for e in cat.catalog.live() {
        let r = file.lookup(&e.pathname).unwrap().unwrap();
        file.read_ts_record(&r).unwrap();
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Old lock scheme (read-array pointer 0, synthetic 7-JA file): the default policy
/// (maintainer 2026-10-28, P19) writes while holding word 100 and reports
/// `readers_undetected`; a strict policy refuses.
#[test]
fn old_lock_scheme_default_writes_strict_refuses() {
    let d = tmp("old");
    let f = d.join("old.dss");
    create_dss7(&f).unwrap();
    let mut b = std::fs::read(&f).unwrap();
    b[92 * 8..93 * 8].copy_from_slice(&0i64.to_le_bytes());
    std::fs::write(&f, &b).unwrap();
    let v = [1.0f32, 2.0, 3.0];
    let input = RtsInput::new(
        b"/O/L/D//1Hour/S/",
        t(2021, 1, 1, 3600),
        RtsValues::F32(&v),
        b"cfs",
        b"INST-VAL",
    );
    let mut strict = opts();
    strict.lock_policy.allow_undetectable_readers = false;
    match write_rts(&f, &input, &strict) {
        Err(WriteError::Locked(r)) => assert!(r.readers_undetectable && r.reasons.is_empty()),
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&f).unwrap(), b);
    let r = write_rts(&f, &input, &opts()).unwrap();
    assert!(r.readers_undetected);
    assert_eq!(r.blocks[0].outcome, BlockOutcome::Added);
    assert_eq!(f32s(&read_all(&f, b"/O/L/D//1Hour/S/").ts.values), v);
    let _ = std::fs::remove_dir_all(&d);
}

/// The write lock is held while a write runs (seen from another handle of this
/// process) and released after the write and after an error.
///
/// Windows only: the whole-file lock is visible to another handle of this process.
/// POSIX `fcntl` record locks belong to the process, so an in-process probe never sees
/// them; Linux uses `lock_seen_from_another_process` below.
#[test]
#[cfg(windows)]
fn lock_held_during_write_and_released_after_error() {
    use dss_core::lock::lock_status;
    let d = tmp("lockheld");
    let f = d.join("l.dss");
    create_dss7(&f).unwrap();
    let held_now = |p: &Path| match lock_status(p) {
        Ok(r) => r.locked(),
        // Windows: the whole-file lock also blocks the probe's header read.
        Err(_) => true,
    };
    assert!(!held_now(&f));
    let fp = f.clone();
    let writer = std::thread::spawn(move || {
        let v: Vec<f32> = (0..1440 * 120).map(|i| (i % 97) as f32).collect();
        let input = RtsInput::new(
            b"/L/H/FLOW//1Minute/S/",
            t(2021, 1, 1, 60),
            RtsValues::F32(&v),
            b"cfs",
            b"INST-VAL",
        );
        write_rts(&fp, &input, &WriteOptions::default()).unwrap()
    });
    let mut seen = false;
    while !writer.is_finished() {
        seen |= held_now(&f);
    }
    let rep = writer.join().unwrap();
    assert_eq!(rep.blocks.len(), 120);
    assert!(
        seen,
        "the lock was never seen held during a 120-block write"
    );
    let expect = if cfg!(windows) {
        "whole_file_lockfileex"
    } else {
        "fcntl_setlk"
    };
    assert_eq!(rep.lock_method, expect);
    assert!(!held_now(&f), "released after the write");
    // An error after the lock was taken releases it too.
    let mut o = opts();
    o.fault = Some(FaultPoint {
        block: 0,
        step: WriteStep::Publish,
    });
    let input = RtsInput::new(
        b"/L/E/FLOW//1Hour/S/",
        t(2021, 1, 1, 3600),
        RtsValues::F32(&[1.0]),
        b"cfs",
        b"INST-VAL",
    );
    assert!(matches!(
        write_rts(&f, &input, &o),
        Err(WriteError::InjectedFault { .. })
    ));
    assert!(!held_now(&f), "released after an error");
    let rep = write_rts(&f, &input, &opts()).unwrap();
    assert!(rep.reconciled_interrupted_write);
    let _ = std::fs::remove_dir_all(&d);
}

/// A crash, then a second crash inside the reconciling write, then recovery.
#[test]
fn double_crash_recovers() {
    let d = tmp("double");
    let base = d.join("base.dss");
    create_dss7(&base).unwrap();
    write(
        &base,
        b"/D/OTHER/FLOW//1Day/S/",
        t(2021, 1, 2, 0),
        RtsValues::F32(&[1.0]),
    );
    let v: Vec<f32> = (0..24).map(|i| i as f32).collect();
    let input = RtsInput::new(
        b"/D/NEW/FLOW//1Hour/S/",
        t(2021, 3, 1, 3600),
        RtsValues::F32(&v),
        b"cfs",
        b"INST-VAL",
    );
    let other_v = [9.0f32];
    let other = RtsInput::new(
        b"/D/SECOND/FLOW//1Day/S/",
        t(2021, 1, 2, 0),
        RtsValues::F32(&other_v),
        b"cfs",
        b"INST-VAL",
    );
    let mut n = 0;
    for first in WriteStep::ALL {
        for second in [
            WriteStep::Allocation,
            WriteStep::Publish,
            WriteStep::Counters,
            WriteStep::Link,
            WriteStep::Activate,
        ] {
            for second_input in [&input, &other] {
                let f = d.join(format!("dc_{n}.dss"));
                n += 1;
                std::fs::copy(&base, &f).unwrap();
                let mut o = opts();
                o.fault = Some(FaultPoint {
                    block: 0,
                    step: first,
                });
                assert!(matches!(
                    write_rts(&f, &input, &o),
                    Err(WriteError::InjectedFault { .. })
                ));
                o.fault = Some(FaultPoint {
                    block: 0,
                    step: second,
                });
                match write_rts(&f, second_input, &o) {
                    Err(WriteError::InjectedFault { .. }) | Ok(_) => {}
                    Err(e) => panic!("{first:?}/{second:?}: {e}"),
                }
                write_rts(&f, &input, &opts()).unwrap();
                write_rts(&f, &other, &opts()).unwrap();
                assert_clean(&f, &format!("double crash {first:?}/{second:?}"));
                assert!(contains(
                    &values_at(&f, b"/D/NEW/FLOW/01Mar2021/1Hour/S/").unwrap(),
                    &v
                ));
                assert!(contains(
                    &values_at(&f, b"/D/SECOND/FLOW/01Jan2021/1Day/S/").unwrap(),
                    &other_v
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Child half of `lock_seen_from_another_process`: run only when the parent sets
/// `P20_LOCK_CHILD_DIR`. Writes 240 one-minute blocks, then a write that fails at
/// Publish, then signals the parent and waits while it probes.
#[test]
#[ignore = "child process of lock_seen_from_another_process"]
#[cfg(target_os = "linux")]
fn lock_child_writer() {
    let Some(dir) = std::env::var_os("P20_LOCK_CHILD_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let f = dir.join("l.dss");
    let v: Vec<f32> = (0..1440 * 240).map(|i| (i % 97) as f32).collect();
    let input = RtsInput::new(
        b"/L/H/FLOW//1Minute/S/",
        t(2021, 1, 1, 60),
        RtsValues::F32(&v),
        b"cfs",
        b"INST-VAL",
    );
    let rep = write_rts(&f, &input, &WriteOptions::default()).unwrap();
    assert_eq!(rep.lock_method, "fcntl_setlk");
    let mut o = opts();
    o.fault = Some(FaultPoint {
        block: 0,
        step: WriteStep::Publish,
    });
    let small = RtsInput::new(
        b"/L/E/FLOW//1Hour/S/",
        t(2021, 1, 1, 3600),
        RtsValues::F32(&[1.0]),
        b"cfs",
        b"INST-VAL",
    );
    assert!(matches!(
        write_rts(&f, &small, &o),
        Err(WriteError::InjectedFault { .. })
    ));
    std::fs::write(dir.join("faulted"), b"1").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
}

/// Linux: a probe from this (parent) process sees the child writer's `fcntl` lock while
/// the write runs, and sees it released after the child's failed write while the child
/// is still alive.
#[test]
#[cfg(target_os = "linux")]
fn lock_seen_from_another_process() {
    use dss_core::lock::lock_status;
    let d = tmp("lockchild");
    let f = d.join("l.dss");
    create_dss7(&f).unwrap();
    assert!(!lock_status(&f).unwrap().locked());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock_child_writer",
            "--ignored",
            "--test-threads=1",
        ])
        .env("P20_LOCK_CHILD_DIR", &d)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut seen = false;
    let marker = d.join("faulted");
    while !marker.exists() {
        if let Ok(r) = lock_status(&f) {
            seen |= r.locked();
        }
        if let Some(st) = child.try_wait().unwrap() {
            panic!("child exited early: {st}");
        }
    }
    assert!(seen, "the child's lock was never seen during its write");
    assert!(
        !lock_status(&f).unwrap().locked(),
        "released after the failed write"
    );
    assert!(child.wait().unwrap().success());
    let rep = write_rts(
        &f,
        &RtsInput::new(
            b"/L/E/FLOW//1Hour/S/",
            t(2021, 1, 1, 3600),
            RtsValues::F32(&[1.0]),
            b"cfs",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap();
    assert!(rep.reconciled_interrupted_write);
    let _ = std::fs::remove_dir_all(&d);
}

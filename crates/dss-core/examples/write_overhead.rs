//! P76: per-call and per-record write cost: an indicative timing harness, not a
//! test. It writes generated data to a new file in the folder given and prints medians.
//!
//! ```text
//! write_overhead <scratch dir> [records]
//! ```
//!
//! For DSS7 and DSS6 it times, per record of 48 hourly values (distinct pathnames):
//! - `held`: `write_rts_to` / `write_rts6_to` on one held read-write handle, `sync` none;
//! - `path`: `write_rts` / `write_rts6` by path (one open per call), `sync` none;
//! - `batch`: `write_rts_batch_to` / `write_rts6_batch_to` (one lock, many records);
//! - `lock probe`: `lock::lock_status_of` on the held handle alone.

use dss_core::dss6::write as w6;
use dss_core::dss7::write as w7;
use dss_core::dss7::write::{RtsInput, RtsValues, SyncMode, WriteOptions};
use dss_core::lock;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn per_item(label: &str, n: usize, mut f: impl FnMut(usize)) {
    let mut xs = Vec::with_capacity(n);
    for i in 0..n {
        let t = Instant::now();
        f(i);
        xs.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("{label:40} median {:.4} ms", median(xs));
}

fn fresh(dir: &Path, name: &str, version: u8) -> PathBuf {
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    if version == 7 {
        w7::create_dss7_with(&p, SyncMode::None).expect("create");
    } else {
        w6::create_dss6_with(&p, SyncMode::None).expect("create");
    }
    p
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).expect("usage: write_overhead <dir> [records]"));
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let vals: Vec<f64> = (0..48).map(|i| i as f64).collect();
    // 2020-01-01 01:00 in HEC seconds (julian 43830).
    let start = 43_830 * 86_400 + 3_600;
    let opts = WriteOptions {
        sync: SyncMode::None,
        ..WriteOptions::default()
    };
    let names: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("/A/B{i}/FLOW//1HOUR/F/").into_bytes())
        .collect();
    let input =
        |i: usize| RtsInput::new(&names[i], start, RtsValues::F64(&vals), b"CFS", b"INST-VAL");
    for version in [7u8, 6] {
        println!("DSS{version}, {n} records of 48 values");
        let p = fresh(&dir, &format!("held{version}.dss"), version);
        let file = OpenOptions::new().read(true).write(true).open(&p).unwrap();
        per_item("held handle, one call per record", n, |i| {
            if version == 7 {
                w7::write_rts_to(&file, &input(i), &opts).unwrap();
            } else {
                w6::write_rts6_to(&file, &input(i), &opts).unwrap();
            }
        });
        per_item("lock probe alone (held handle)", n, |_| {
            lock::lock_status_of(&file).unwrap();
        });
        drop(file);
        let p = fresh(&dir, &format!("path{version}.dss"), version);
        per_item("by path, one call per record", n.min(200), |i| {
            if version == 7 {
                w7::write_rts(&p, &input(i), &opts).unwrap();
            } else {
                w6::write_rts6(&p, &input(i), &opts).unwrap();
            }
        });
        let p = fresh(&dir, &format!("batch{version}.dss"), version);
        let file = OpenOptions::new().read(true).write(true).open(&p).unwrap();
        let inputs: Vec<RtsInput> = (0..n).map(input).collect();
        let t = Instant::now();
        let ok = batch(version, &file, &inputs, &opts);
        if ok {
            println!(
                "{:40} {:.4} ms per record",
                "batch, one call",
                t.elapsed().as_secs_f64() * 1e3 / n as f64
            );
        }
    }
}

fn batch(version: u8, file: &std::fs::File, inputs: &[RtsInput], opts: &WriteOptions) -> bool {
    let r = if version == 7 {
        let items: Vec<w7::BatchItem> = inputs.iter().cloned().map(w7::BatchItem::Rts).collect();
        w7::write_batch_to(file, &items, opts).unwrap()
    } else {
        let items: Vec<w6::BatchItem6> = inputs.iter().cloned().map(w6::BatchItem6::Rts).collect();
        w6::write_batch6_to(file, &items, opts).unwrap()
    };
    assert!(r.failed.is_none(), "{:?}", r.failed);
    assert_eq!(r.reports.len(), inputs.len());
    true
}

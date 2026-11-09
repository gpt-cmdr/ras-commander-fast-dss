//! Paired-data fuzzing : corrupt PD info blocks, internal
//! headers, labels and data, and truncate files; every read must return `Ok` or a typed
//! error, never panic. Bases: the synthetic files of `pd_support` and the public
//! fixtures. Iterations: 1,500 per base in debug builds, 20,000 in release builds or with
//! `PD_FUZZ_ITERS`.

mod pd_support;

use dss_core::dss6::{word_byte_offset, CatalogOptions, Dss6File, InfoCheck};
use dss_core::dss7::Dss7File;
use pd_support::dss7_common::Rng;
use pd_support::{fixture, standard_dss6, standard_dss7};
use std::ops::Range;

fn iters() -> usize {
    std::env::var("PD_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if cfg!(debug_assertions) {
            1_500
        } else {
            20_000
        })
}

/// Byte ranges of the PD records' info blocks and areas (the fuzz targets).
fn dss7_targets(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut f = Dss7File::from_bytes(bytes).unwrap();
    let cat = f.catalog().unwrap();
    let mut out = Vec::new();
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        if !(e.is_live() && (e.record_type == 200 || e.record_type == 205)) {
            continue;
        }
        let Ok(rec) = f.record_at(&cat, i) else {
            continue;
        };
        let info = &rec.info;
        let w = |a: i64| (a.max(0) as usize) * 8;
        out.push(w(info.address)..w(info.address) + 31 * 8);
        for a in [
            info.internal_header,
            info.header2,
            info.user_header,
            info.values1,
            info.values2,
        ] {
            if a.number_ints > 0 {
                let s = w(a.address);
                out.push(s..s + (a.number_ints as usize).div_ceil(2) * 8);
            }
        }
    }
    out
}

fn dss6_targets(bytes: &[u8]) -> Vec<Range<usize>> {
    let f = Dss6File::from_bytes(bytes).unwrap();
    let walk = f.walk_bins(&CatalogOptions::default()).unwrap();
    let mut out = Vec::new();
    let b = |a: i32| word_byte_offset(i64::from(a)).unwrap_or(0) as usize;
    for e in walk.live() {
        if e.record_type != 200 && e.record_type != 205 {
            continue;
        }
        let Ok(info) = f.read_info(e.info_address.into(), e.pathname.len() as i32) else {
            continue;
        };
        out.push(b(e.info_address)..b(e.info_address) + 40 * 4);
        let (ia, inn) = info.internal_header;
        out.push(b(ia)..b(ia) + (inn.max(0) as usize) * 4 + 8);
        out.push(
            b(info.data_address)..b(info.data_address) + (info.data_words.max(0) as usize) * 4,
        );
    }
    out
}

fn read_all_7(bytes: &[u8]) -> usize {
    let Ok(mut f) = Dss7File::from_bytes(bytes) else {
        return 0;
    };
    let Ok(cat) = f.catalog() else { return 0 };
    let mut ok = 0;
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        if e.record_type == 200 || e.record_type == 205 {
            if f.read_pd(&e.pathname).is_ok() {
                ok += 1;
            }
            if let Ok(rec) = f.record_at(&cat, i) {
                let _ = f.read_pd_record(&rec);
            }
        }
    }
    ok
}

fn read_all_6(bytes: &[u8]) -> usize {
    let Ok(f) = Dss6File::from_bytes(bytes) else {
        return 0;
    };
    let opts = CatalogOptions {
        verify_info: InfoCheck::None,
        ..Default::default()
    };
    let Ok(walk) = f.walk_bins(&opts) else {
        return 0;
    };
    let mut ok = 0;
    for e in walk.entries.iter() {
        if e.record_type == 200 || e.record_type == 205 {
            if f.read_pd(&e.pathname).is_ok() {
                ok += 1;
            }
            let _ = f.read_pd_entry(&e.to_catalog_entry());
        }
    }
    ok
}

fn mutate(rng: &mut Rng, base: &[u8], targets: &[Range<usize>], word: usize) -> Vec<u8> {
    let mut b = base.to_vec();
    match rng.below(10) {
        // Truncate somewhere inside or after a PD record.
        0 => {
            let t = &targets[rng.below(targets.len() as u64) as usize];
            let cut = (t.start + rng.below((t.len() as u64).max(1)) as usize).min(b.len());
            b.truncate(cut);
        }
        // Overwrite whole words with interesting values.
        1..=5 => {
            for _ in 0..1 + rng.below(4) {
                let t = &targets[rng.below(targets.len() as u64) as usize];
                let at = t.start + (rng.below((t.len() / word).max(1) as u64) as usize) * word;
                let v: i64 = match rng.below(8) {
                    0 => 0,
                    1 => -1,
                    2 => i64::from(i32::MAX),
                    3 => i64::from(i32::MIN),
                    4 => 1 << 40,
                    5 => rng.below(64) as i64,
                    6 => -(rng.below(1000) as i64),
                    _ => rng.next() as i64,
                };
                for (k, byte) in v.to_le_bytes().iter().take(word).enumerate() {
                    if let Some(x) = b.get_mut(at + k) {
                        *x = *byte;
                    }
                }
            }
        }
        // Random byte flips.
        _ => {
            for _ in 0..1 + rng.below(16) {
                let t = &targets[rng.below(targets.len() as u64) as usize];
                let at = t.start + rng.below((t.len() as u64).max(1)) as usize;
                if let Some(x) = b.get_mut(at) {
                    *x ^= 1 << rng.below(8);
                }
            }
        }
    }
    b
}

fn run(name: &str, base: Vec<u8>, v7: bool, seed: u64) {
    let targets = if v7 {
        dss7_targets(&base)
    } else {
        dss6_targets(&base)
    };
    assert!(!targets.is_empty(), "{name}");
    let clean = if v7 {
        read_all_7(&base)
    } else {
        read_all_6(&base)
    };
    assert!(clean > 0, "{name}");
    let mut rng = Rng(seed);
    let word = if v7 { 8 } else { 4 };
    for _ in 0..iters() {
        let b = mutate(&mut rng, &base, &targets, word);
        if v7 {
            read_all_7(&b);
        } else {
            read_all_6(&b);
        }
    }
}

#[test]
fn fuzz_dss7_synthetic() {
    run("synthetic7", standard_dss7(), true, 0x5eed_0001);
}

#[test]
fn fuzz_dss6_synthetic() {
    run("synthetic6", standard_dss6(), false, 0x5eed_0002);
}

#[test]
fn fuzz_dss7_sample7() {
    run(
        "sample7",
        std::fs::read(fixture("sample7.dss")).unwrap(),
        true,
        0x5eed_0003,
    );
}

#[test]
fn fuzz_dss6_public() {
    for (i, n) in ["castro.dss", "sample6.dss"].iter().enumerate() {
        run(
            n,
            std::fs::read(fixture(n)).unwrap(),
            false,
            0x5eed_0010 + i as u64,
        );
    }
}

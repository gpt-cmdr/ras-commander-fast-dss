//! P06 acceptance check 7: truncated, bit-flipped and word-corrupted DSS7 files give
//! errors or anomalies, never panics, and every loop terminates.
//!
//! Deterministic sweeps (xorshift seeds) over a synthetic file, plus the public fixtures
//! when present. Set `DSS7_FUZZ_ITERS` to scale the random sweeps.

mod dss7_common;

use dss7_common::{build, get_word, set_word, standard_spec, Rec, Rng};
use dss_core::dss7::{CatalogOptions, Dss7File, InfoCheck};

fn iters(default: usize) -> usize {
    std::env::var("DSS7_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Run everything a reader can do on `bytes`; results are ignored, panics are not.
fn exercise(bytes: &[u8], probes: &[Vec<u8>]) {
    let Ok(mut f) = Dss7File::from_bytes(bytes) else {
        return;
    };
    let opts = CatalogOptions {
        check_info: InfoCheck::Live,
        ..CatalogOptions::default()
    };
    if let Ok(cat) = f.catalog_with(&opts) {
        let _ = cat.resolved_live();
        for e in cat.catalog.entries.iter().take(64) {
            let _ = f.lookup(&e.pathname);
        }
    }
    for p in probes {
        let _ = f.lookup(p);
    }
}

fn synthetic() -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut spec = standard_spec();
    spec.recs
        .push(Rec::live("/BASIN/ORPHAN/FLOW/01Jan2020/1Hour/RUN/", 100).orphan());
    spec.recs
        .push(Rec::live("/BASIN/ZERO/FLOW/01Jan2020/1Hour/RUN/", 100).zero_info());
    let probes = spec.recs.iter().map(|r| r.path.clone()).collect();
    (build(&spec).bytes, probes)
}

#[test]
fn truncation_sweep_synthetic() {
    let (bytes, probes) = synthetic();
    for len in (0..bytes.len()).step_by(4) {
        let cut = &bytes[..len];
        // As is: the header check rejects it (or it is too short).
        assert!(Dss7File::from_bytes(cut).is_err() || len >= bytes.len() - 8);
        // With kfileSize patched to fit, the walk runs into the missing data.
        if len >= 808 {
            let mut patched = cut.to_vec();
            set_word(&mut patched, 5, (len / 8) as i64 - 1);
            exercise(&patched, &probes);
        }
    }
}

#[test]
fn bit_flip_sweep_synthetic() {
    let (bytes, probes) = synthetic();
    let mut rng = Rng(0x5eed_0006);
    for _ in 0..iters(4000) {
        let mut b = bytes.clone();
        for _ in 0..=rng.below(4) {
            let bit = rng.below(b.len() as u64 * 8);
            b[(bit / 8) as usize] ^= 1 << (bit % 8);
        }
        exercise(&b, &probes);
    }
}

#[test]
fn word_corruption_sweep_synthetic() {
    let (bytes, probes) = synthetic();
    let eof = get_word(&bytes, 5);
    let words = bytes.len() / 8;
    let mut rng = Rng(0xc0ffee);
    for _ in 0..iters(4000) {
        let mut b = bytes.clone();
        let w = rng.below(words as u64) as usize;
        let v = match rng.below(10) {
            0 => 0,
            1 => -1,
            2 => i64::MAX,
            3 => i64::MIN,
            4 => eof,
            5 => eof - 1,
            6 => 100 + rng.below(200) as i64,
            7 => rng.below(eof as u64) as i64,
            8 => (rng.below(400) as i64) | ((rng.below(60) as i64) << 32),
            _ => rng.next() as i64,
        };
        set_word(&mut b, w, v);
        exercise(&b, &probes);
    }
}

#[test]
fn header_word_sweep_synthetic() {
    // Every header word against a set of hostile values.
    let (bytes, probes) = synthetic();
    let eof = get_word(&bytes, 5);
    for w in 0..100 {
        for v in [
            0,
            1,
            -1,
            19,
            20,
            99,
            100,
            177,
            178,
            eof - 1,
            eof,
            eof + 1,
            1 << 40,
            i64::MAX,
            i64::MIN,
        ] {
            let mut b = bytes.clone();
            set_word(&mut b, w, v);
            exercise(&b, &probes);
        }
    }
}

fn structural_flips(path: std::path::PathBuf, seed: u64, n: usize) {
    let bytes = std::fs::read(path).unwrap();
    let f = Dss7File::from_bytes(&bytes).unwrap();
    let h = f.header().clone();
    let probes: Vec<Vec<u8>> = Vec::new();
    // Structural region: header, hash table, and the first bin blocks plus the info and
    // data that follow them.
    let end = ((h.first_bin_address + 4 * h.block_words()) as usize * 8).min(bytes.len());
    let ht = h.hash_table_address as usize * 8;
    let mut rng = Rng(seed);
    for _ in 0..n {
        let mut b = bytes.clone();
        for _ in 0..=rng.below(3) {
            let off = match rng.below(3) {
                0 => rng.below(800) as usize,
                1 => ht + rng.below((h.max_hash * 8) as u64) as usize,
                _ => {
                    (h.first_bin_address as usize * 8)
                        + rng.below((end - h.first_bin_address as usize * 8) as u64) as usize
                }
            };
            b[off] ^= 1 << rng.below(8);
        }
        // Truncation at a random point, with and without a patched kfileSize.
        if rng.below(4) == 0 {
            let len = 800 + rng.below((b.len() - 800) as u64) as usize;
            b.truncate(len);
            if rng.below(2) == 0 {
                set_word(&mut b, 5, (len / 8) as i64 - 1);
            }
        }
        exercise(&b, &probes);
    }
}

#[test]
fn public_fixture_flips() {
    if let Some(p) = dss7_common::bald_eagle() {
        structural_flips(p, 0xba1d, iters(4000) / 20);
    }
    if let Some(p) = dss7_common::sample7() {
        structural_flips(p, 0x5a7, iters(4000) / 40);
    }
}

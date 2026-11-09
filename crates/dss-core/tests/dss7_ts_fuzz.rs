//! Fuzz the DSS7 time-series reader : corrupted info blocks, internal
//! headers, compression headers and data words, and truncated files must give errors,
//! never panics. `DSS7_TS_FUZZ_ITERS` raises the iteration count (default 1,500 per
//! fixture).

mod dss7_common;

use dss7_common::{bald_eagle, get_word, sample7, set_word, Rng};
use dss_core::dss7::{Dss7File, RecordRef, TsReadOptions, TsWindow};

fn iters() -> u64 {
    std::env::var("DSS7_TS_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_500)
}

fn records(bytes: &[u8]) -> Vec<RecordRef> {
    let mut f = Dss7File::from_bytes(bytes).unwrap();
    let cat = f.catalog().unwrap();
    let mut out = Vec::new();
    for e in cat
        .catalog
        .live()
        .filter(|e| (100..200).contains(&e.record_type))
    {
        if let Ok(Some(r)) = f.lookup(&e.pathname) {
            out.push(r);
        }
    }
    out
}

/// Exercise every read path on (possibly damaged) bytes; results are ignored.
fn exercise(bytes: &[u8], rec: &RecordRef) {
    let Ok(mut f) = Dss7File::from_bytes(bytes) else {
        return;
    };
    if let Ok(Some(r)) = f.lookup(&rec.pathname) {
        let _ = f.read_ts_record(&r);
        let _ = f.read_ts_profile_record(&r);
    }
    // Same record through the info block we already know (bypasses lookup checks).
    let _ = f.read_ts_record(rec);
    let mut p = rec.pathname.clone();
    if let Some(parts) = split(&p) {
        p = parts;
        let first = rec.info.first_date.0 as i64 * 86_400;
        let w = TsWindow {
            start: first - 86_400 * 3,
            end: first + 86_400 * 40,
        };
        let _ = f.read_ts(
            &p,
            &TsReadOptions {
                window: Some(w),
                trim_missing: true,
            },
        );
    }
}

/// Blank the D part.
fn split(p: &[u8]) -> Option<Vec<u8>> {
    let idx: Vec<usize> = p
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == b'/')
        .map(|(i, _)| i)
        .collect();
    if idx.len() != 7 {
        return None;
    }
    let mut v = p[..idx[3] + 1].to_vec();
    v.extend_from_slice(&p[idx[4]..]);
    Some(v)
}

fn hostile(rng: &mut Rng, old: i64) -> i64 {
    match rng.below(8) {
        0 => 0,
        1 => -1,
        2 => i64::MAX,
        3 => i64::from(i32::MAX) | (i64::from(i32::MAX) << 32),
        4 => old ^ (1 << rng.below(64)),
        5 => old.wrapping_add(rng.below(5) as i64 - 2),
        6 => (rng.next() as i64) & 0xFFFF_FFFF,
        _ => rng.next() as i64,
    }
}

fn fuzz_fixture(bytes: Vec<u8>, seed: u64) {
    let recs = records(&bytes);
    assert!(!recs.is_empty());
    let mut rng = Rng(seed);
    let words = (bytes.len() / 8) as u64;
    for _ in 0..iters() {
        let rec = &recs[rng.below(recs.len() as u64) as usize];
        let i = &rec.info;
        let mut b = bytes.clone();
        let flips = 1 + rng.below(3);
        for _ in 0..flips {
            // Target: info words, internal header, header2, values1, or anywhere.
            let target = match rng.below(5) {
                0 => i.address + rng.below(30) as i64,
                1 => i.internal_header.address + rng.below(10) as i64,
                2 => i.header2.address + rng.below(4) as i64,
                3 => {
                    i.values1.address
                        + rng.below((i.values1.number_ints.max(2) / 2) as u64 + 1) as i64
                }
                _ => rng.below(words) as i64,
            };
            if target < 0 || target as u64 >= words {
                continue;
            }
            let t = target as usize;
            let old = get_word(&b, t);
            set_word(&mut b, t, hostile(&mut rng, old));
        }
        exercise(&b, rec);
    }
    // Truncations through the data of a few records.
    for rec in recs.iter().take(8) {
        let end = ((rec.info.values1.address + rec.info.values1.number_ints / 2 + 1) * 8) as usize;
        for cut in [
            rec.info.address as usize * 8 + 40,
            rec.info.internal_header.address as usize * 8 + 4,
            end.saturating_sub(12),
        ] {
            if cut < bytes.len() {
                exercise(&bytes[..cut], rec);
                // Keep kfileSize consistent with the cut so reads get past the header check.
                let mut b = bytes[..cut - cut % 8].to_vec();
                if b.len() >= 800 {
                    let last = (b.len() / 8) as i64 - 1;
                    set_word(&mut b, 5, last);
                    exercise(&b, rec);
                }
            }
        }
    }
}

#[test]
fn fuzz_sample7_ts() {
    let Some(p) = sample7() else { return };
    fuzz_fixture(std::fs::read(p).unwrap(), 0x5eed_0710);
}

#[test]
fn fuzz_bald_eagle_ts() {
    let Some(p) = bald_eagle() else { return };
    fuzz_fixture(std::fs::read(p).unwrap(), 0x0ba1_dea6);
}

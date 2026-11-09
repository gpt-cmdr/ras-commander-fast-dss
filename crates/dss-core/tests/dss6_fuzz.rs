//! Deterministic robustness tests for the DSS6 reader :
//! truncation, bit flips, hostile header and bin words, cyclic chains, out-of-range
//! addresses, bad pathname lengths and statuses. The reader must never panic and every
//! loop must terminate; errors and anomalies are expected.
//!
//! Bases are synthetic files plus, when present, public samples from `DSS6_PUBLIC_DIR`.

mod dss6_support;

use dss6_support::{build, mixed_records, Opts};
use dss_core::dss6::{CatalogOptions, Dss6File, InfoCheck};
use std::path::PathBuf;

/// Small deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[derive(Default, Debug)]
struct Tally {
    runs: usize,
    open_err: usize,
    with_anomalies: usize,
    lookups: usize,
}

/// Open and fully exercise a buffer. Panics propagate and fail the test.
fn exercise(bytes: &[u8], t: &mut Tally) {
    t.runs += 1;
    let Ok(f) = Dss6File::from_bytes(bytes) else {
        t.open_err += 1;
        return;
    };
    let opts = CatalogOptions {
        verify_info: InfoCheck::All,
        ..Default::default()
    };
    let Ok(walk) = f.walk_bins(&opts) else { return };
    if !walk.anomalies.is_empty() {
        t.with_anomalies += 1;
    }
    for e in walk.entries.iter().take(64) {
        let _ = f.lookup(&e.pathname);
        let _ = f.read_info(e.info_address.into(), e.pathname.len() as i32);
        let _ = e.display_pathname();
        t.lookups += 1;
    }
    let _ = f.lookup(b"/A/B/C/D/~1DAY/F/");
    let _ = f.catalog_with(&CatalogOptions {
        stop_at_knrecs: true,
        ..Default::default()
    });
    let _ = walk.into_catalog();
}

fn bases() -> Vec<(String, Vec<u8>)> {
    let recs = mixed_records();
    let mut v = vec![
        (
            "synthetic-t1".to_string(),
            build(&recs, &Opts::default()).bytes,
        ),
        (
            "synthetic-t2".to_string(),
            build(
                &recs,
                &Opts {
                    ktable: 2,
                    ..Opts::default()
                },
            )
            .bytes,
        ),
        (
            "synthetic-bin63".to_string(),
            build(
                &recs,
                &Opts {
                    kbnsiz: 63,
                    khash: 32,
                    ..Opts::default()
                },
            )
            .bytes,
        ),
    ];
    let dir = std::env::var_os("DSS6_PUBLIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    for name in [
        "10_castro.dss",
        "14_green_river.dss",
        "31_Muncie.dss",
        "01_hrap.dss",
    ] {
        if let Ok(b) = std::fs::read(dir.join(name)) {
            v.push((name.to_string(), b));
        }
    }
    v
}

fn set_word(b: &mut [u8], word_index: usize, v: i32) {
    if let Some(s) = b.get_mut(4 * word_index..4 * word_index + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn get_word(b: &[u8], word_index: usize) -> i32 {
    b.get(4 * word_index..4 * word_index + 4)
        .map_or(0, |s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Byte offset of a 1-based word address.
fn off(addr: i64) -> usize {
    dss_core::dss6::word_byte_offset(addr).unwrap_or(0) as usize
}

#[test]
fn truncation_never_panics() {
    for (name, base) in bases() {
        let mut t = Tally::default();
        let n = base.len();
        let step = (n / 400).max(1);
        let mut cuts: Vec<usize> = (0..n.min(1600)).collect();
        cuts.extend((0..n).step_by(step));
        cuts.extend(n.saturating_sub(16)..n);
        for cut in cuts {
            exercise(&base[..cut], &mut t);
        }
        eprintln!("truncation {name}: {t:?}");
    }
}

#[test]
fn bit_flips_never_panic() {
    let mut rng = Rng(0x0007_D556);
    for (name, base) in bases() {
        let mut t = Tally::default();
        let iters = if base.len() > 200_000 { 150 } else { 1500 };
        // Bias flips toward the header, hash table and first bin blocks.
        let hot = base.len().min(48 * 1024);
        for _ in 0..iters {
            let mut b = base.clone();
            for _ in 0..1 + rng.below(4) {
                let pos = if rng.below(4) < 3 {
                    rng.below(hot)
                } else {
                    rng.below(b.len())
                };
                b[pos] ^= 1 << rng.below(8);
            }
            exercise(&b, &mut t);
        }
        eprintln!("bit flips {name}: {t:?}");
    }
}

const NASTY: [i32; 16] = [
    0,
    -1,
    1,
    2,
    11,
    63,
    127,
    392,
    393,
    1001,
    -1001,
    -9753,
    -9630,
    -1357,
    i32::MAX,
    i32::MIN,
];

#[test]
fn hostile_header_and_bin_words() {
    let mut rng = Rng(0x0007_0BAD);
    for (name, base) in bases() {
        let mut t = Tally::default();
        // Every header word with every nasty value.
        for w in 1..69usize {
            for v in NASTY {
                let mut b = base.clone();
                set_word(&mut b, w, v);
                exercise(&b, &mut t);
            }
        }
        // Words of the first bin block and hash table.
        let kafbin = i64::from(get_word(&base, 19));
        let span = 2 * 127 * 32;
        let iters = if base.len() > 200_000 { 200 } else { 1500 };
        for _ in 0..iters {
            let mut b = base.clone();
            let addr = if rng.below(2) == 0 {
                kafbin + rng.below(span) as i64
            } else {
                70 + rng.below(2048) as i64
            };
            let v = if rng.below(3) == 0 {
                rng.next() as i32
            } else {
                NASTY[rng.below(NASTY.len())]
            };
            let o = off(addr);
            if o + 4 <= b.len() {
                b[o..o + 4].copy_from_slice(&v.to_le_bytes());
            }
            exercise(&b, &mut t);
        }
        eprintln!("hostile words {name}: {t:?}");
    }
}

#[test]
fn cyclic_and_out_of_range_chains() {
    for (name, base) in bases() {
        let mut t = Tally::default();
        let kafbin = i64::from(get_word(&base, 19));
        let bnsiz = i64::from(get_word(&base, 18)).clamp(13, 127);
        let kbnblk = i64::from(get_word(&base, 16)).max(1);
        let ktable = get_word(&base, 15);
        let nbin = if ktable == 2 {
            i64::from(get_word(&base, 14))
        } else {
            kbnblk
        };
        let last_bin = kafbin + (nbin - 1) * bnsiz;
        let link_word = last_bin + bnsiz - 1;
        let kfsize = i64::from(get_word(&base, 5));
        // Block link: to itself, to the first bin of the same block, into the lock record,
        // into the header, past EOF, negative.
        for v in [
            kafbin,
            last_bin,
            kafbin - 127,
            1,
            70,
            kfsize,
            kfsize + 1,
            i64::from(i32::MAX),
            -5,
        ] {
            let mut b = base.clone();
            let o = off(link_word);
            if o + 4 <= b.len() {
                b[o..o + 4].copy_from_slice(&(v as i32).to_le_bytes());
                exercise(&b, &mut t);
            }
        }
        // Overflow cycles: mark the first bin full and point its overflow at itself or
        // the next bin, which points back.
        for target in [kafbin, kafbin + bnsiz] {
            let mut b = base.clone();
            set_word(&mut b, off(kafbin) / 4, -1);
            set_word(&mut b, off(kafbin + bnsiz - 2) / 4, target as i32);
            set_word(&mut b, off(kafbin + bnsiz) / 4, -1);
            set_word(&mut b, off(kafbin + 2 * bnsiz - 2) / 4, kafbin as i32);
            exercise(&b, &mut t);
            // Point every hash-table slot at the first bin so lookups enter the cycle.
            if ktable == 1 {
                let khash = get_word(&b, 14).clamp(1, 8192) as i64;
                for h in 1..=khash {
                    let o = off(69 + h);
                    if o + 4 <= b.len() {
                        b[o..o + 4].copy_from_slice(&(kafbin as i32).to_le_bytes());
                    }
                }
                exercise(&b, &mut t);
            }
        }
        // Every hash slot into the lock record or beyond EOF.
        if ktable == 1 {
            for v in [kafbin - 1, kfsize + 100, -7] {
                let mut b = base.clone();
                let khash = get_word(&b, 14).clamp(1, 8192) as i64;
                for h in 1..=khash {
                    let o = off(69 + h);
                    if o + 4 <= b.len() {
                        b[o..o + 4].copy_from_slice(&(v as i32).to_le_bytes());
                    }
                }
                exercise(&b, &mut t);
            }
        }
        // Bad npath / status values in the first entry of the first bin.
        for v in NASTY {
            let mut b = base.clone();
            set_word(&mut b, off(kafbin + 1) / 4, v);
            exercise(&b, &mut t);
            let mut b = base.clone();
            set_word(&mut b, off(kafbin) / 4, v);
            exercise(&b, &mut t);
        }
        eprintln!("chains {name}: {t:?}");
    }
}

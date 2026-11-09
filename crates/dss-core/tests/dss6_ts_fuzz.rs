//! Robustness of DSS6 time-series reads : hostile info-block,
//! internal-header, compression-header and data words, bit flips, and truncations. Every
//! read must return a value or a typed error; a panic fails the test.
//!
//! Bases: the synthetic sample file, plus (when present) public DSS6 samples listed in
//! `DSS6_TS_FUZZ_FILES` (`;`-separated paths; release builds, or with
//! `DSS6_TS_FUZZ_FULL` set).

mod dss6_ts_support;

use dss_core::dss6::{word_byte_offset, DatasetOptions, Dss6File, TsReadOptions};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Default, Debug)]
struct Tally {
    files: usize,
    reads: usize,
    ok: usize,
    errors: usize,
}

/// Decode every TS record of `bytes` and read each dataset whole and windowed.
fn exercise(bytes: &[u8], t: &mut Tally, datasets: bool) {
    t.files += 1;
    let Ok(f) = Dss6File::from_bytes(bytes) else {
        return;
    };
    let Ok(cat) = f.catalog() else { return };
    let opts = TsReadOptions {
        allow_sig_digits: true,
        ..TsReadOptions::default()
    };
    let mut keys = std::collections::BTreeSet::new();
    for e in cat
        .live()
        .filter(|e| (100..120).contains(&e.record_type))
        .take(64)
    {
        t.reads += 1;
        match f.read_ts_entry(e, &opts) {
            Ok(r) => {
                t.ok += 1;
                let _ = r.to_timeseries();
            }
            Err(err) => {
                t.errors += 1;
                let _ = err.to_string();
            }
        }
        keys.insert(e.pathname.clone());
    }
    if !datasets {
        return;
    }
    for k in keys.iter().take(8) {
        for o in [
            DatasetOptions::whole(),
            DatasetOptions::window(i64::MIN / 4, i64::MAX / 4),
            DatasetOptions::window(3_000_000_000, 3_000_100_000),
        ] {
            t.reads += 1;
            match f.read_ts_dataset(k, &o) {
                Ok(_) => t.ok += 1,
                Err(_) => t.errors += 1,
            }
        }
    }
}

fn targets(bytes: &[u8]) -> Vec<(i64, &'static str)> {
    // Word addresses of every TS record's info block, headers and first data words.
    let f = Dss6File::from_bytes(bytes).unwrap();
    let cat = f.catalog().unwrap();
    let mut out = Vec::new();
    for e in cat
        .live()
        .filter(|e| (100..120).contains(&e.record_type))
        .take(24)
    {
        let Ok(Some(rec)) = f.lookup(&e.pathname) else {
            continue;
        };
        let i = &rec.info;
        let p = (e.pathname.len() as i64 - 1) / 4 + 1;
        for k in [1, 2, 5, 6, 7, 8, 17, 18, 19, 20, 21, 22, 23, 24, 25] {
            out.push((i.address + if k < 3 { k } else { k + p }, "info"));
        }
        for k in 0..i.internal_header.1.clamp(0, 28) {
            out.push((i64::from(i.internal_header.0) + i64::from(k), "ihead"));
        }
        for k in 0..i.compression_header.1.clamp(0, 12) {
            out.push((i64::from(i.compression_header.0) + i64::from(k), "chead"));
        }
        for k in 0..i.data_words.clamp(0, 6) {
            out.push((i64::from(i.data_address) + i64::from(k), "data"));
        }
    }
    out
}

fn set_word(b: &mut [u8], addr: i64, v: i32) {
    if let Some(off) = word_byte_offset(addr) {
        let off = off as usize;
        if off + 4 <= b.len() {
            b[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
}

fn fuzz_base(base: &[u8], t: &mut Tally, rng: &mut Rng, heavy: bool) {
    exercise(base, t, true);
    let kfsize = i32::from_le_bytes(base[20..24].try_into().unwrap());
    let hostile = [
        0,
        -1,
        1,
        2,
        3,
        4,
        5,
        6,
        100,
        105,
        110,
        115,
        -901,
        i32::MAX,
        i32::MIN,
        1 << 20,
        kfsize,
        kfsize - 1,
        kfsize + 1,
    ];
    for (addr, _) in targets(base) {
        for &v in &hostile {
            let mut b = base.to_vec();
            set_word(&mut b, addr, v);
            exercise(&b, t, heavy && rng.next() % 8 == 0);
        }
        for _ in 0..4 {
            let mut b = base.to_vec();
            if let Some(off) = word_byte_offset(addr) {
                let o = off as usize + (rng.next() % 4) as usize;
                if o < b.len() {
                    b[o] ^= 1 << (rng.next() % 8);
                }
            }
            exercise(&b, t, false);
        }
    }
    // Truncations.
    let n = base.len();
    let cuts = if heavy { 200 } else { 60 };
    for k in 0..cuts {
        let cut = (rng.next() % n as u64) as usize;
        exercise(&base[..cut], t, k % 4 == 0);
    }
    for cut in [n - 1, n - 4, n - 512, n.saturating_sub(5000)] {
        exercise(&base[..cut.min(n)], t, true);
    }
}

#[test]
fn ts_reads_never_panic() {
    let mut t = Tally::default();
    let mut rng = Rng(0x0D55_6008);
    let base = dss6_ts_support::sample_file();
    fuzz_base(&base, &mut t, &mut rng, true);
    // `DSS6_TS_FUZZ_FILES`: `;`-separated DSS6 files to fuzz besides the synthetic base.
    let list = std::env::var("DSS6_TS_FUZZ_FILES").unwrap_or_default();
    // Debug builds fuzz the synthetic base only; release builds (or DSS6_TS_FUZZ_FULL)
    // add the external samples.
    let full = !cfg!(debug_assertions) || std::env::var_os("DSS6_TS_FUZZ_FULL").is_some();
    for p in list.split(';').filter(|s| full && !s.is_empty()) {
        let Ok(b) = std::fs::read(p) else {
            eprintln!("skip {p} (absent)");
            continue;
        };
        if b.len() > 4_000_000 {
            continue;
        }
        fuzz_base(&b, &mut t, &mut rng, false);
    }
    eprintln!("{t:?}");
    assert!(t.files > 1000);
}

//! Structural check of a DSS6 file (the DSS6 counterpart of heclib's DSS7 `zcheckFile`,
//! which has no DSS6 version): every live bin entry is reachable through the hash table
//! under its stored hash code and names a valid info block; no bin is on two chains; the
//! header counters match the bins. Read-only.

use std::collections::HashMap;
use std::path::Path;

use super::engine::{
    path_words, BIN, K_BINS, K_BNBLK, K_BNREM, K_BOVER, K_HASH, K_HUSED, K_MARK_NEW, K_MARK_OLD,
    K_NRECS,
};
use super::io::Io;
use super::{WriteError, WriteOptions};
use crate::dss6::{zhash6, EOF_FLAG, NPERM};

/// Findings of [`check_file6`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Live entries found by walking the bin blocks.
    pub live_entries: i64,
    /// Problems; empty when the file is consistent.
    pub problems: Vec<String>,
    /// Informational notes that are not errors (for example an end-of-file word other
    /// than -9630, which HEC tolerates).
    pub notes: Vec<String>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Check the structures a DSS6 writer maintains. Opens its own read-only handle.
pub fn check_file6(path: &Path) -> Result<CheckReport, WriteError> {
    let f = std::fs::File::open(path)?;
    let opts = WriteOptions::default();
    let eng = super::engine::Engine::new(&f, &opts)?;
    let io = Io::new(&f);
    let h = eng.h;
    let mut r = CheckReport::default();
    let kfs = i64::from(h[super::engine::K_FSIZE]);
    if h[K_MARK_NEW] != 0 || h[K_MARK_OLD] != 0 {
        r.problems.push(format!(
            "in-progress marker set (words 67/68 = {}/{})",
            h[K_MARK_OLD], h[K_MARK_NEW]
        ));
    }
    let len = io.len()?;
    let expect = (kfs as u64).div_ceil(127) * 512;
    if len != expect {
        r.notes
            .push(format!("file is {len} bytes, KFSIZE implies {expect}"));
    }
    match io.read_words(kfs, 1) {
        Ok(w) if w[0] == EOF_FLAG => {}
        Ok(w) => r
            .notes
            .push(format!("word at KFSIZE is {} (not -9630)", w[0])),
        Err(_) => r.notes.push("no word at KFSIZE".into()),
    }
    // Chains from the table.
    let khash = h[K_HASH] as usize;
    let table = io.read_words(NPERM as i64 + 1, khash)?;
    let mut owner: HashMap<i64, usize> = HashMap::new();
    let mut reachable: HashMap<Vec<u8>, i64> = HashMap::new();
    for (i, &a0) in table.iter().enumerate() {
        let hash = i + 1;
        let mut a = i64::from(a0);
        let mut hops = 0;
        while a != 0 {
            hops += 1;
            if hops > 100_000 {
                r.problems.push(format!("hash {hash}: chain cycle"));
                break;
            }
            if let Some(&o) = owner.get(&a) {
                r.problems.push(format!(
                    "bin {a} is on the chains of hash {o} and hash {hash}"
                ));
                break;
            }
            owner.insert(a, hash);
            let b = match io.read_words(a, BIN) {
                Ok(b) => b,
                Err(_) => {
                    r.problems
                        .push(format!("hash {hash}: bin {a} outside the file"));
                    break;
                }
            };
            let mut j = 1usize;
            let mut next = 0i64;
            while j <= BIN - 2 {
                let st = b[j - 1];
                if st == 0 {
                    break;
                }
                if st == -1 {
                    next = i64::from(b[BIN - 2]);
                    break;
                }
                let np = b[j] as usize;
                if !(1..=392).contains(&np) {
                    r.problems
                        .push(format!("bin {a} word {j}: pathname length {np}"));
                    break;
                }
                let nbw = path_words(np);
                // The entry (status .. KBPINTL) must lie inside the bin (zcheck6 never
                // places one past word KBNSIZ - 2).
                if j + nbw + 10 > BIN {
                    r.problems
                        .push(format!("bin {a} word {j}: entry passes the end of the bin"));
                    break;
                }
                if st == 1 || st == 11 {
                    let p: Vec<u8> = b[j + 1..j + 1 + nbw]
                        .iter()
                        .flat_map(|w| w.to_le_bytes())
                        .take(np)
                        .collect();
                    reachable.insert(p, a);
                }
                j += nbw + 11;
            }
            a = next;
        }
    }
    // Walk the bin blocks as zcatfi6 does.
    let kbnblk = i64::from(h[K_BNBLK]);
    let mut addr = i64::from(h[super::engine::K_AFBIN]);
    let (mut live, mut over, mut blocks) = (0i64, 0i64, 0i64);
    let kanbin = i64::from(h[super::engine::K_ANBIN]);
    let mut allocated_blocks = 0i64;
    'blocks: loop {
        blocks += 1;
        if blocks > kfs / (kbnblk * 127) + 2 {
            r.problems.push("bin block chain cycle".into());
            break;
        }
        if (addr..addr + kbnblk * BIN as i64).contains(&kanbin) {
            allocated_blocks = blocks;
        }
        let mut last = Vec::new();
        for k in 0..kbnblk {
            let a = addr + k * BIN as i64;
            let b = match io.read_words(a, BIN) {
                Ok(b) => b,
                Err(_) => {
                    r.problems.push(format!("bin {a} outside the file"));
                    break 'blocks;
                }
            };
            let mut j = 1usize;
            while j <= BIN - 2 {
                let st = b[j - 1];
                if st == 0 || st == -1 {
                    break;
                }
                let np = b[j] as usize;
                if !(1..=392).contains(&np) {
                    r.problems
                        .push(format!("bin {a} word {j}: pathname length {np}"));
                    break;
                }
                let nbw = path_words(np);
                // The entry (status .. KBPINTL) must lie inside the bin (zcheck6 never
                // places one past word KBNSIZ - 2).
                if j + nbw + 10 > BIN {
                    r.problems
                        .push(format!("bin {a} word {j}: entry passes the end of the bin"));
                    break;
                }
                if st == 1 || st == 11 {
                    live += 1;
                    let p: Vec<u8> = b[j + 1..j + 1 + nbw]
                        .iter()
                        .flat_map(|w| w.to_le_bytes())
                        .take(np)
                        .collect();
                    let name = String::from_utf8_lossy(&p).into_owned();
                    let stored_hash = b[j + nbw + 8];
                    if zhash6(&p, h[K_HASH]) != stored_hash {
                        r.problems
                            .push(format!("{name}: stored hash {stored_hash} differs"));
                    }
                    if reachable.get(&p) != Some(&a) {
                        r.problems.push(format!(
                            "{name}: entry in bin {a} is not reachable by lookup"
                        ));
                    }
                    let ia = i64::from(b[j + 1 + nbw]);
                    match io.read_words(ia, 3 + nbw) {
                        Ok(info) if info[0] == -9753 && info[2] as usize == np => {
                            let ip: Vec<u8> = info[3..3 + nbw]
                                .iter()
                                .flat_map(|w| w.to_le_bytes())
                                .take(np)
                                .collect();
                            if ip != p {
                                r.problems
                                    .push(format!("{name}: info block names another path"));
                            }
                            if ia >= kfs {
                                r.problems.push(format!("{name}: info block past KFSIZE"));
                            }
                        }
                        _ => r.problems.push(format!("{name}: bad info block at {ia}")),
                    }
                }
                j += nbw + 11;
            }
            if b[BIN - 2] != 0 {
                over += 1;
            }
            last = b;
        }
        let next = i64::from(last[BIN - 1]);
        if next == 0 {
            break;
        }
        addr = next;
    }
    r.live_entries = live;
    let used = table.iter().filter(|&&a| a != 0).count() as i32;
    let checks = [
        ("KNRECS", h[K_NRECS] as i64, live),
        ("KHUSED", i64::from(h[K_HUSED]), i64::from(used)),
        ("KBOVER", i64::from(h[K_BOVER]), over),
        (
            "KBINS",
            i64::from(h[K_BINS]),
            (if allocated_blocks > 0 {
                allocated_blocks
            } else {
                blocks
            }) * kbnblk
                - i64::from(h[K_BNREM]),
        ),
    ];
    for (name, got, want) in checks {
        if got != want {
            r.problems
                .push(format!("{name} is {got}, the bins give {want}"));
        }
    }
    Ok(r)
}

//! Differential tests: the memoized hash-chain placement and borrowed-key duplicate
//! grouping ([`ChainMode::Memo`]) against the earlier per-entry walk and grouping
//! ([`ChainMode::Reference`]). The whole catalog (entries, every `meta` field including
//! `chain` hops and `duplicate_of`, recovered flags, stats, anomalies in order with their
//! text) is compared through its `Debug` form, for the default, recovery and
//! info-checked catalogs of every public DSS7 fixture and, when `DSS7_CHAIN_GOLDEN_DIR`
//! names a directory of DSS7 files, of those files too. Traversal counters show the work
//! is bounded.

use super::*;
use crate::header::{DssVersion, FileSignature};
use std::path::{Path, PathBuf};

fn dss7_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dss")))
        .filter(|p| {
            let mut head = vec![0u8; 4096];
            let n = {
                use std::io::Read;
                let mut f = std::fs::File::open(p)
                    .unwrap_or_else(|e| panic!("cannot open {}: {e}", p.display()));
                f.read(&mut head).expect("read header")
            };
            matches!(
                FileSignature::from_bytes(&head[..n]),
                Ok(FileSignature {
                    version: DssVersion::V7,
                    ..
                })
            )
        })
        .collect();
    v.sort();
    v
}

/// Length plus two independent 64-bit hashes of the `Debug` form, streamed (the form of
/// a large catalog exceeds a gigabyte).
fn digest(c: &Dss7Catalog) -> (u64, u64, u64) {
    use std::fmt::Write as _;
    use std::hash::Hasher as _;
    struct D(u64, std::hash::DefaultHasher, u64);
    impl std::fmt::Write for D {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            for &b in s.as_bytes() {
                self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
            }
            self.1.write(s.as_bytes());
            self.2 += s.len() as u64;
            Ok(())
        }
    }
    let mut d = D(0xcbf2_9ce4_8422_2325, std::hash::DefaultHasher::new(), 0);
    write!(d, "{c:?}").expect("format");
    (d.2, d.0, d.1.finish())
}

fn variants() -> [(&'static str, CatalogOptions); 4] {
    [
        ("default", CatalogOptions::default()),
        (
            "recover",
            CatalogOptions {
                recover_unlinked: true,
                ..CatalogOptions::default()
            },
        ),
        (
            "recover-unchecked",
            CatalogOptions {
                check_hashes: false,
                recover_unlinked: true,
                ..CatalogOptions::default()
            },
        ),
        (
            "info",
            CatalogOptions {
                check_info: InfoCheck::LiveUpTo(5000),
                ..CatalogOptions::default()
            },
        ),
    ]
}

/// Compare both modes on one file; return (memo, reference) work for the default catalog.
fn compare_file(p: &Path) -> (ChainWork, ChainWork, u64) {
    let mut f = Dss7File::open(p).unwrap_or_else(|e| panic!("{}: open: {e}", p.display()));
    let mut out = None;
    for (label, opts) in variants() {
        let (a, wa) = walk_measured(&mut f, &opts, ChainMode::Memo)
            .unwrap_or_else(|e| panic!("{} {label}: memo catalog: {e}", p.display()));
        let (b, wb) = walk_measured(&mut f, &opts, ChainMode::Reference)
            .unwrap_or_else(|e| panic!("{} {label}: reference catalog: {e}", p.display()));
        let (da, db) = (digest(&a), digest(&b));
        assert!(
            da == db,
            "{} {label}: catalogs differ ({da:?} / {db:?})",
            p.display()
        );
        assert_eq!(wa.queries, wb.queries, "{} {label}", p.display());
        // Bounded work: one recorded walk per distinct chain start, each at most the
        // per-entry walk's limit; the fallback is the per-entry walk itself.
        let limit = a.stats.bins + a.stats.unlisted_bins + 2;
        assert!(wa.recorded <= wa.queries, "{} {label}", p.display());
        assert!(
            wa.steps <= (wa.recorded + wa.fallbacks) * limit,
            "{} {label}: {wa:?}",
            p.display()
        );
        if label == "default" {
            out = Some((wa, wb, a.stats.bins));
        }
    }
    out.unwrap()
}

#[test]
fn public_fixtures_memo_equals_reference() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    let files = dss7_files(&dir);
    assert!(files.len() >= 10, "only {} DSS7 fixtures", files.len());
    let mut chains_fixture = false;
    for p in &files {
        let (memo, reference, _) = compare_file(p);
        if p.file_name().is_some_and(|n| n == "synthetic_chains7.dss") {
            chains_fixture = true;
            // Merged chains, a loop and foreign-slot entries: the memo walks each start
            // once, the per-entry walk repeats loops up to its bound.
            assert!(memo.steps * 4 < reference.steps, "{memo:?} {reference:?}");
            assert_eq!(memo.fallbacks, 0);
        }
    }
    assert!(chains_fixture, "synthetic_chains7.dss missing");
}

/// External DSS7 files (opt-in, run with `--release`): `DSS7_CHAIN_GOLDEN_DIR` names
/// a directory of `.dss` copies; every DSS7 file is compared in all variants. A DSS7
/// file that does not open or catalog fails. Prints counts only.
#[test]
fn golden_copies_memo_equals_reference() {
    let Some(dir) = std::env::var_os("DSS7_CHAIN_GOLDEN_DIR") else {
        eprintln!("DSS7_CHAIN_GOLDEN_DIR not set; skipped");
        return;
    };
    let files = dss7_files(Path::new(&dir));
    assert!(!files.is_empty(), "no DSS7 files in DSS7_CHAIN_GOLDEN_DIR");
    for p in &files {
        let (m, r, bins) = compare_file(p);
        eprintln!(
            "{}: bins {bins}, live placed {}, chain steps memo {} / per-entry {}, starts {}, fallbacks {}",
            p.file_stem().unwrap().to_string_lossy(),
            m.queries,
            m.steps,
            r.steps,
            m.recorded,
            m.fallbacks
        );
    }
}

#[test]
// The reference is the earlier key: an upper-cased copy.
#[allow(clippy::manual_ignore_case_cmp)]
fn caseless_key_groups_like_uppercase_copies() {
    use std::collections::HashSet;
    let keys: [&[u8]; 6] = [
        b"/A/b/C//1HOUR/x/",
        b"/a/B/c//1hour/X/",
        b"/A/B/C//1HOUR/Y/",
        b"",
        b"\xe9",
        b"\xc9",
    ];
    for x in keys {
        for y in keys {
            assert_eq!(
                CaselessKey(x) == CaselessKey(y),
                x.to_ascii_uppercase() == y.to_ascii_uppercase()
            );
        }
    }
    let set: HashSet<CaselessKey<'_>> = keys.iter().map(|k| CaselessKey(k)).collect();
    // Two spellings of one pathname collapse; Latin-1 bytes are not case-folded.
    assert_eq!(set.len(), 5);
    // Keys longer than the 64-byte hashing chunk.
    let long_a: Vec<u8> = (0..300).map(|i| b'a' + (i % 26) as u8).collect();
    let long_b = long_a.to_ascii_uppercase();
    let set: HashSet<CaselessKey<'_>> = [CaselessKey(&long_a), CaselessKey(&long_b)].into();
    assert_eq!(set.len(), 1);
}

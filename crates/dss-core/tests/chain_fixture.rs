//! P63 fixture `fixtures/public/synthetic_chains7.dss`: a DSS7 file whose hash chains are
//! malformed in every way the catalog's chain check distinguishes, so the memoized chain
//! placement (`src/chain.rs`) is compared with the per-entry walk on them
//! (`src/dss7/walk_tests.rs` runs every public fixture in both modes).
//!
//! Built with `dss7_common::build` (no HEC software) from 8 table slots of 4-5 bins each,
//! then edited:
//!
//! - slot A's last bin links to slot B's first bin (two chains merge);
//! - slot C's last bin links back to its first bin (a loop);
//! - slot B points at C's first bin, so B's entries are reachable only through A
//!   (`ChainState::OtherSlot`) and their own walk goes round C's loop;
//! - slot D's last bin links past the end of the file and slot E's last bin to a zeroed
//!   info block (dangling links, one to an all-zero bin);
//! - three orphan-bin entries, an orphan lower-case duplicate, a deleted entry.
//!
//! To regenerate the committed fixture, set `DSS_PY_FIXTURE_OUT` to a scratch directory,
//! run this test and copy the file.

mod dss7_common;

use dss7_common::{build, get_word, set_word, Rec, Spec, HT};
use dss_core::dss7::{CatalogOptions, ChainState, Dss7File};
use dss_core::AnomalyKind;

const BS: i64 = 40;
const MH: i64 = 8;

fn spec() -> Spec {
    let mut recs = Vec::new();
    for i in 0..96 {
        recs.push(Rec::live(
            &format!("/P63/LOC{i:02}/FLOW/01Jan2020/1Hour/RUN/"),
            100,
        ));
    }
    // A zeroed info block: the target of a dangling link.
    recs.push(Rec::live("/P63/ZERO/FLOW/01Jan2020/1Hour/RUN/", 100).zero_info());
    for i in 0..3 {
        recs.push(Rec::live(&format!("/P63/ORPHAN{i}/FLOW/01Jan2020/1Hour/RUN/"), 100).orphan());
    }
    recs.push(Rec::live("/p63/loc05/flow/01jan2020/1hour/run/", 100).orphan());
    recs.push(Rec::live("/P63/GONE/FLOW/01Jan2020/1Hour/RUN/", 100).with_status(11));
    Spec {
        max_hash: MH,
        bin_size: BS,
        bins_per_block: 4,
        recs,
        ..Spec::default()
    }
}

/// Bins of the chain that table slot `k` starts, following overflow links.
fn chain(b: &[u8], k: i64) -> Vec<i64> {
    let mut v = Vec::new();
    let mut a = get_word(b, (HT + k) as usize);
    while a != 0 && !v.contains(&a) && v.len() < 100 {
        v.push(a);
        a = get_word(b, (a + BS - 1) as usize);
    }
    v
}

fn generate() -> Vec<u8> {
    let s = spec();
    let built = build(&s);
    let mut b = built.bytes.clone();
    let mut slots: Vec<i64> = (0..MH).filter(|&k| chain(&b, k).len() >= 2).collect();
    slots.truncate(5);
    assert_eq!(slots.len(), 5, "need five multi-bin chains");
    let [a, bb, c, d, e] = [slots[0], slots[1], slots[2], slots[3], slots[4]];
    let (ca, cb, cc, cd, ce) = (
        chain(&b, a),
        chain(&b, bb),
        chain(&b, c),
        chain(&b, d),
        chain(&b, e),
    );
    let link = |b: &mut Vec<u8>, bin: i64, to: i64| set_word(b, (bin + BS - 1) as usize, to);
    link(&mut b, *ca.last().unwrap(), cb[0]);
    link(&mut b, *cc.last().unwrap(), cc[0]);
    set_word(&mut b, (HT + bb) as usize, cc[0]);
    let eof = get_word(&b, 5);
    link(&mut b, *cd.last().unwrap(), eof + 1000);
    let zero = built.info_addr[96];
    assert_eq!(get_word(&b, zero as usize), 0);
    link(&mut b, *ce.last().unwrap(), zero);
    b
}

#[test]
fn chains_fixture_is_current() {
    let bytes = generate();
    assert_eq!(bytes, generate(), "generation is not deterministic");
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_chains7.dss"), &bytes).unwrap();
    }
    let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_chains7.dss");
    let c = std::fs::read(&committed).unwrap_or_else(|e| panic!("{}: {e}", committed.display()));
    assert!(
        c == bytes,
        "committed synthetic_chains7.dss differs from the generator"
    );
}

#[test]
fn chains_fixture_anomalies() {
    let bytes = generate();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let cat = f.catalog().unwrap();
    let kinds = |k: AnomalyKind| cat.catalog.anomalies.iter().filter(|a| a.kind == k).count();
    let has = |s: &str| cat.catalog.anomalies.iter().any(|a| a.detail.contains(s));
    assert!(kinds(AnomalyKind::CycleDetected) >= 1);
    assert!(has("overflow chains loop"));
    assert!(has("hash chains share bins"));
    assert!(has(
        "2 hash-table or overflow links point outside the walked bins (1 to empty bins"
    ));
    assert_eq!(cat.stats.dangling_links, 2);
    assert_eq!(cat.stats.dangling_zeroed_bins, 1);
    let state = |pred: fn(&ChainState) -> bool| cat.meta.iter().filter(|m| pred(&m.chain)).count();
    let other = state(|c| matches!(c, ChainState::OtherSlot));
    let orphan = state(|c| matches!(c, ChainState::Orphan));
    let deep = state(|c| matches!(c, ChainState::OwnChain(h) if *h >= 3));
    assert!(other >= 6, "{other} foreign-slot entries");
    assert_eq!(orphan, 4);
    assert!(deep >= 3, "{deep} entries at hop >= 3");
    assert_eq!(cat.stats.orphan_entries, 4);
    assert_eq!(cat.stats.table_chain_miss as usize, other + orphan);
    assert_eq!(cat.stats.duplicate_pathnames, 1);
    assert_eq!(cat.stats.deleted, 1);
    // Recovery mode and lookups terminate on the loop and the dangling links.
    let rec = f
        .catalog_with(&CatalogOptions {
            recover_unlinked: true,
            ..CatalogOptions::default()
        })
        .unwrap();
    assert_eq!(rec.stats.dangling_links, 2);
    for e in cat.catalog.live() {
        let _ = f.lookup(&e.pathname);
    }
}

//! P06 checks on the public DSS7 fixtures (HEC `sample7.dss`, RasExamples
//! `BaldEagleDamBrk.dss`): golden offsets (check 2), hash self-check (3), point lookup (5),
//! read-only guarantee (6) and catalog metadata against oracle values (8).
//!
//! Fixtures are looked up in `fixtures/public/` and then in `DSS_GOLDEN_DIR` (the P02
//! golden copies); each test skips when its fixture is absent. Expected values come from
//! the P02 oracle dumps (`monolith/catalog.json`, `hecdss/records/*.json`).

mod dss7_common;

use dss_core::dss7::{CatalogOptions, ChainState, Dss7File, InfoCheck};
use std::path::Path;

fn full() -> CatalogOptions {
    CatalogOptions {
        check_info: InfoCheck::Live,
        ..CatalogOptions::default()
    }
}

fn check_lookup_all(path: &Path) -> usize {
    let mut f = Dss7File::open(path).unwrap();
    let cat = f.catalog_with(&full()).unwrap();
    assert!(
        cat.catalog.anomalies.is_empty(),
        "{:?}",
        &cat.catalog.anomalies[..cat.catalog.anomalies.len().min(5)]
    );
    assert_eq!(
        cat.stats.hash_mismatch_live + cat.stats.hash_mismatch_nonlive,
        0
    );
    assert_eq!(cat.stats.table_chain_miss, 0);
    let mut n = 0;
    for (e, m) in cat.catalog.entries.iter().zip(&cat.meta) {
        assert!(matches!(m.chain, ChainState::OwnChain(_)));
        let lower = e.pathname.to_ascii_lowercase();
        let r = f
            .lookup(&lower)
            .unwrap()
            .expect("live entry found by lookup");
        assert_eq!(r.info_address as u64 * 8, e.info_address);
        assert_eq!(r.info.flag, -97534);
        assert_eq!(r.info.hash, e.stored_hash);
        assert_eq!(r.info.pathname, e.pathname);
        assert_eq!(r.record_type, e.record_type);
        n += 1;
    }
    n
}

#[test]
fn sample7_catalog_and_lookup() {
    let Some(p) = dss7_common::sample7() else {
        eprintln!("sample7.dss not available; skipped");
        return;
    };
    let mut f = Dss7File::open(&p).unwrap();
    let cat = f.catalog().unwrap();
    assert_eq!(cat.stats.live, 594);
    assert_eq!(
        cat.catalog.live().filter(|e| e.record_type == 100).count(),
        590
    );
    assert_eq!(
        cat.catalog.live().filter(|e| e.record_type == 200).count(),
        4
    );
    assert_eq!(check_lookup_all(&p), 594);

    // Check 8 against Monolith / hecdss for one record.
    let r = f
        .lookup(b"//SACRAMENTO/PRECIP-INC/01Jan1877/1Day/OBS/")
        .unwrap()
        .unwrap();
    assert_eq!(r.info_address, 62354); // Monolith info_address
    assert_eq!(r.record_type, 100);
    assert_eq!(r.info.status, 0, "7-10 writes info status 0");
    assert_eq!(r.info.last_write_ms / 1000, 1_249_981); // Monolith last_write_time_value
                                                        // First / last valid value: 12Jul1877 00:00 and 01Jan1878 00:00 (24:00 convention).
    assert_eq!(r.info.first_date, (-8208, 86400));
    assert_eq!(r.info.last_date, (-8035, 86400));
    let e = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == b"//SACRAMENTO/PRECIP-INC/01Jan1877/1Day/OBS/")
        .unwrap();
    assert_eq!(
        (cat.meta[e].julian_first, cat.meta[e].julian_last),
        (Some(-8208), Some(-8035))
    );
}

#[test]
fn bald_eagle_catalog_and_lookup() {
    let Some(p) = dss7_common::bald_eagle() else {
        eprintln!("BaldEagleDamBrk.dss not available; skipped");
        return;
    };
    let mut f = Dss7File::open(&p).unwrap();
    let cat = f.catalog().unwrap();
    // P04 golden offsets: 102 records in 4 bin blocks, first info block at word 15773.
    assert_eq!(cat.stats.live, 102);
    assert_eq!(cat.stats.blocks, 4);
    assert_eq!(cat.catalog.entries[0].info_address, 15773 * 8);
    assert!(cat.catalog.live().all(|e| e.record_type == 100));
    assert_eq!(check_lookup_all(&p), 102);

    let path = b"/BCLINE/BaldEagleCr: DS2NormalD/FLOW/08Sep2018/1Minute/Grid Precip Infiltration/";
    let r = f.lookup(path).unwrap().unwrap();
    assert_eq!(r.info_address, 50056);
    assert_eq!(r.info.last_write_ms / 1000, 1_647_007_371);
    // The info stamps are the first / last *valid* value (dss7.md §5.1). hecdss reports
    // the block window 2018-09-08T00:01 .. 2018-09-09T00:00; only the last value
    // (08Sep2018 24:00) is non-missing in this record.
    assert_eq!(r.info.first_date, (43350, 86400));
    assert_eq!(r.info.last_date, (43350, 86400));
    assert_eq!(r.info.logical_number, 1440);
    assert_eq!(r.info.program, "not specified");
}

#[test]
fn reader_never_modifies_files() {
    // Check 6: bytes, length and mtime unchanged after open, catalog and lookups.
    for p in [dss7_common::sample7(), dss7_common::bald_eagle()]
        .into_iter()
        .flatten()
    {
        let before = std::fs::read(&p).unwrap();
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        {
            let mut f = Dss7File::open(&p).unwrap();
            let cat = f.catalog_with(&full()).unwrap();
            for e in cat.catalog.live().take(50) {
                f.lookup(&e.pathname).unwrap();
            }
        }
        let after = std::fs::read(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), mtime);
        assert!(before == after, "file bytes changed");
    }
}

#[test]
fn in_memory_and_file_readers_agree() {
    let Some(p) = dss7_common::bald_eagle() else {
        return;
    };
    let bytes = std::fs::read(&p).unwrap();
    let a = Dss7File::from_bytes(&bytes).unwrap().catalog().unwrap();
    let b = Dss7File::open(&p).unwrap().catalog().unwrap();
    assert_eq!(a.catalog.entries, b.catalog.entries);
}

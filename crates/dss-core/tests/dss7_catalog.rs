//! Catalog, hash and lookup tests on synthetic DSS7 files
//! and the anomaly kinds found in real files (orphan bins, zeroed info blocks, duplicate
//! live pathnames, record-count mismatch, loops).

mod dss7_common;

use dss7_common::{build, get_word, set_word, standard_spec, Rec, Spec};
use dss_core::dss7::{CatalogOptions, ChainState, Dss7Error, Dss7File, InfoCheck, InfoProblem};
use dss_core::{AnomalyKind, RecordStatus};

fn full() -> CatalogOptions {
    CatalogOptions {
        check_info: InfoCheck::Live,
        ..CatalogOptions::default()
    }
}

fn count(cat: &dss_core::Catalog, k: AnomalyKind) -> usize {
    cat.anomalies.iter().filter(|a| a.kind == k).count()
}

#[test]
fn standard_file_catalog_and_statuses() {
    let spec = standard_spec();
    let built = build(&spec);
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog_with(&full()).unwrap();
    assert!(
        cat.catalog.anomalies.is_empty(),
        "{:?}",
        cat.catalog.anomalies
    );
    assert_eq!(cat.stats.entries as usize, spec.recs.len());
    assert_eq!(cat.stats.live, 42); // 40 RTS + 1 PD + 1 undeleted (status 0)
    assert_eq!(cat.stats.undeleted, 1);
    assert_eq!(cat.stats.aliases, 1);
    assert_eq!(cat.stats.deleted, 1);
    assert_eq!(cat.stats.renamed, 1);
    assert!(
        cat.stats.blocks > 1,
        "fixture should span several bin blocks"
    );
    assert_eq!(cat.stats.info_checked, 42);
    // Every entry kept, in bin order, with raw status and bin/info addresses.
    let by_path = |p: &str| {
        cat.catalog
            .entries
            .iter()
            .find(|e| e.pathname == p.as_bytes())
            .unwrap()
    };
    assert_eq!(
        by_path("/BASIN/OLD/FLOW/01Jan2020/1Hour/RUN/").status,
        RecordStatus::Deleted
    );
    assert_eq!(
        by_path("/BASIN/RENAMED/FLOW/01Jan2020/1Hour/RUN/").status,
        RecordStatus::Renamed
    );
    let u = by_path("/BASIN/UNDELETED/FLOW/01Jan2020/1Day/RUN/");
    assert_eq!((u.status, u.raw_status), (RecordStatus::Live, 0));
    assert_eq!(
        by_path("/BASIN/ALIAS/STAGE-FLOW///RATING/").status,
        RecordStatus::Alias
    );
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        let k = spec.recs.iter().position(|r| r.path == e.pathname).unwrap();
        assert_eq!(e.bin_address, built.entry_addr[k] as u64 * 8);
        assert_eq!(e.record_type, spec.recs[k].rtype);
        assert_eq!(e.last_write_ms, Some(spec.recs[k].last_write_ms));
        let m = &cat.meta[i];
        if (100..200).contains(&e.record_type) {
            assert_eq!((m.julian_first, m.julian_last), (Some(43350), Some(43351)));
        } else {
            assert_eq!(m.julian_first, None);
        }
        if e.is_live() {
            assert!(matches!(m.chain, ChainState::OwnChain(_)));
            let info = m.info.unwrap();
            assert_eq!(info.problem, None);
            assert_eq!(info.record_type, e.record_type);
            assert_eq!(info.last_write_ms, spec.recs[k].last_write_ms);
        }
    }
    assert!(
        cat.meta
            .iter()
            .any(|m| matches!(m.chain, ChainState::OwnChain(h) if h > 0)),
        "need overflow bins"
    );
}

#[test]
fn lookup_is_case_insensitive_and_matches_scan() {
    let built = build(&standard_spec());
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog().unwrap();
    for e in cat.catalog.entries.iter().filter(|e| e.is_live()) {
        for variant in [
            e.pathname.clone(),
            e.pathname.to_ascii_lowercase(),
            e.pathname.to_ascii_uppercase(),
        ] {
            let r = f.lookup(&variant).unwrap().expect("found");
            assert_eq!(r.info_address as u64 * 8, e.info_address);
            assert_eq!(r.bin_entry_address as u64 * 8, e.bin_address);
            assert_eq!(r.info.flag, -97534);
            assert_eq!(r.info.hash, e.stored_hash);
            assert_eq!(r.info.pathname, e.pathname);
            assert_eq!(r.pathname, e.pathname);
        }
    }
    // Deleted and renamed entries are not found; nor are absent names.
    assert!(f
        .lookup(b"/BASIN/OLD/FLOW/01Jan2020/1Hour/RUN/")
        .unwrap()
        .is_none());
    assert!(f
        .lookup(b"/BASIN/RENAMED/FLOW/01Jan2020/1Hour/RUN/")
        .unwrap()
        .is_none());
    assert!(f
        .lookup(b"/NOT/THERE/FLOW/01Jan2020/1Hour/RUN/")
        .unwrap()
        .is_none());
    assert!(matches!(f.lookup(b""), Err(Dss7Error::BadPathname)));
    assert!(matches!(
        f.lookup(&[b'/'; 400]),
        Err(Dss7Error::BadPathname)
    ));
    // Alias resolves to the primary's info block.
    let alias = f
        .lookup(b"/basin/alias/stage-flow///rating/")
        .unwrap()
        .unwrap();
    let primary = f
        .lookup(b"/BASIN/CURVE/STAGE-FLOW///RATING/")
        .unwrap()
        .unwrap();
    assert_eq!(alias.status, 2);
    assert_eq!(alias.info_address, primary.info_address);
}

#[test]
fn orphan_bin_detected_and_not_found_by_lookup() {
    let mut spec = standard_spec();
    spec.recs
        .push(Rec::live("/BASIN/ORPHAN/FLOW/01Jan2020/1Hour/RUN/", 100).orphan());
    let built = build(&spec);
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog().unwrap();
    assert_eq!(cat.stats.orphan_entries, 1);
    assert_eq!(cat.stats.orphan_bins, 1);
    assert_eq!(cat.stats.table_chain_miss, 1);
    assert_eq!(count(&cat.catalog, AnomalyKind::OrphanBin), 1);
    // HEC's catalog lists it (it is live in a bin) but a lookup cannot reach it.
    assert!(cat
        .catalog
        .live()
        .any(|e| e.pathname == b"/BASIN/ORPHAN/FLOW/01Jan2020/1Hour/RUN/"));
    assert!(f
        .lookup(b"/BASIN/ORPHAN/FLOW/01Jan2020/1Hour/RUN/")
        .unwrap()
        .is_none());
}

#[test]
fn duplicate_live_pathname_prefers_hash_reachable_entry() {
    let mut spec = standard_spec();
    let p = "/BASIN/LOC3/FLOW/01Jan2020/1Hour/RUN0/";
    let mut stale = Rec::live(&p.to_ascii_lowercase(), 100).orphan();
    stale.last_write_ms = 1;
    spec.recs.push(stale);
    let built = build(&spec);
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog().unwrap();
    assert_eq!(cat.stats.duplicate_pathnames, 1);
    assert_eq!(cat.stats.duplicate_entries, 1);
    assert_eq!(count(&cat.catalog, AnomalyKind::DuplicatePathname), 1);
    let resolved = cat.resolved_live();
    let winners: Vec<_> = resolved
        .iter()
        .filter(|&&i| {
            cat.catalog.entries[i]
                .pathname
                .eq_ignore_ascii_case(p.as_bytes())
        })
        .collect();
    assert_eq!(winners.len(), 1);
    let w = &cat.catalog.entries[*winners[0]];
    assert_eq!(w.pathname, p.as_bytes());
    let r = f.lookup(p.as_bytes()).unwrap().unwrap();
    assert_eq!(r.bin_entry_address as u64 * 8, w.bin_address);
}

#[test]
fn zeroed_info_block_reported_and_lookup_errors() {
    let mut spec = standard_spec();
    spec.recs
        .push(Rec::live("/BASIN/ZERO/FLOW/01Jan2020/1Hour/RUN/", 100).zero_info());
    spec.number_records = Some(50); // header ahead of the walk, an interrupted-write signature (dss7.md §6.4)
    let built = build(&spec);
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog_with(&full()).unwrap();
    assert_eq!(cat.stats.info_bad, 1);
    assert_eq!(cat.stats.info_zeroed, 1);
    assert_eq!(count(&cat.catalog, AnomalyKind::BadInfoBlock), 1);
    assert_eq!(count(&cat.catalog, AnomalyKind::RecordCountMismatch), 1);
    // Without info checks the catalog still lists it (HEC behaviour).
    let fast = f.catalog().unwrap();
    assert_eq!(count(&fast.catalog, AnomalyKind::BadInfoBlock), 0);
    assert!(matches!(
        f.lookup(b"/BASIN/ZERO/FLOW/01Jan2020/1Hour/RUN/"),
        Err(Dss7Error::BadInfoBlock {
            problem: InfoProblem::Zeroed,
            ..
        })
    ));
}

#[test]
fn hash_mismatch_reported() {
    let built = build(&standard_spec());
    let mut b = built.bytes.clone();
    let e = built.entry_addr[5] as usize;
    let h = get_word(&b, e);
    set_word(&mut b, e, h ^ 0x10);
    let mut f = Dss7File::from_bytes(&b).unwrap();
    let cat = f.catalog().unwrap();
    assert_eq!(cat.stats.hash_mismatch_live, 1);
    assert_eq!(count(&cat.catalog, AnomalyKind::HashMismatch), 1);
}

#[test]
fn looping_structures_are_bounded() {
    let spec = standard_spec();
    let built = build(&spec);
    let h = Dss7File::from_bytes(&built.bytes).unwrap().header().clone();
    // Block link pointing back at the first block.
    let mut b = built.bytes.clone();
    let link = (h.first_bin_address + h.bin_size * h.bins_per_block) as usize;
    assert_ne!(get_word(&b, link), 0);
    set_word(&mut b, link, h.first_bin_address);
    let cat = Dss7File::from_bytes(&b).unwrap().catalog().unwrap();
    assert!(count(&cat.catalog, AnomalyKind::CycleDetected) >= 1);
    // Overflow link pointing at itself: catalog and lookup both terminate.
    let mut b = built.bytes.clone();
    let first = h.first_bin_address as usize;
    set_word(&mut b, first + h.bin_size as usize - 1, first as i64);
    let mut f = Dss7File::from_bytes(&b).unwrap();
    let cat = f.catalog().unwrap();
    assert!(count(&cat.catalog, AnomalyKind::CycleDetected) >= 1);
    for e in cat.catalog.live() {
        let _ = f.lookup(&e.pathname).unwrap();
    }
    assert!(f.lookup(b"/X/Y/Z/01Jan2020/1Hour/W/").is_ok());
}

#[test]
fn empty_file_with_no_records() {
    let built = build(&Spec::default());
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog_with(&full()).unwrap();
    assert!(cat.catalog.entries.is_empty());
    assert!(cat.catalog.anomalies.is_empty());
    assert!(f.lookup(b"/A/B/C/D/E/F/").unwrap().is_none());
}

#[test]
fn bin_size_150_and_odd_hash_size() {
    // Header variants seen in real files: kmaxHash 16348 (not a power of two) and bin size 150.
    let mut spec = standard_spec();
    spec.max_hash = 16348;
    spec.bin_size = 150;
    let built = build(&spec);
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let cat = f.catalog_with(&full()).unwrap();
    assert!(
        cat.catalog.anomalies.is_empty(),
        "{:?}",
        cat.catalog.anomalies
    );
    for e in cat.catalog.live() {
        assert!(f.lookup(&e.pathname).unwrap().is_some());
    }
}

#[test]
fn unlinked_block_entries_are_counted_and_reachable_by_lookup() {
    // Seen in one private 7-IQ test file: hash slots point into bin blocks that the
    // block chain no longer reaches. HEC's catalog skips them; a lookup still finds them.
    let spec = standard_spec();
    let built = build(&spec);
    let h = Dss7File::from_bytes(&built.bytes).unwrap().header().clone();
    let mut b = built.bytes.clone();
    let link = (h.first_bin_address + h.bin_size * h.bins_per_block) as usize;
    set_word(&mut b, link, 0);
    let mut f = Dss7File::from_bytes(&b).unwrap();
    let cat = f.catalog().unwrap();
    assert!(cat.stats.unlisted_bins > 0);
    assert!(cat.stats.unlisted_live_entries > 0);
    assert_eq!(
        cat.stats.live + cat.stats.aliases + cat.stats.unlisted_live_entries,
        43,
        "walked + unlisted live/alias entries cover the file"
    );
    let listed: Vec<&[u8]> = cat
        .catalog
        .entries
        .iter()
        .map(|e| e.pathname.as_slice())
        .collect();
    let hidden: Vec<&Rec> = spec
        .recs
        .iter()
        .filter(|r| r.status == 1 && !listed.contains(&r.path.as_slice()))
        .collect();
    assert!(!hidden.is_empty());
    for r in hidden {
        assert!(f.lookup(&r.path).unwrap().is_some());
    }
}

#[test]
fn recovery_mode_lists_unlinked_block_entries() {
    // Same damage as above. With `recover_unlinked`, entries in bins outside the
    // block chain are listed, flagged as recovered, and agree with lookup.
    let spec = standard_spec();
    let built = build(&spec);
    let h = Dss7File::from_bytes(&built.bytes).unwrap().header().clone();
    let mut b = built.bytes.clone();
    let link = (h.first_bin_address + h.bin_size * h.bins_per_block) as usize;
    set_word(&mut b, link, 0);
    let mut f = Dss7File::from_bytes(&b).unwrap();

    let plain = f.catalog().unwrap();
    assert!(plain.meta.iter().all(|m| !m.recovered));
    assert_eq!(plain.stats.recovered_entries, 0);

    for check_hashes in [true, false] {
        let opts = CatalogOptions {
            recover_unlinked: true,
            check_hashes,
            check_info: InfoCheck::Live,
            ..CatalogOptions::default()
        };
        let cat = f.catalog_with(&opts).unwrap();
        // HEC-catalog counters are unchanged; recovered entries are counted apart.
        assert_eq!(cat.stats.entries, plain.stats.entries);
        assert_eq!(cat.stats.live, plain.stats.live);
        // `unlisted_live_entries` counts live and alias entries; `recovered_live` only live.
        let recovered_aliases = cat
            .meta
            .iter()
            .filter(|m| m.recovered && m.raw_status == 2)
            .count() as u64;
        assert_eq!(
            cat.stats.recovered_live + recovered_aliases,
            plain.stats.unlisted_live_entries
        );
        assert_eq!(
            cat.catalog.entries.len() as u64,
            plain.stats.entries + cat.stats.recovered_entries
        );
        // The bin-order prefix is exactly the plain catalog.
        assert_eq!(
            &cat.catalog.entries[..plain.catalog.entries.len()],
            &plain.catalog.entries[..]
        );

        // Every live spec record is now listed exactly once.
        for r in spec.recs.iter().filter(|r| r.status == 1) {
            let hits: Vec<usize> = (0..cat.catalog.entries.len())
                .filter(|&i| cat.catalog.entries[i].pathname == r.path)
                .collect();
            assert_eq!(hits.len(), 1, "{:?}", String::from_utf8_lossy(&r.path));
        }

        // Recovered entries resolve through lookup to the same addresses, sit in their
        // own hash chain, and have valid info blocks.
        let recovered: Vec<usize> = (0..cat.meta.len())
            .filter(|&i| cat.meta[i].recovered)
            .collect();
        assert_eq!(recovered.len() as u64, cat.stats.recovered_entries);
        assert!(!recovered.is_empty());
        for i in recovered {
            let e = &cat.catalog.entries[i];
            let m = &cat.meta[i];
            if !e.is_live() {
                continue;
            }
            assert!(matches!(m.chain, ChainState::OwnChain(_)), "{:?}", m.chain);
            assert_eq!(m.info.and_then(|s| s.problem), None);
            let r = f
                .lookup(&e.pathname)
                .unwrap()
                .expect("lookup finds recovered entry");
            assert_eq!(r.info_address, m.info_address);
            assert_eq!(
                r.bin_entry_address,
                m.bin_address + i64::from(m.entry_offset)
            );
            let j = cat.find(&e.pathname).expect("find lists recovered entry");
            assert_eq!(j, i);
            assert_eq!(f.record_at(&cat, j).unwrap().info_address, m.info_address);
        }
        assert_eq!(count(&cat.catalog, AnomalyKind::OrphanBin), 0);
        assert_eq!(count(&cat.catalog, AnomalyKind::DuplicatePathname), 0);
    }
}

#[test]
fn recovery_mode_is_a_no_op_on_undamaged_files() {
    let built = build(&standard_spec());
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();
    let plain = f.catalog().unwrap();
    let rec = f
        .catalog_with(&CatalogOptions {
            recover_unlinked: true,
            ..CatalogOptions::default()
        })
        .unwrap();
    assert_eq!(rec.catalog.entries, plain.catalog.entries);
    assert_eq!(rec.stats, plain.stats);
}

#[test]
fn recovery_mode_prefers_the_valid_copy_of_a_duplicate() {
    // An interrupted-write file: the copy HEC's lookup reaches first has a zeroed info
    // block; another copy of the same pathname is intact. Default mode keeps HEC's
    // choice; recovery mode picks the intact copy, and record_at() opens it.
    let mut spec = standard_spec();
    let p = "/BASIN/DUPZ/FLOW/01Jan2020/1Hour/RUN/";
    spec.recs.push(Rec::live(p, 100).zero_info());
    spec.recs
        .push(Rec::live(&p.to_ascii_lowercase(), 100).orphan());
    let built = build(&spec);
    let mut f = Dss7File::from_bytes(&built.bytes).unwrap();

    let plain = f.catalog().unwrap();
    let hec = plain.find(p.as_bytes()).unwrap();
    assert_eq!(plain.catalog.entries[hec].pathname, p.as_bytes());
    assert!(matches!(
        f.record_at(&plain, hec),
        Err(Dss7Error::BadInfoBlock {
            problem: InfoProblem::Zeroed,
            ..
        })
    ));
    assert!(f.lookup(p.as_bytes()).is_err());

    let rec = f
        .catalog_with(&CatalogOptions {
            recover_unlinked: true,
            ..CatalogOptions::default()
        })
        .unwrap();
    let best = rec.find(p.as_bytes()).unwrap();
    assert_ne!(best, hec);
    assert_eq!(rec.meta[hec].duplicate_of, Some(best));
    let r = f.record_at(&rec, best).unwrap();
    assert_eq!(r.pathname, p.to_ascii_lowercase().as_bytes());
    assert_eq!(r.info.flag, dss_core::dss7::info::INFO_FLAG);

    // record_at() rejects indices past the catalog and non-live entries.
    assert!(matches!(
        f.record_at(&rec, rec.catalog.entries.len()),
        Err(Dss7Error::NotLiveEntry { status: None, .. })
    ));
    if let Some(i) = (0..rec.meta.len())
        .find(|&i| !rec.catalog.entries[i].is_live() && rec.meta[i].raw_status != 2)
    {
        assert!(matches!(
            f.record_at(&rec, i),
            Err(Dss7Error::NotLiveEntry {
                status: Some(_),
                ..
            })
        ));
    }
}

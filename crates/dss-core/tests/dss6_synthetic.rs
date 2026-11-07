//! DSS6 reader tests on synthetic files built in memory (no external fixtures).

mod dss6_support;

use dss6_support::{build, mixed_records, Opts, Rec, Spy};
use dss_core::dss6::{CatalogOptions, Dss6Error, Dss6File, InfoCheck};
use dss_core::{AnomalyKind, HeaderError, RecordStatus};
use std::collections::BTreeSet;

fn full_opts() -> CatalogOptions {
    CatalogOptions {
        verify_info: InfoCheck::All,
        ..CatalogOptions::default()
    }
}

#[test]
fn catalog_lists_every_entry_in_bin_order_with_status() {
    let recs = mixed_records();
    let b = build(&recs, &Opts::default());
    assert!(b.blocks.len() > 1, "fixture should need several bin blocks");
    assert!(!b.overflows.is_empty(), "fixture should overflow bins");
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    assert!(f.warnings().is_empty(), "{:?}", f.warnings());
    assert_eq!(f.eof_word(), Some(-9630));
    let walk = f.walk_bins(&full_opts()).unwrap();
    assert!(walk.anomalies.is_empty(), "{:?}", walk.anomalies);
    assert_eq!(walk.info_checked, walk.live().count());
    assert_eq!(walk.blocks, b.blocks.len());

    let got: BTreeSet<(Vec<u8>, i32, i32)> = walk
        .entries
        .iter()
        .map(|e| (e.pathname.clone(), e.record_type, e.status))
        .collect();
    let want: BTreeSet<(Vec<u8>, i32, i32)> = recs
        .iter()
        .map(|r| (r.path.clone(), r.rtype, r.status))
        .collect();
    assert_eq!(got, want);

    let cat = walk.into_catalog();
    let live = cat.live().count();
    assert_eq!(
        live,
        recs.iter()
            .filter(|r| r.status == 1 || r.status == 11)
            .count()
    );
    let by_status = |s: RecordStatus| cat.entries.iter().filter(|e| e.status == s).count();
    assert_eq!(by_status(RecordStatus::Deleted), 2);
    assert_eq!(by_status(RecordStatus::Renamed), 1);
    assert_eq!(by_status(RecordStatus::RewrittenOld), 1);
    assert!(cat.entries.iter().any(|e| e.raw_status == 11));
    // Byte offsets of info blocks map back to the builder's word addresses.
    for e in cat.entries.iter() {
        let (_, addr) = b.info_addrs.iter().find(|(p, _)| *p == e.pathname).unwrap();
        assert_eq!(
            e.info_address,
            dss_core::dss6::word_byte_offset((*addr).into()).unwrap()
        );
    }
}

#[test]
fn lookup_finds_live_records_only() {
    let recs = mixed_records();
    let b = build(&recs, &Opts::default());
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    for r in &recs {
        let found = f.lookup(&r.path).unwrap();
        if r.status == 1 || r.status == 11 {
            let rec =
                found.unwrap_or_else(|| panic!("missing {}", String::from_utf8_lossy(&r.path)));
            assert_eq!(rec.entry.pathname, r.path);
            assert_eq!(rec.info.record_type, r.rtype);
            assert_eq!(rec.info.program, "P07TST");
            assert!(rec.info.has_valid_flag());
        } else {
            assert!(
                found.is_none(),
                "non-live record found: {}",
                String::from_utf8_lossy(&r.path)
            );
        }
    }
    // Lower case is normalized (zcheck6 lines 70-82).
    assert!(f
        .lookup(b"/basin/loc007/flow/01jan2000/1hour/obs/")
        .unwrap()
        .is_some());
    // Pseudo-regular E part converts to the stored IR- form.
    let p = f
        .lookup(b"/A/B/FLOW/01JAN2000/~1DAY/OBS/")
        .unwrap()
        .unwrap();
    assert_eq!(
        p.entry.pathname,
        b"/A/B/FLOW/01JAN2000/IR-YEAR/OBS/".to_vec()
    );
    assert!(f
        .lookup(b"/NOT/THERE/FLOW/01JAN2000/1HOUR/OBS/")
        .unwrap()
        .is_none());
    assert!(f.lookup(b"").unwrap().is_none());
    assert!(f.lookup(&[b'/'; 500]).unwrap().is_none());
}

#[test]
fn pseudo_regular_display_path() {
    let b = build(&mixed_records(), &Opts::default());
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    let walk = f.walk_bins(&CatalogOptions::default()).unwrap();
    let e = walk.entries.iter().find(|e| e.pseudo_interval > 0).unwrap();
    assert_eq!(
        e.display_pathname(),
        b"/A/B/FLOW/01JAN2000/~1DAY/OBS/".to_vec()
    );
    let plain = walk
        .entries
        .iter()
        .find(|e| e.pseudo_interval == 0)
        .unwrap();
    assert_eq!(plain.display_pathname(), plain.pathname);
}

#[test]
fn never_reads_the_lock_record() {
    let b = build(&mixed_records(), &Opts::default());
    let spy = Spy::new(&b.bytes);
    let f = Dss6File::new(&spy).unwrap();
    let (lo, hi) = f.header().lock_byte_range();
    assert_eq!(lo, (b.lock_record as u64 - 1) * 512);
    assert_eq!(dss_core::dss6::word_byte_offset(b.kafbin.into()), Some(hi));
    let walk = f.walk_bins(&full_opts()).unwrap();
    assert!(walk.anomalies.is_empty());
    for e in walk.live() {
        f.lookup(&e.pathname).unwrap().unwrap();
    }
    assert!(!spy.touched(lo, hi), "lock record was read");
    // And a direct read into it is refused.
    let lock_addr = (b.lock_record - 1) * 127 + 1;
    assert!(matches!(
        f.read_words(lock_addr, 1),
        Err(Dss6Error::LockRecord { .. })
    ));
}

#[test]
fn table_type_2_direct_bins() {
    let recs = mixed_records();
    let b = build(
        &recs,
        &Opts {
            ktable: 2,
            khash: 8,
            ..Opts::default()
        },
    );
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    assert_eq!(f.header().table_type, 2);
    let walk = f.walk_bins(&full_opts()).unwrap();
    assert!(walk.anomalies.is_empty(), "{:?}", walk.anomalies);
    assert_eq!(walk.entries.len(), recs.len());
    for r in recs.iter().filter(|r| r.status == 1 || r.status == 11) {
        assert!(f.lookup(&r.path).unwrap().is_some());
    }
}

#[test]
fn larger_tables_and_small_bins() {
    let recs = mixed_records();
    for (khash, kbnsiz, kbnblk) in [(128, 127, 8), (2048, 127, 32), (32, 63, 4)] {
        let b = build(
            &recs,
            &Opts {
                khash,
                kbnsiz,
                kbnblk,
                ..Opts::default()
            },
        );
        let f = Dss6File::from_bytes(&b.bytes).unwrap();
        let walk = f.walk_bins(&full_opts()).unwrap();
        assert!(
            walk.anomalies.is_empty(),
            "khash {khash}: {:?}",
            walk.anomalies
        );
        assert_eq!(walk.entries.len(), recs.len());
    }
}

#[test]
fn short_final_record_is_accepted() {
    let recs = vec![Rec::live("/A/B/C/01JAN2000/1DAY/F/", 100)];
    let b = build(
        &recs,
        &Opts {
            short_final: true,
            ..Opts::default()
        },
    );
    assert_eq!(b.bytes.len() % 512, 508);
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    assert!(f.warnings().is_empty(), "{:?}", f.warnings());
    assert_eq!(f.catalog().unwrap().live().count(), 1);
}

#[test]
fn size_mismatch_is_a_warning() {
    let recs = vec![Rec::live("/A/B/C/01JAN2000/1DAY/F/", 100)];
    let mut bytes = build(&recs, &Opts::default()).bytes;
    bytes.extend_from_slice(&[0u8; 512]);
    let f = Dss6File::from_bytes(&bytes).unwrap();
    assert_eq!(f.warnings().len(), 1);
    assert_eq!(f.warnings()[0].kind, AnomalyKind::Other);
    let cat = f.catalog().unwrap();
    assert_eq!(cat.live().count(), 1);
    assert_eq!(cat.anomalies.len(), 1);
}

#[test]
fn knrecs_mismatch_is_reported_and_optional_stop() {
    let recs = mixed_records();
    let live = recs
        .iter()
        .filter(|r| r.status == 1 || r.status == 11)
        .count() as i32;
    // KNRECS one ahead of the bins (5 corpus files).
    let b = build(
        &recs,
        &Opts {
            knrecs: Some(live + 1),
            ..Opts::default()
        },
    );
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    let cat = f.catalog().unwrap();
    assert_eq!(cat.live().count() as i32, live);
    assert_eq!(
        cat.anomalies
            .iter()
            .filter(|a| a.kind == AnomalyKind::RecordCountMismatch)
            .count(),
        1
    );
    // KNRECS behind the bins: the HEC-style walk stops early, the default walk does not.
    let b = build(
        &recs,
        &Opts {
            knrecs: Some(10),
            ..Opts::default()
        },
    );
    let f = Dss6File::from_bytes(&b.bytes).unwrap();
    assert_eq!(f.catalog().unwrap().live().count() as i32, live);
    let hec = f
        .catalog_with(&CatalogOptions {
            stop_at_knrecs: true,
            ..CatalogOptions::default()
        })
        .unwrap();
    assert_eq!(hec.live().count(), 10);
}

#[test]
fn write_locked_size_marker() {
    let recs = vec![Rec::live("/A/B/C/01JAN2000/1DAY/F/", 100)];
    let mut bytes = build(&recs, &Opts::default()).bytes;
    bytes[20..24].copy_from_slice(&(-1357i32).to_le_bytes());
    let f = Dss6File::from_bytes(&bytes).unwrap();
    assert!(f.header().is_write_locked());
    assert_eq!(f.eof_word(), None);
    assert!(f.warnings().iter().any(|w| w.detail.contains("-1357")));
    assert_eq!(f.catalog().unwrap().live().count(), 1);
}

fn with_word(bytes: &[u8], index: usize, v: i32) -> Vec<u8> {
    let mut b = bytes.to_vec();
    b[4 * index..4 * index + 4].copy_from_slice(&v.to_le_bytes());
    b
}

#[test]
fn typed_header_errors() {
    let base = build(
        &[Rec::live("/A/B/C/01JAN2000/1DAY/F/", 100)],
        &Opts::default(),
    )
    .bytes;
    let err = |b: &[u8]| Dss6File::from_bytes(b).unwrap_err();
    assert!(matches!(
        err(&[]),
        Dss6Error::Header(HeaderError::TooShort(0))
    ));
    assert!(matches!(
        err(b"not a dss file at all"),
        Dss6Error::Header(HeaderError::NotDss)
    ));
    assert!(matches!(
        err(&vec![0u8; 4096]),
        Dss6Error::Header(HeaderError::NotDss)
    ));
    assert!(matches!(
        err(&base[..100]),
        Dss6Error::Header(HeaderError::TooShort(100))
    ));
    // Version must start "6-".
    let mut v = base.clone();
    v[16..20].copy_from_slice(b"6XYN");
    assert!(matches!(err(&v), Dss6Error::NotVersion6 { .. }));
    let mut v = base.clone();
    v[16..20].copy_from_slice(b"7-IQ");
    assert!(matches!(
        err(&v),
        Dss6Error::NotVersion6 { .. } | Dss6Error::Header(_)
    ));
    // Byte swap: KTABLE 1 stored big-endian, and a reversed signature.
    assert!(matches!(
        err(&with_word(&base, 15, 1i32.swap_bytes())),
        Dss6Error::ByteSwapped { .. }
    ));
    let mut sw = base.clone();
    sw[0..4].copy_from_slice(b"SSDZ");
    sw[16..20].copy_from_slice(b"NY-6");
    assert!(matches!(err(&sw), Dss6Error::ByteSwapped { .. }));
    // KTABLE > 10000 that is not a swapped 1/2 (text-mangled snodas_test.dss).
    assert!(matches!(
        err(&with_word(&base, 15, 65536)),
        Dss6Error::BadTableType(65536)
    ));
    assert!(matches!(
        err(&with_word(&base, 15, 3)),
        Dss6Error::BadTableType(3)
    ));
    assert!(matches!(
        err(&with_word(&base, 18, 12)),
        Dss6Error::BinSizeOutOfRange(12)
    ));
    assert!(matches!(
        err(&with_word(&base, 18, 128)),
        Dss6Error::BinSizeOutOfRange(128)
    ));
    assert!(matches!(
        err(&with_word(&base, 5, -2)),
        Dss6Error::BadHeader {
            field: "KFSIZE",
            ..
        }
    ));
    assert!(matches!(
        err(&with_word(&base, 14, 0)),
        Dss6Error::BadHeader { field: "KHASH", .. }
    ));
    assert!(matches!(
        err(&with_word(&base, 16, 0)),
        Dss6Error::BadHeader {
            field: "KBNBLK",
            ..
        }
    ));
    assert!(matches!(
        err(&with_word(&base, 19, 0)),
        Dss6Error::BadHeader {
            field: "KAFBIN",
            ..
        }
    ));
    assert!(matches!(
        err(&with_word(&base, 19, i32::MAX)),
        Dss6Error::BadHeader {
            field: "KAFBIN",
            ..
        }
    ));
}

#[test]
fn errors_display() {
    let e = Dss6File::from_bytes(&[]).unwrap_err();
    assert!(!e.to_string().is_empty());
}

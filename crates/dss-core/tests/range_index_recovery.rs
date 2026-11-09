//! P61: whole-dataset ranges through `IndexedDss7Catalog` against the scan route
//! (`read_ts_with_catalog`), through the public API only.
//!
//! - A synthetic DSS7 recovery fixture (no HEC software): the record payloads are
//!   written by the engine's DSS7 writer (`write_rts`) and their areas copied into the
//!   bin layout of `dss7_common::build` (as `synthetic_recovery7.dss`), whose dataset `/P61/SITE/FLOW//1Hour/SYN/` has dated
//!   blocks in linked bins (Jan, Feb 2020), an orphan-bin block (Dec 2019, listed by
//!   the default catalog, unreachable by lookup), an orphan lower-case duplicate of the
//!   Feb block, a deleted Nov 2019 block, and a Mar 2020 block in an unlinked bin
//!   (listed only by the recovery catalog). The recovery catalog therefore has a later
//!   last date than the default one.
//! - Edits to a caller's `Dss7Catalog` after a whole read, and to a clone of it, are
//!   honoured, as with the scan before P61.
//! - `Dss7Catalog` can still be built with a struct literal of its three public fields.
//!
//! To regenerate the committed fixture, set `DSS_PY_FIXTURE_OUT` to a scratch directory,
//! run this test, copy `synthetic_range_recovery7.dss` into `fixtures/public/` and update
//! its SHA-256 in `fixtures/public/README.md`.

mod dss7_common;

use dss7_common::{build, set_word, Rec, Spec};
use dss_core::dss7::info::key;
use dss_core::dss7::interval as iv;
use dss_core::dss7::write::{create_dss7, write_rts, RtsInput, RtsValues, WriteOptions};
use dss_core::dss7::{
    status, CatalogOptions, Dss7Catalog, Dss7File, Dss7Ts, IndexedDss7Catalog, TsError,
    TsReadOptions,
};
use dss_core::{Catalog, TsValues};

const DATASET: &[u8] = b"/P61/SITE/FLOW//1Hour/SYN/";
const OTHER: &[u8] = b"/P61/OTHER/FLOW//1Hour/SYN/";

/// Spec order; the unlinked block must be last (alone in the last bin block).
fn spec() -> Spec {
    let rec = |path: &str, y: i32, m: i32| {
        let mut r = Rec::live(path, 100);
        let first = iv::ymd_to_julian(y as i64, m as i64, 1) as i32;
        let last =
            iv::increment_block(first as i64, iv::reg_block_start(first as i64, 3600).1) as i32 - 1;
        r.julian = (first, last);
        r
    };
    Spec {
        bins_per_block: 1,
        recs: vec![
            rec("/P61/SITE/FLOW/01JAN2020/1Hour/SYN/", 2020, 1),
            rec("/P61/SITE/FLOW/01FEB2020/1Hour/SYN/", 2020, 2),
            rec("/P61/SITE/FLOW/01DEC2019/1Hour/SYN/", 2019, 12).orphan(),
            rec("/p61/site/flow/01feb2020/1hour/syn/", 2020, 2).orphan(),
            rec("/P61/SITE/FLOW/01NOV2019/1Hour/SYN/", 2019, 11).with_status(status::DELETED),
            rec("/P61/OTHER/FLOW/01JAN2020/1Hour/SYN/", 2020, 1),
            rec("/P61/SITE/FLOW/01MAR2020/1Hour/SYN/", 2020, 3),
        ],
        ..Spec::default()
    }
}

const R_UNLINKED: usize = 6;

/// One engine-written file with every block the fixture needs: `/P61/SITE/FLOW/` hourly
/// from 01 Nov 2019 to 31 Mar 2020 (one record per month) and `/P61/OTHER/FLOW/` for
/// January 2020. Values are distinct per record.
fn engine_written() -> Vec<u8> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("p61_range_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("src7.dss");
    let _ = std::fs::remove_file(&path);
    create_dss7(&path).unwrap();
    let opts = WriteOptions {
        sync: false,
        ..WriteOptions::default()
    };
    let start = |y, m| iv::ymd_to_julian(y, m, 1) * 86_400 + 3600;
    let site: Vec<f32> = (0..(30 + 31 + 31 + 31 + 29) * 24)
        .map(|i| i as f32)
        .collect();
    let input = RtsInput::new(
        b"/P61/SITE/FLOW//1Hour/SYN/",
        start(2019, 11),
        RtsValues::F32(&site),
        b"CFS",
        b"INST-VAL",
    );
    write_rts(&path, &input, &opts).unwrap();
    let other: Vec<f32> = (0..31 * 24).map(|i| -(i as f32)).collect();
    let input = RtsInput::new(
        b"/P61/OTHER/FLOW//1Hour/SYN/",
        start(2020, 1),
        RtsValues::F32(&other),
        b"CFS",
        b"INST-VAL",
    );
    write_rts(&path, &input, &opts).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// Copy the record areas (internal header, header2, user header, values1-3) of the
/// engine-written record `src_path` to the end of `bytes`, and point the synthetic info
/// block at word `info` to them (Internal/zwriteNew.c:zwriteNew: addresses count int64
/// words, lengths int32 words).
fn splice(bytes: &mut Vec<u8>, info: usize, src: &[u8], src_path: &[u8]) {
    let mut f = Dss7File::from_bytes(src).unwrap();
    let rec = f.lookup(src_path).unwrap().expect("engine-written block");
    let i = &rec.info;
    let areas = [
        (key::INTERNAL_HEAD_ADDRESS, i.internal_header),
        (key::HEADER2_ADDRESS, i.header2),
        (key::USER_HEAD_ADDRESS, i.user_header),
        (key::VALUES1_ADDRESS, i.values1),
        (key::VALUES2_ADDRESS, i.values2),
        (key::VALUES3_ADDRESS, i.values3),
    ];
    for (addr_key, a) in areas {
        if a.number_ints <= 0 {
            continue;
        }
        let words = (a.number_ints as usize).div_ceil(2);
        let from = a.address as usize * 8;
        let to = bytes.len() / 8;
        bytes.extend_from_slice(&src[from..from + words * 8]);
        set_word(bytes, info + addr_key, to as i64);
        set_word(bytes, info + addr_key + 1, a.number_ints);
    }
    set_word(bytes, info + key::NUMBER_DATA, i.number_data);
    set_word(bytes, info + key::LOGICAL_NUMBER, i.logical_number);
    set_word(bytes, info + key::ALLOCATED_SIZE, i.allocated_size);
}

fn fixture_bytes() -> Vec<u8> {
    let spec = spec();
    let built = build(&spec);
    let mut bytes = built.bytes;
    let src = engine_written();
    let eof_word = bytes.len() / 8 - 1; // overwrite the old EOF flag
    bytes.truncate(eof_word * 8);
    for (r, info) in spec.recs.iter().zip(&built.info_addr) {
        // The orphan lower-case duplicate carries the upper-case block's payload.
        let p = String::from_utf8(r.path.clone()).unwrap();
        let src_path = p.to_ascii_uppercase().replace("/1HOUR/", "/1Hour/");
        splice(&mut bytes, *info as usize, &src, src_path.as_bytes());
    }
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(&mut bytes, 5, eof as i64);
    // Cut the link to the last bin block (dss7.md §3.5): zcatalogInternal() follows the
    // block links and no longer lists it; zcheck() still reaches it from the hash table.
    let h = Dss7File::from_bytes(&bytes).unwrap().header().clone();
    let unlinked_bin = built.entry_addr[R_UNLINKED];
    let prev_block = unlinked_bin - (h.bin_size * h.bins_per_block + 1);
    assert_eq!(
        built
            .entry_addr
            .iter()
            .filter(|&&e| e >= unlinked_bin && e < unlinked_bin + h.bin_size)
            .count(),
        1,
        "the unlinked record must be alone in the last bin"
    );
    set_word(
        &mut bytes,
        (prev_block + h.bin_size * h.bins_per_block) as usize,
        0,
    );
    bytes
}

/// Bit-level fingerprint of a whole read: every value's bits, times, quality, all
/// metadata, or the error.
fn bits(r: &Result<Dss7Ts, TsError>) -> String {
    match r {
        Ok(t) => {
            let v: Vec<u64> = match &t.ts.values {
                TsValues::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
                TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
            };
            format!(
                "ok {:?} {} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
                t.ts.pathname,
                t.ts.record_type,
                t.ts.times,
                v,
                t.ts.quality,
                t.ts.units,
                t.ts.data_type,
                t.ts.time_zone,
                t.meta
            )
        }
        Err(e) => format!("err {e:?}"),
    }
}

fn whole() -> TsReadOptions {
    TsReadOptions {
        window: None,
        trim_missing: false,
    }
}

fn catalogs(f: &mut Dss7File<std::io::Cursor<&[u8]>>) -> (Dss7Catalog, Dss7Catalog) {
    let normal = f.catalog().expect("default catalog");
    let recover = f
        .catalog_with(&CatalogOptions {
            recover_unlinked: true,
            ..CatalogOptions::default()
        })
        .expect("recovery catalog");
    (normal, recover)
}

#[test]
fn recovered_block_extends_the_range_and_index_matches_scan() {
    let bytes = fixture_bytes();
    let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_range_recovery7.dss");
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_range_recovery7.dss"), &bytes).unwrap();
    }
    let c = std::fs::read(&committed)
        .expect("fixtures/public/synthetic_range_recovery7.dss is missing");
    assert_eq!(
        c, bytes,
        "fixtures/public/synthetic_range_recovery7.dss is stale"
    );

    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let (normal, recover) = catalogs(&mut f);
    assert_eq!(normal.catalog.entries.len(), 6);
    assert_eq!(recover.catalog.entries.len(), 7);
    assert!(recover.meta[6].recovered);

    let mut results = Vec::new();
    for cat in [normal, recover] {
        let ix = IndexedDss7Catalog::new(cat.clone());
        for q in [DATASET, OTHER, b"/P61/NONE/FLOW//1Hour/SYN/".as_slice()] {
            let scan = f.read_ts_with_catalog(&cat, q, &whole());
            let indexed = f.read_ts_with_indexed_catalog(&ix, q, &whole());
            assert_eq!(bits(&scan), bits(&indexed));
            let sp = f.read_ts_profile_with_catalog(&cat, q, &whole());
            let ip = f.read_ts_profile_with_indexed_catalog(&ix, q, &whole());
            assert_eq!(format!("{sp:?}"), format!("{ip:?}"));
            results.push(scan);
        }
        assert!(ix.index_built());
    }
    // The dataset: the recovery catalog's range reaches the unlinked March block.
    let (n, r) = (&results[0], &results[3]);
    assert_ne!(bits(n), bits(r), "recovery must change the dataset's range");
    let span = |x: &Result<Dss7Ts, TsError>| {
        x.as_ref()
            .map(|t| {
                (
                    t.ts.times[0],
                    *t.ts.times.last().unwrap(),
                    t.meta.blocks_in_window,
                )
            })
            .unwrap()
    };
    let (n0, n1, nb) = span(n);
    let (r0, r1, rb) = span(r);
    assert_eq!(
        n0, r0,
        "first date comes from the orphan December block in both"
    );
    assert!(r1 > n1, "last date comes from the recovered March block");
    assert_eq!((nb, rb), (3, 4));
    // The other dataset is unchanged by recovery; the unknown one is not found.
    assert_eq!(bits(&results[1]), bits(&results[4]));
    assert!(matches!(results[2], Err(TsError::NotFound { .. })));
}

#[test]
fn edits_to_a_caller_catalog_after_a_read_are_honoured() {
    let bytes = fixture_bytes();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let (mut cat, _) = catalogs(&mut f);
    let before = f.read_ts_with_catalog(&cat, DATASET, &whole());
    assert!(before.is_ok(), "{before:?}");
    // A clone taken after the read, then edited: every entry deleted.
    let mut clone = cat.clone();
    for m in &mut clone.meta {
        m.raw_status = status::DELETED;
    }
    assert!(matches!(
        f.read_ts_with_catalog(&clone, DATASET, &whole()),
        Err(TsError::NotFound { .. })
    ));
    // The original is unaffected by the clone's edit.
    assert_eq!(
        bits(&f.read_ts_with_catalog(&cat, DATASET, &whole())),
        bits(&before)
    );
    // Editing the original after its first read: drop the December orphan block.
    let dec = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == b"/P61/SITE/FLOW/01DEC2019/1Hour/SYN/")
        .unwrap();
    cat.meta[dec].raw_status = status::DELETED;
    let edited = f.read_ts_with_catalog(&cat, DATASET, &whole());
    let fresh = {
        let (mut c, _) = catalogs(&mut f);
        c.meta[dec].raw_status = status::DELETED;
        f.read_ts_with_catalog(&c, DATASET, &whole())
    };
    assert_eq!(bits(&edited), bits(&fresh));
    assert_ne!(bits(&edited), bits(&before));
    // An indexed snapshot of the edited catalog agrees; giving it back and editing
    // again is honoured too.
    let ix = IndexedDss7Catalog::new(cat);
    assert_eq!(
        bits(&f.read_ts_with_indexed_catalog(&ix, DATASET, &whole())),
        bits(&edited)
    );
    let mut back = ix.into_catalog();
    for m in &mut back.meta {
        m.raw_status = status::DELETED;
    }
    assert!(matches!(
        f.read_ts_with_catalog(&back, DATASET, &whole()),
        Err(TsError::NotFound { .. })
    ));
}

#[test]
fn dss7_catalog_struct_literal_still_compiles() {
    // Downstream construction with exactly the three public fields.
    let cat = Dss7Catalog {
        catalog: Catalog::default(),
        meta: Vec::new(),
        stats: Default::default(),
    };
    let Dss7Catalog {
        catalog,
        meta,
        stats,
    } = cat.clone();
    assert!(catalog.entries.is_empty() && meta.is_empty() && stats.entries == 0);
    let _ = Dss7Catalog {
        meta: vec![],
        ..cat
    };
}

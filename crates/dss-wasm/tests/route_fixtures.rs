//! P40 host tests of the profile, location and recovery routes: every public fixture read
//! through `dss_wasm::reader` (in-memory bytes) equals the `dss-core` reads from the file
//! path, in default and recovery mode; the guards refuse before `dss-core` assembles.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::dataset_key;
use common::routes::{open_mode, route_digest, MAX_BLOCKS, MAX_CELLS, ROUTE_FIXTURES};
use dss_core::dss6::{self, Dss6File};
use dss_core::dss7::{self, CatalogOptions, Dss7File, RecordRef, TsReadOptions, TsWindow};
use dss_core::location::LOCATION;
use dss_core::{GridReadOptions, Location, TimeSeries, TsValues};
use dss_wasm::reader::{index_arg, lossy, DssBuffer, ErrorKind, LocationData, ProfileData};
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// Upper bound on a refusal that must come before any walk or assembly. It catches work that
/// grows with the claimed range, and is wide because debug builds share busy CI runners, where
/// 0.5 s failed on refusals that take about that long locally.
const REFUSAL_SECONDS: f64 = 10.0;

fn public_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public")
}

fn all_public_fixtures() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(public_dir())
        .expect("fixtures/public")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "dss"))
        .collect();
    v.sort();
    v
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(public_dir().join(name)).expect("fixture")
}

fn same_values(a: &TsValues, b: &TsValues) -> bool {
    match (a, b) {
        (TsValues::F32(x), TsValues::F32(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits())
        }
        (TsValues::F64(x), TsValues::F64(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits())
        }
        _ => false,
    }
}

fn same_ts(a: &TimeSeries, b: &TimeSeries) -> bool {
    a.pathname == b.pathname
        && a.record_type == b.record_type
        && a.kind == b.kind
        && a.times == b.times
        && same_values(&a.values, &b.values)
        && a.quality == b.quality
        && a.units == b.units
        && a.data_type == b.data_type
        && a.time_zone == b.time_zone
}

/// Bitwise equality of every field; the metadata holds no floats.
fn same_profile(a: &dss7::Dss7Profile, b: &ProfileData) -> bool {
    let b = &b.profile;
    a.pathname == b.pathname
        && a.record_type == b.record_type
        && a.times == b.times
        && same_values(&a.depths, &b.depths)
        && same_values(&a.values, &b.values)
        && a.depth_units == b.depth_units
        && a.value_units == b.value_units
        && a.data_type == b.data_type
        && a.time_zone == b.time_zone
        && a.meta == b.meta
}

fn same_location(a: &Location, b: &Location) -> bool {
    a.pathname == b.pathname
        && a.source == b.source
        && [a.x, a.y, a.z].map(f64::to_bits) == [b.x, b.y, b.z].map(f64::to_bits)
        && a.descriptors_stored == b.descriptors_stored
        && a.coordinate_system_code == b.coordinate_system_code
        && a.coordinate_id == b.coordinate_id
        && a.horizontal_units_code == b.horizontal_units_code
        && a.horizontal_datum_code == b.horizontal_datum_code
        && a.vertical_units_code == b.vertical_units_code
        && a.vertical_datum_code == b.vertical_datum_code
        && a.time_zone_name == b.time_zone_name
        && a.supplemental == b.supplemental
}

#[derive(Default, Debug)]
struct Tally {
    profiles: usize,
    profile_windows: usize,
    profile_refusals: usize,
    locations: usize,
    associated: usize,
    recovered_reads: usize,
    at_records: usize,
    at_refused: usize,
    both_failed: usize,
}

fn cat7(f: &mut Dss7File<File>, recover: bool) -> dss7::Dss7Catalog {
    f.catalog_with(&CatalogOptions {
        recover_unlinked: recover,
        ..CatalogOptions::default()
    })
    .expect("catalog")
}

/// The record `dss-core` reaches for a pathname in this mode (Python `read_by_pathname`).
fn record7(
    f: &mut Dss7File<File>,
    cat: &dss7::Dss7Catalog,
    p: &[u8],
    recover: bool,
) -> Result<Option<RecordRef>, dss7::Dss7Error> {
    if recover {
        match cat.find(p) {
            Some(i) => f.record_at(cat, i).map(Some),
            None => Ok(None),
        }
    } else {
        f.lookup(p)
    }
}

fn compare_v7(path: &Path, bytes: &[u8], recover: bool, t: &mut Tally) {
    let mut f = Dss7File::open(path).expect("open");
    let cat = cat7(&mut f, recover);
    let mut b = open_mode(bytes, recover);
    let mine = b.catalog().expect("catalog");
    assert_eq!(mine.len(), cat.catalog.entries.len(), "{path:?} catalog");
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        assert_eq!(mine.stored_pathname[i], lossy(&e.pathname));
        assert_eq!(mine.recovered[i], cat.meta[i].recovered);
    }
    let mut seen = std::collections::BTreeSet::new();
    for i in cat.resolved_live() {
        let e = &cat.catalog.entries[i];
        let p = lossy(&e.pathname);
        let rec = match record7(&mut f, &cat, &e.pathname, recover) {
            Ok(Some(r)) => r,
            _ => {
                t.both_failed += 1;
                continue;
            }
        };
        match e.record_type {
            LOCATION => match (f.read_location_record(&rec), b.read_location(&p)) {
                (Ok(n), Ok(w)) => {
                    assert!(same_location(&n.location, &w.location), "{path:?} {p}");
                    assert_eq!(Some(n.values1), w.values1);
                    assert_eq!(Some(n.program), w.program);
                    assert_eq!(Some(n.last_write_ms), w.last_write_ms);
                    t.locations += 1;
                }
                (Err(_), Err(_)) => t.both_failed += 1,
                (n, w) => panic!("{path:?} {p}: {:?} vs {:?}", n.err(), w.err()),
            },
            100..=199 => {
                match (
                    f.read_ts_profile_record(&rec),
                    b.read_profile_record(&p, MAX_CELLS),
                ) {
                    (Ok(n), Ok(w)) => {
                        assert!(same_profile(&n, &w), "{path:?} {p} profile record");
                        t.profiles += 1;
                    }
                    (Err(_), Err(w)) => {
                        assert_ne!(w.kind, ErrorKind::InvalidArgument, "{path:?} {p}: {w}");
                        t.profile_refusals += 1;
                    }
                    (n, w) => panic!("{path:?} {p}: {:?} vs {:?}", n.err(), w.err()),
                }
                // Recovery mode reads the copy the recovery catalog selects.
                if recover {
                    if let (Ok(n), Ok(w)) = (f.read_ts_record(&rec), b.read_record(&p)) {
                        assert!(same_ts(&n.ts, &w.ts), "{path:?} {p} recovered record");
                        t.recovered_reads += 1;
                    }
                }
                if !dataset_key(&p).is_some_and(|k| seen.insert(k)) {
                    continue;
                }
                let whole = TsReadOptions {
                    window: None,
                    trim_missing: true,
                };
                let n = f.read_ts_profile_with_catalog(&cat, &e.pathname, &whole);
                let w = b.read_profile(&p, None, true, MAX_BLOCKS, MAX_CELLS);
                match (n, w) {
                    (Ok(n), Ok(w)) => {
                        assert!(same_profile(&n, &w), "{path:?} {p} profile dataset");
                        t.profiles += 1;
                        let (s, e2) = (n.times[0], n.times[n.times.len() / 2]);
                        let o = TsReadOptions {
                            window: Some(TsWindow { start: s, end: e2 }),
                            trim_missing: false,
                        };
                        let n = f.read_ts_profile(&e.pathname, &o).expect("window");
                        let w = b
                            .read_profile(&p, Some((s, e2)), false, MAX_BLOCKS, MAX_CELLS)
                            .expect("window");
                        assert!(same_profile(&n, &w), "{path:?} {p} profile window");
                        t.profile_windows += 1;
                    }
                    (Err(n), Err(w)) => {
                        assert_ne!(w.kind, ErrorKind::InvalidArgument, "{path:?} {p}: {w}");
                        // The guard refuses what `dss-core` refuses, with the same kind.
                        let same = matches!(
                            (&n, w.kind),
                            (
                                dss7::TsError::UnsupportedRecordType { .. },
                                ErrorKind::Unsupported
                            ) | (dss7::TsError::NotFound { .. }, ErrorKind::NotFound)
                                | (dss7::TsError::BadPathname { .. }, ErrorKind::NotFound)
                                | (dss7::TsError::Corrupt { .. }, ErrorKind::Format)
                                | (dss7::TsError::Dss7(_), ErrorKind::Format)
                        );
                        assert!(same, "{path:?} {p}: {n} vs {w}");
                        t.profile_refusals += 1;
                    }
                    (n, w) => panic!("{path:?} {p}: {:?} vs {:?}", n.err(), w.err()),
                }
            }
            200..=299 if recover => {
                if let (Ok(n), Ok(w)) = (f.read_pd_record(&rec), b.read_paired_data(&p)) {
                    assert!(same_values(&n.pd.values, &w.pd.values), "{path:?} {p}");
                    t.recovered_reads += 1;
                }
            }
            400..=499 if recover => {
                let o = GridReadOptions::default();
                if let (Ok(n), Ok(w)) = (f.read_grid_record_with(&rec, &o), b.read_grid(&p, &o)) {
                    assert_eq!(n.data.len(), w.grid.data.len());
                    assert!(n
                        .data
                        .iter()
                        .zip(&w.grid.data)
                        .all(|(x, y)| x.to_bits() == y.to_bits()));
                    t.recovered_reads += 1;
                }
            }
            _ => {}
        }
        // Associated location: the /A/B/Location Info//// record, as `dss-core`, through
        // the record this mode reaches.
        let core_assoc = match dss_core::location::location_pathname(&e.pathname) {
            Some(lp) if lp.len() <= dss7::MAX_PATHNAME_BYTES => {
                match record7(&mut f, &cat, &lp, recover) {
                    Ok(Some(r)) => f.read_location_record(&r).map(Some).map_err(|_| ()),
                    Ok(None) => Ok(None),
                    Err(_) => Err(()),
                }
            }
            _ => Err(()),
        };
        match (core_assoc, b.read_associated_location(&p)) {
            (Ok(Some(n)), Ok(Some(w))) => {
                assert!(same_location(&n.location, &w.location), "{path:?} {p}");
                t.associated += 1;
            }
            (Ok(None), Ok(None)) => {}
            (Err(()), Err(_)) => t.both_failed += 1,
            (n, w) => panic!("{path:?} {p}: {n:?} vs {w:?}"),
        }
    }
    // Index route: every row of the catalog in this mode.
    for i in 0..cat.catalog.entries.len() {
        let live = (0..=2).contains(&cat.meta[i].raw_status);
        match (f.record_at(&cat, i), b.read_record_at(i)) {
            (Ok(rec), got) => {
                let core_ts = f.read_ts_record(&rec);
                let pv = b.entry_provenance(i).expect("provenance");
                assert_eq!(pv.index, i);
                assert_eq!(pv.recover_mode, recover);
                assert_eq!(pv.recovered, cat.meta[i].recovered);
                assert_eq!(pv.duplicate_of, cat.meta[i].duplicate_of);
                match (core_ts, got) {
                    (Ok(n), Ok((_, w))) => {
                        assert!(same_ts(&n.ts, &w.ts), "{path:?} row {i}");
                        t.at_records += 1;
                    }
                    (Err(_), Err(w)) => {
                        assert!(
                            matches!(w.kind, ErrorKind::Unsupported | ErrorKind::Format),
                            "{path:?} row {i}: {w}"
                        );
                        t.at_refused += 1;
                    }
                    (Ok(_), Err(w)) => panic!("{path:?} row {i}: core read it, wasm {w}"),
                    (Err(n), Ok(_)) => panic!("{path:?} row {i}: core {n}, wasm returned data"),
                }
            }
            (Err(dss7::Dss7Error::NotLiveEntry { .. }), Err(w)) => {
                assert!(!live, "{path:?} row {i}");
                assert_eq!(w.kind, ErrorKind::Index);
                t.at_refused += 1;
            }
            (Err(dss7::Dss7Error::BadInfoBlock { .. }), Err(w)) => {
                assert_eq!(w.kind, ErrorKind::Unavailable, "{path:?} row {i}: {w}");
                assert_eq!(w.kind.as_str(), "format");
                assert_eq!(w.unavailable.as_ref().unwrap().index, i);
                t.at_refused += 1;
            }
            (Err(n), Err(w)) => {
                assert_eq!(w.kind, ErrorKind::Unavailable, "{path:?} row {i}: {n} {w}");
                t.at_refused += 1;
            }
            (Err(n), Ok(_)) => panic!("{path:?} row {i}: core {n}, wasm returned data"),
        }
    }
    let n = cat.catalog.entries.len();
    assert_eq!(b.read_record_at(n).unwrap_err().kind, ErrorKind::Index);
    assert_eq!(b.entry_provenance(n).unwrap_err().kind, ErrorKind::Index);
}

fn compare_v6(path: &Path, bytes: &[u8], recover: bool, t: &mut Tally) {
    let f = Dss6File::open(path).expect("open");
    let walk = f.walk_bins(&dss6::CatalogOptions::default()).expect("walk");
    let mut b = open_mode(bytes, recover);
    let mine = b.catalog().expect("catalog");
    assert_eq!(mine.len(), walk.entries.len());
    assert!(mine.recovered.iter().all(|r| !r), "DSS6 has no recovery");
    let mut seen = std::collections::BTreeSet::new();
    let mut seen_full = std::collections::BTreeSet::new();
    for (i, e) in walk.entries.iter().enumerate() {
        let p = lossy(&e.pathname);
        let ce = e.to_catalog_entry();
        // Index route.
        let status = e.record_status();
        let live = matches!(
            status,
            dss_core::RecordStatus::Live | dss_core::RecordStatus::Alias
        );
        match (
            live.then(|| f.read_ts_entry(&ce, &dss6::TsReadOptions::default())),
            b.read_record_at(i),
        ) {
            (Some(Ok(n)), Ok((pv, w))) => {
                assert!(same_ts(&n.to_timeseries(), &w.ts), "{path:?} row {i}");
                assert_eq!(pv.index, i);
                assert!(!pv.recover_mode);
                t.at_records += 1;
            }
            (Some(Err(_)), Err(_)) => t.at_refused += 1,
            (None, Err(w)) => {
                assert_eq!(w.kind, ErrorKind::Index);
                t.at_refused += 1;
            }
            (n, w) => panic!(
                "{path:?} row {i}: {:?} vs {:?}",
                n.map(|r| r.is_ok()),
                w.err()
            ),
        }
        if !live || !seen_full.insert(e.pathname.clone()) {
            continue;
        }
        // No location records and no profiles in DSS6.
        assert_eq!(
            b.read_location(&p).unwrap_err().kind,
            ErrorKind::Unsupported,
            "{path:?} {p}"
        );
        if (100..200).contains(&e.record_type) {
            assert_eq!(
                b.read_profile_record(&p, MAX_CELLS).unwrap_err().kind,
                ErrorKind::Unsupported
            );
            if dataset_key(&p).is_some_and(|k| seen.insert(k)) {
                if let Err(w) = b.read_profile(&p, None, true, MAX_BLOCKS, MAX_CELLS) {
                    assert!(
                        matches!(
                            w.kind,
                            ErrorKind::Unsupported
                                | ErrorKind::NotFound
                                | ErrorKind::Format
                                | ErrorKind::Dss
                        ),
                        "{path:?} {p}: {w}"
                    );
                    t.profile_refusals += 1;
                } else {
                    panic!("{path:?} {p}: DSS6 returned a profile");
                }
            }
            // Associated location: the block's own header and user header.
            let n = f.read_ts_record(&e.pathname).ok().and_then(|ts| {
                Location::from_dss6_ts(&ts.pathname, ts.coordinates, ts.coordinate_descriptors)
                    .map(|l| l.with_dss6_user_header(&ts.user_header))
            });
            match (n, b.read_associated_location(&p)) {
                (Some(n), Ok(Some(w))) => {
                    assert!(same_location(&n, &w.location), "{path:?} {p}");
                    t.associated += 1;
                }
                (None, Ok(None)) => {}
                (None, Err(_)) => t.both_failed += 1,
                (n, w) => panic!("{path:?} {p}: {n:?} vs {w:?}"),
            }
        }
    }
}

/// Every public fixture, default and recovery mode, every new route equals `dss-core`.
#[test]
fn every_public_fixture_equals_dss_core() {
    let files = all_public_fixtures();
    assert!(files.len() >= 40, "{}", files.len());
    let mut total = [Tally::default(), Tally::default()];
    for path in &files {
        let bytes = std::fs::read(path).expect("read");
        let version = DssBuffer::new(bytes.clone()).expect("open").version();
        for (m, recover) in [false, true].into_iter().enumerate() {
            if version == 7 {
                compare_v7(path, &bytes, recover, &mut total[m]);
            } else {
                compare_v6(path, &bytes, recover, &mut total[m]);
            }
        }
    }
    eprintln!(
        "P40 compared {} files: default {:?}; recovery {:?}",
        files.len(),
        total[0],
        total[1]
    );
    let [d, r] = &total;
    assert!(d.profiles >= 6 && d.profile_windows >= 3, "{d:?}");
    assert!(d.locations >= 5 && d.associated >= 5, "{d:?}");
    assert!(d.at_records > 1000 && d.at_refused > 100, "{d:?}");
    assert!(r.recovered_reads > 500 && r.at_records > 1000, "{r:?}");
}

/// A DSS6 pathname naming no block reads the dataset, and the last block by D-part date
/// supplies the result (`_file.py::read_associated_location`).
#[test]
fn dss6_associated_location_falls_back_to_the_dataset() {
    let bytes = fixture_bytes("synthetic_location_uh6.dss");
    let mut b = DssBuffer::new(bytes).unwrap();
    let cat = b.catalog().unwrap();
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for i in 0..cat.len() {
        if cat.status[i] == "live" && (100..120).contains(&cat.record_type[i]) {
            if let Some(k) = dataset_key(&cat.pathname[i]) {
                groups.entry(k).or_default().push(cat.pathname[i].clone());
            }
        }
    }
    assert!(!groups.is_empty());
    let mut fallbacks = 0;
    for (key, blocks) in &groups {
        let got = b.read_associated_location(key);
        let got: Option<LocationData> = match got {
            Ok(v) => v,
            Err(e) => {
                assert_eq!(e.kind, ErrorKind::LocationConflict, "{key}: {e}");
                assert_eq!(e.kind.as_str(), "dss");
                continue;
            }
        };
        let mut per_block: Vec<Option<LocationData>> = Vec::new();
        for p in blocks {
            per_block.push(b.read_associated_location(p).unwrap());
        }
        if let Some(l) = &got {
            let last = per_block
                .iter()
                .flatten()
                .max_by_key(|x| (x.block_julian, x.location.pathname.clone()))
                .unwrap();
            assert_eq!(l.location.pathname, last.location.pathname, "{key}");
            assert_eq!(l.location.supplemental, last.location.supplemental, "{key}");
            fallbacks += 1;
        } else {
            assert!(per_block.iter().all(|x| x.is_none()), "{key}");
        }
    }
    assert!(fallbacks >= 1);
    // A malformed or unknown pathname is not found, and not a location of nothing.
    assert_eq!(
        b.read_associated_location("/NO/SUCH/SERIES//1HOUR/X/")
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        b.read_associated_location("nonsense").unwrap_err().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn location_refusals_are_typed() {
    let mut b = DssBuffer::new(fixture_bytes("synthetic_location7.dss")).unwrap();
    assert_eq!(
        b.read_location("/NO/SUCH/Location Info////")
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    // No location record for these parts: `None`; the data record need not exist.
    assert!(b
        .read_associated_location("/NO/SUCH/FLOW//1HOUR/X/")
        .unwrap()
        .is_none());
    assert_eq!(
        b.read_associated_location("nonsense").unwrap_err().kind,
        ErrorKind::NotFound
    );
    // A record that is not type 20 is refused by name.
    let mut t = DssBuffer::new(fixture_bytes("synthetic_python7.dss")).unwrap();
    let c = t.catalog().unwrap();
    let p = (0..c.len())
        .find(|&i| (100..200).contains(&c.record_type[i]) && c.status[i] == "live")
        .map(|i| c.stored_pathname[i].clone())
        .unwrap();
    assert_eq!(
        t.read_location(&p).unwrap_err().kind,
        ErrorKind::Unsupported
    );
}

/// Recovery stays opt-in: the default mode never lists or reads what only recovery finds;
/// recovery lists it, and every integrity check still applies.
#[test]
fn recovery_is_explicit_and_keeps_integrity_checks() {
    let bytes = fixture_bytes("synthetic_recovery7.dss");
    let mut d = DssBuffer::new(bytes.clone()).unwrap();
    let mut r = DssBuffer::new_recovery(bytes.clone()).unwrap();
    assert!(!d.recovery_mode() && r.recovery_mode());
    let (cd, cr) = (d.catalog().unwrap(), r.catalog().unwrap());
    assert!(
        cd.recovered.iter().all(|x| !x),
        "default lists no recovered"
    );
    assert!(cr.recovered.iter().any(|x| *x), "recovery lists them");
    assert!(cr.len() > cd.len());

    let mut outcomes = std::collections::BTreeMap::<(bool, &str), usize>::new();
    let mut unavailable = 0;
    for (recover, f, n) in [(false, &mut d, cd.len()), (true, &mut r, cr.len())] {
        for i in 0..n {
            let p = f.entry_provenance(i).unwrap();
            *outcomes.entry((recover, p.outcome())).or_default() += 1;
            match (p.outcome(), f.read_record_at(i)) {
                ("not_live", Err(e)) => assert_eq!(e.kind, ErrorKind::Index),
                ("unavailable", Err(e)) => {
                    assert_eq!(e.kind, ErrorKind::Unavailable, "{e}");
                    assert_eq!(e.kind.error_name(), "DssRecordUnavailableError");
                    let u = e.unavailable.unwrap();
                    assert_eq!(u.problem, p.info_problem.unwrap());
                    assert_eq!(u.info_address, p.info_address);
                    unavailable += 1;
                }
                ("unsupported_record_type", Err(e)) => {
                    assert_eq!(e.kind, ErrorKind::Unsupported, "{e}")
                }
                ("recovered" | "unreachable_by_pathname" | "ordinary", Ok((q, _))) => {
                    assert_eq!(q, p)
                }
                (o, got) => panic!("row {i}: outcome {o}, read {:?}", got.map(|x| x.0)),
            }
        }
    }
    eprintln!("P40 synthetic_recovery7 outcomes: {outcomes:?}");
    assert!(unavailable >= 2, "the zeroed info block in both modes");
    for want in [
        (false, "ordinary"),
        (false, "unavailable"),
        (false, "unreachable_by_pathname"),
        (true, "recovered"),
    ] {
        assert!(outcomes.contains_key(&want), "{want:?}: {outcomes:?}");
    }
}

/// On a file with nothing to recover, recovery mode reads exactly what the default reads.
#[test]
fn recovery_mode_is_a_no_op_on_healthy_files() {
    for (name, bytes) in ROUTE_FIXTURES {
        if *name == "synthetic_recovery7.dss" {
            continue;
        }
        let mut d = DssBuffer::new(bytes.to_vec()).unwrap();
        let mut r = DssBuffer::new_recovery(bytes.to_vec()).unwrap();
        let (cd, cr) = (d.catalog().unwrap(), r.catalog().unwrap());
        assert_eq!(cd.stored_pathname, cr.stored_pathname, "{name}");
        for p in &cd.stored_pathname {
            match (d.read_record(p), r.read_record(p)) {
                (Ok(a), Ok(b)) => assert!(same_ts(&a.ts, &b.ts), "{name} {p}"),
                (Err(a), Err(b)) => assert_eq!(a.kind, b.kind, "{name} {p}"),
                (a, b) => panic!("{name} {p}: {:?} vs {:?}", a.err(), b.err()),
            }
        }
    }
}

fn profile_rec(bytes: &[u8]) -> (String, RecordRef) {
    let mut f = Dss7File::new(Cursor::new(bytes.to_vec())).unwrap();
    let cat = f.catalog().unwrap();
    let i = cat
        .resolved_live()
        .into_iter()
        .find(|&i| matches!(cat.catalog.entries[i].record_type, 102 | 107))
        .unwrap();
    let e = &cat.catalog.entries[i];
    let r = f.lookup(&e.pathname).unwrap().unwrap();
    (lossy(&e.pathname), r)
}

fn dataset_of(p: &str) -> String {
    dataset_key(p).unwrap()
}

#[test]
fn profile_limits_refuse_before_reading() {
    let bytes = fixture_bytes("synthetic_profile7.dss");
    let (p, _) = profile_rec(&bytes);
    let dataset = dataset_of(&p);
    let mut b = DssBuffer::new(bytes).unwrap();
    let whole = b
        .read_profile(&dataset, None, false, MAX_BLOCKS, MAX_CELLS)
        .unwrap();
    let (rows, cols) = (
        whole.profile.times.len() as u64,
        whole.profile.depths.len() as u64,
    );
    assert!(rows > 1 && cols > 1);

    // A whole-dataset read is limited too: every period of the range gets a row.
    let e = b
        .read_profile(&dataset, None, false, MAX_BLOCKS, cols)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("maxCells"), "{e}");
    let e = b
        .read_profile(&dataset, None, false, 1, MAX_CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("maxBlocks"), "{e}");

    // The record route: a block of rows x depths.
    let e = b.read_profile_record(&p, 1).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(b.read_profile_record(&p, MAX_CELLS).is_ok());

    // A wide window on the caller's thread is refused at once.
    let t = std::time::Instant::now();
    let e = b
        .read_profile(&dataset, Some((0, 1 << 44)), true, MAX_BLOCKS, MAX_CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(t.elapsed().as_secs_f64() < REFUSAL_SECONDS);
    // A reversed window, and dates beyond the core's limit, stay typed.
    let e = b
        .read_profile(&dataset, Some((10, 0)), true, MAX_BLOCKS, MAX_CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument);
    let e = b
        .read_profile(&dataset, Some((0, 1 << 52)), true, u64::MAX, MAX_CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");

    // The limits are upper estimates: the exact cell count plus the two margin rows reads.
    assert!(b
        .read_profile(
            &dataset,
            None,
            false,
            MAX_BLOCKS,
            (rows + 2 + 744 * 2) * cols
        )
        .is_ok());
}

/// Hostile sizes never reach `dss-core`'s assembly: a header claiming 90,000 depths (within
/// the core's limit, and it would expand every row) is refused from its size alone.
#[test]
fn hostile_sizes_are_refused_before_assembly() {
    let mut bytes = fixture_bytes("synthetic_profile7.dss");
    let (p, rec) = profile_rec(&bytes);
    let dataset = dataset_of(&p);

    // INT_HEAD_profileDepthsNumber, word 3 of the internal header.
    let at = rec.info.internal_header.address as usize * 8 + 3 * 4;
    let saved = bytes[at..at + 4].to_vec();
    bytes[at..at + 4].copy_from_slice(&90_000i32.to_le_bytes());
    let mut b = DssBuffer::new(bytes.clone()).unwrap();
    let e = b.read_profile_record(&p, MAX_CELLS).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    let e = b
        .read_profile(&dataset, None, true, MAX_BLOCKS, MAX_CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    // Beyond the core's own limit: its typed refusal, before it allocates.
    bytes[at..at + 4].copy_from_slice(&2_000_000_000i32.to_le_bytes());
    let mut b = DssBuffer::new(bytes.clone()).unwrap();
    let e = b.read_profile_record(&p, MAX_CELLS).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Format, "{e}");
    bytes[at..at + 4].copy_from_slice(&saved);

    // `kbinDates` (bin entry word 6) claiming tens of thousands of years widens the
    // dataset range `dss-core` would assemble.
    let at = rec.bin_entry_address as usize * 8 + 6 * 8;
    let first = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    bytes[at + 4..at + 8].copy_from_slice(&(first + 30_000_000).to_le_bytes());
    let mut b = DssBuffer::new(bytes).unwrap();
    let t = std::time::Instant::now();
    let e = b
        .read_profile(&dataset, None, true, MAX_BLOCKS, MAX_CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(
        t.elapsed().as_secs_f64() < REFUSAL_SECONDS,
        "refused before any walk"
    );
}

/// `p20_hec_rts7.dss` with `kbinDates` (bin entry word 6) of its first 1-minute dataset
/// moved `days` past the first date. Before the row limit, 36,500 days returned
/// 52,561,440 rows from two stored blocks.
fn patched_kbin_dates(days: i32) -> (Vec<u8>, String) {
    let mut bytes = fixture_bytes("p20_hec_rts7.dss");
    let mut f = Dss7File::new(Cursor::new(bytes.clone())).unwrap();
    let cat = f.catalog().unwrap();
    let i = cat
        .resolved_live()
        .into_iter()
        .find(|&i| matches!(cat.catalog.entries[i].record_type, 100 | 105))
        .unwrap();
    let text = lossy(&cat.catalog.entries[i].pathname);
    let key = dataset_of(&text);
    let mut first = None;
    for j in cat.resolved_live() {
        let e = &cat.catalog.entries[j];
        if dataset_key(&lossy(&e.pathname)).as_deref() == Some(key.as_str()) {
            let r = f.lookup(&e.pathname).unwrap().unwrap();
            let at = r.bin_entry_address as usize * 8 + 6 * 8;
            let a = *first
                .get_or_insert_with(|| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
            bytes[at + 4..at + 8].copy_from_slice(&(a + days).to_le_bytes());
        }
    }
    (bytes, text)
}

/// The P35 exposure: a two-block dataset whose stored dates claim a century.
#[test]
fn patched_kbin_dates_cannot_expand_a_dataset() {
    let (bytes, p) = patched_kbin_dates(36_500);
    let mut b = DssBuffer::new(bytes).unwrap();
    let t = std::time::Instant::now();
    let e = b.read_dataset(&p, None, false).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("maxRows"), "{e}");
    assert!(
        t.elapsed().as_secs_f64() < REFUSAL_SECONDS,
        "refused before assembling"
    );
    // A limit below the range refuses, whatever `maxBlocks` says.
    let e = b
        .read_dataset_limited(&p, None, false, u64::MAX, 52_000_000)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");

    // Within the limit, the same patched range reads, and shows what the limit counts.
    let (bytes, p) = patched_kbin_dates(3_650);
    let mut b = DssBuffer::new(bytes).unwrap();
    let got = b.read_dataset(&p, None, false).unwrap();
    assert_eq!(got.ts.times.len(), 5_257_440);
    assert_eq!(got.blocks_found, 2);
    assert_eq!(got.blocks_missing, 3_649);
    let e = b
        .read_dataset_limited(&p, None, false, MAX_BLOCKS, 5_000_000)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    let e = b
        .read_dataset_limited(&p, None, false, 100, MAX_CELLS)
        .unwrap_err();
    assert!(e.message.contains("maxBlocks"), "{e}");
    // A window expands time the same way.
    let (s, day) = (got.ts.times[0], 86_400);
    let e = b
        .read_dataset(&p, Some((s, s + 36_500 * day)), false)
        .unwrap_err();
    assert!(e.message.contains("maxRows"), "{e}");
    assert!(b
        .read_dataset_limited(&p, Some((s, s + day)), false, MAX_BLOCKS, 1_500)
        .is_ok());
}

/// DSS6: regular datasets also get a row per period between stored blocks (`zrrtsi.f`), and
/// a header's value count cannot expand a record beyond its stored words.
#[test]
fn dss6_rows_are_limited_and_records_stay_file_bounded() {
    let mut b = DssBuffer::new(fixture_bytes("synthetic_python6.dss")).unwrap();
    let c = b.catalog().unwrap();
    let p = (0..c.len())
        .find(|&i| {
            let up = c.pathname[i].to_uppercase();
            c.status[i] == "live" && up.contains("/1HOUR/") && !up.contains("TS-P")
        })
        .map(|i| c.stored_pathname[i].clone())
        .expect("hourly series");
    let rows = b.read_dataset(&p, None, false).unwrap().ts.times.len() as u64;
    let e = b
        .read_dataset_limited(&p, None, false, MAX_BLOCKS, rows / 2)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("maxRows"), "{e}");
    let s = b.read_dataset(&p, None, false).unwrap().ts.times[0];
    let e = b
        .read_dataset(&p, Some((s, s + 3_600 * 20_000_000)), false)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");

    let mut bytes = fixture_bytes("synthetic_python6.dss");
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let info = f.lookup(p.as_bytes()).unwrap().unwrap().info;
    drop(f);
    // KILNDA is word 7 of the info block (`Dss6Info`).
    let at = dss6::source::word_byte_offset(info.address + 7).unwrap() as usize;
    bytes[at..at + 4].copy_from_slice(&i32::MAX.to_le_bytes());
    let mut b = DssBuffer::new(bytes).unwrap();
    assert!(b.read_record(&p).is_err(), "header cannot expand a record");
}

#[test]
fn index_arguments_never_wrap() {
    for bad in [0.5, 1.5, f64::NAN, f64::INFINITY, -f64::INFINITY] {
        assert_eq!(index_arg(bad).unwrap_err().kind, ErrorKind::InvalidArgument);
    }
    for out in [-1.0, 4_294_967_296.0, 1e300] {
        assert_eq!(index_arg(out).unwrap_err().kind, ErrorKind::Index, "{out}");
    }
    assert_eq!(index_arg(0.0).unwrap(), 0);
    assert_eq!(index_arg(7.0).unwrap(), 7);
}

/// Every new route on hostile input: truncations and flipped bytes never panic.
#[test]
fn damaged_buffers_do_not_panic() {
    let exercise = |b: &mut DssBuffer, names: &[String]| {
        if let Ok(c) = b.catalog() {
            for i in 0..c.len().min(64) {
                let _ = b.entry_provenance(i);
                let _ = b.read_record_at(i);
            }
        }
        for p in names {
            let _ = b.read_profile(p, None, true, MAX_BLOCKS, MAX_CELLS);
            let _ = b.read_profile_record(p, MAX_CELLS);
            let _ = b.read_location(p);
            let _ = b.read_associated_location(p);
            let _ = b.read_record(p);
        }
    };
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for name in [
        "synthetic_profile7.dss",
        "synthetic_location7.dss",
        "synthetic_location_uh6.dss",
        "synthetic_recovery7.dss",
    ] {
        let bytes = fixture_bytes(name);
        let names = DssBuffer::new_recovery(bytes.clone())
            .unwrap()
            .catalog()
            .unwrap()
            .stored_pathname;
        let step = (bytes.len() / 61).max(1);
        for cut in (0..bytes.len()).step_by(step) {
            for recover in [false, true] {
                let open = if recover {
                    DssBuffer::new_recovery(bytes[..cut].to_vec())
                } else {
                    DssBuffer::new(bytes[..cut].to_vec())
                };
                if let Ok(mut b) = open {
                    exercise(&mut b, &names);
                }
            }
        }
        for _ in 0..150 {
            let mut m = bytes.clone();
            for _ in 0..4 {
                // xorshift64: deterministic, no dependency.
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let at = (state % m.len() as u64) as usize;
                m[at] = (state >> 56) as u8;
            }
            if let Ok(mut b) = DssBuffer::new_recovery(m) {
                exercise(&mut b, &names);
            }
        }
    }
}

/// The digest table checked inside WASM (`node_routes.rs`) holds on the host too. Set
/// `DSS_WASM_PRINT_DIGESTS=1` to print the table after a deliberate change.
#[test]
fn recorded_route_digests_hold_on_host() {
    let print = std::env::var_os("DSS_WASM_PRINT_DIGESTS").is_some();
    let mut bad = Vec::new();
    for (name, bytes) in ROUTE_FIXTURES {
        for recover in [false, true] {
            let d = route_digest(bytes, recover);
            if print {
                println!(
                    "    (\"{name}\", {recover}, RouteDigest {{ entries: {}, at_records: {}, profiles: {}, locations: {}, associated: {}, errors: {}, values: {}, hash: 0x{:016x} }}),",
                    d.entries, d.at_records, d.profiles, d.locations, d.associated, d.errors, d.values, d.hash
                );
            }
            let want = common::expected_routes::EXPECTED
                .iter()
                .find(|(n, r, _)| n == name && *r == recover)
                .map(|(_, _, d)| *d);
            if want != Some(d) {
                bad.push(format!(
                    "{name} recover={recover}: got {d:?}, recorded {want:?}"
                ));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The crate's own code stays free of file, thread and shared-state use.
#[test]
fn source_uses_no_filesystem_or_threads() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for f in ["lib.rs", "reader.rs", "reader/routes.rs"] {
        let text = std::fs::read_to_string(src.join(f)).unwrap();
        for word in [
            "std::fs",
            "std::thread",
            "std::sync",
            "std::net",
            "std::process",
            "std::env",
            "File::open",
            "FileSource",
            "Mutex",
            "Arc<",
            "write::",
        ] {
            assert!(!text.contains(word), "{f} uses {word}");
        }
    }
}

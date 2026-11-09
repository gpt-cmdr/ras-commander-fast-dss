//! P35 host tests of the WASM read layer: every public fixture read through
//! `dss_wasm::reader` (in-memory bytes) equals the `dss-core` reads from the file path,
//! the recorded digests (also checked inside WASM by `node.rs`) hold, and errors carry
//! the typed kind.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{dataset_key, digest, FIXTURES};
use dss_core::dss6::{self, DatasetOptions, Dss6File};
use dss_core::dss7::{self, Dss7File, TsReadOptions};
use dss_core::{GridReadOptions, TimeSeries, TsValues};
use dss_wasm::reader::{
    count_arg, lossy, window_blocks, DssBuffer, ErrorKind, TsData, DEFAULT_MAX_WINDOW_BLOCKS,
};
use std::path::{Path, PathBuf};

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

/// Bitwise equality (NaN payloads included), which `PartialEq` cannot give.
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

fn same_f32s(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(p, q)| p.to_bits() == q.to_bits())
}

/// Counts of reads compared for one file.
#[derive(Default, Debug)]
struct Tally {
    records: usize,
    datasets: usize,
    paired: usize,
    grids: usize,
    both_failed: usize,
}

fn compare_v7(path: &Path, buf: &mut DssBuffer, t: &mut Tally) {
    let mut f = Dss7File::open(path).expect("open dss7");
    let cat = f.catalog().expect("catalog");
    let mine = buf.catalog().expect("wasm catalog");
    assert_eq!(
        mine.len(),
        cat.catalog.entries.len(),
        "{path:?} catalog size"
    );
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        assert_eq!(mine.stored_pathname[i], lossy(&e.pathname));
        assert_eq!(mine.record_type[i], e.record_type);
    }
    let mut seen = std::collections::BTreeSet::new();
    for i in cat.resolved_live() {
        let e = &cat.catalog.entries[i];
        // Latin-1 pathnames come back through the Latin-1 encoding of the text.
        let p = lossy(&e.pathname);
        // Reads by pathname use the hash lookup (not the walk), as the Python binding
        // does outside recovery mode; an entry the lookup misses is not found.
        let Ok(Some(rec)) = f.lookup(&e.pathname) else {
            assert!(buf.read_record(&p).is_err(), "{path:?} {p}");
            t.both_failed += 1;
            continue;
        };
        match e.record_type {
            100..=199 => {
                let native = f.read_ts_record(&rec);
                match (native, buf.read_record(&p)) {
                    (Ok(n), Ok(w)) => {
                        assert!(same_ts(&n.ts, &w.ts), "{path:?} {p} record");
                        assert_eq!(n.meta.precision, w.precision.unwrap());
                        t.records += 1;
                    }
                    (Err(_), Err(_)) => t.both_failed += 1,
                    (n, w) => panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err()),
                }
                if let Some(k) = dataset_key(&p) {
                    if seen.insert(k) {
                        let opts = TsReadOptions {
                            window: None,
                            trim_missing: true,
                        };
                        let native = f.read_ts(e.pathname.as_slice(), &opts);
                        match (native, buf.read_dataset(&p, None, true)) {
                            (Ok(n), Ok(w)) => {
                                assert!(same_ts(&n.ts, &w.ts), "{path:?} {p} dataset");
                                t.datasets += 1;
                            }
                            (Err(_), Err(_)) => t.both_failed += 1,
                            (n, w) => {
                                panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err())
                            }
                        }
                    }
                }
            }
            200..=299 => match (f.read_pd_record(&rec), buf.read_paired_data(&p)) {
                (Ok(n), Ok(w)) => {
                    assert_eq!(n.pd.labels, w.pd.labels);
                    assert!(same_values(&n.pd.ordinates, &w.pd.ordinates));
                    assert!(same_values(&n.pd.values, &w.pd.values), "{path:?} {p} pd");
                    assert_eq!(n.pd.units_dependent, w.pd.units_dependent);
                    t.paired += 1;
                }
                (Err(_), Err(_)) => t.both_failed += 1,
                (n, w) => panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err()),
            },
            400..=499 => {
                let o = GridReadOptions::default();
                match (f.read_grid_record_with(&rec, &o), buf.read_grid(&p, &o)) {
                    (Ok(n), Ok(w)) => {
                        assert!(same_f32s(&n.data, &w.grid.data), "{path:?} {p} grid");
                        assert_eq!((n.nx, n.ny), (w.grid.nx, w.grid.ny));
                        assert_eq!(n.srs_definition, w.grid.srs_definition);
                        assert_eq!(n.albers, w.grid.albers);
                        t.grids += 1;
                    }
                    (Err(_), Err(_)) => t.both_failed += 1,
                    (n, w) => panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err()),
                }
            }
            _ => {}
        }
    }
}

fn compare_v6(path: &Path, buf: &mut DssBuffer, t: &mut Tally) {
    let f = Dss6File::open(path).expect("open dss6");
    let walk = f.walk_bins(&dss6::CatalogOptions::default()).expect("walk");
    let mine = buf.catalog().expect("wasm catalog");
    assert_eq!(mine.len(), walk.entries.len(), "{path:?} catalog size");
    let mut seen_live = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    for (i, e) in walk.entries.iter().enumerate() {
        assert_eq!(mine.stored_pathname[i], lossy(&e.pathname));
        assert_eq!(mine.record_type[i], e.record_type);
        if mine.status[i] != "live" || !seen_live.insert(e.pathname.clone()) {
            continue;
        }
        let p = lossy(&e.pathname);
        match e.record_type {
            100..=199 => {
                match (f.read_ts_record(&e.pathname), buf.read_record(&p)) {
                    (Ok(n), Ok(w)) => {
                        assert!(same_ts(&n.to_timeseries(), &w.ts), "{path:?} {p} record");
                        t.records += 1;
                    }
                    (Err(_), Err(_)) => t.both_failed += 1,
                    (n, w) => panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err()),
                }
                if let Some(k) = dataset_key(&p) {
                    if seen.insert(k) {
                        let native = f.read_ts_dataset(&e.pathname, &DatasetOptions::whole());
                        match (native, buf.read_dataset(&p, None, true)) {
                            (Ok(n), Ok(w)) => {
                                assert!(same_ts(&n.series, &w.ts), "{path:?} {p} dataset");
                                assert_eq!(n.report.blocks_found, w.blocks_found);
                                t.datasets += 1;
                            }
                            (Err(_), Err(_)) => t.both_failed += 1,
                            (n, w) => {
                                panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err())
                            }
                        }
                    }
                }
            }
            200..=299 => match (f.read_pd(&e.pathname), buf.read_paired_data(&p)) {
                (Ok(n), Ok(w)) => {
                    assert_eq!(n.pd.labels, w.pd.labels);
                    assert!(same_values(&n.pd.ordinates, &w.pd.ordinates));
                    assert!(same_values(&n.pd.values, &w.pd.values), "{path:?} {p} pd");
                    t.paired += 1;
                }
                (Err(_), Err(_)) => t.both_failed += 1,
                (n, w) => panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err()),
            },
            400..=499 => {
                let o = GridReadOptions::default();
                match (f.read_grid_with(&e.pathname, &o), buf.read_grid(&p, &o)) {
                    (Ok(n), Ok(w)) => {
                        assert!(same_f32s(&n.data, &w.grid.data), "{path:?} {p} grid");
                        assert_eq!((n.nx, n.ny), (w.grid.nx, w.grid.ny));
                        assert_eq!(n.albers, w.grid.albers);
                        t.grids += 1;
                    }
                    (Err(_), Err(_)) => t.both_failed += 1,
                    (n, w) => panic!("{path:?} {p}: native {:?} vs wasm {:?}", n.err(), w.err()),
                }
            }
            _ => {}
        }
    }
}

/// Every public fixture: catalog, every live record and every dataset read from bytes
/// equals the `dss-core` read from the file.
#[test]
fn every_public_fixture_equals_dss_core() {
    let mut total = Tally::default();
    let files = all_public_fixtures();
    assert!(
        files.len() >= 40,
        "fixtures/public has {} files",
        files.len()
    );
    for path in &files {
        let bytes = std::fs::read(path).expect("read fixture");
        let mut buf = match DssBuffer::new(bytes) {
            Ok(b) => b,
            Err(e) => panic!("{path:?}: {e}"),
        };
        let mut t = Tally::default();
        if buf.version() == 7 {
            compare_v7(path, &mut buf, &mut t);
        } else {
            compare_v6(path, &mut buf, &mut t);
        }
        if t.both_failed > 0 {
            eprintln!(
                "  {:?}: {} reads fail on both sides",
                path.file_name().unwrap(),
                t.both_failed
            );
        }
        total.records += t.records;
        total.datasets += t.datasets;
        total.paired += t.paired;
        total.grids += t.grids;
        total.both_failed += t.both_failed;
    }
    eprintln!("P35 compared {} files: {total:?}", files.len());
    assert!(total.records > 1000 && total.datasets > 100);
    assert!(total.paired >= 20 && total.grids >= 20);
}

/// The digest table checked inside WASM (`node.rs`) holds on the host too. Set
/// `DSS_WASM_PRINT_DIGESTS=1` to print the table after a deliberate change.
#[test]
fn recorded_digests_hold_on_host() {
    let print = std::env::var_os("DSS_WASM_PRINT_DIGESTS").is_some();
    let mut bad = Vec::new();
    for (name, bytes) in FIXTURES {
        let d = digest(bytes);
        if print {
            println!(
                "    (\"{name}\", Digest {{ entries: {}, records: {}, datasets: {}, paired: {}, grids: {}, errors: {}, values: {}, hash: 0x{:016x} }}),",
                d.entries, d.records, d.datasets, d.paired, d.grids, d.errors, d.values, d.hash
            );
        }
        let want = common::expected::EXPECTED
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| *d);
        if want != Some(d) {
            bad.push(format!("{name}: got {d:?}, recorded {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

fn open(name: &str) -> DssBuffer {
    let bytes = std::fs::read(public_dir().join(name)).expect("fixture");
    DssBuffer::new(bytes).expect("open")
}

fn first_of(buf: &mut DssBuffer, lo: i32, hi: i32) -> String {
    let c = buf.catalog().unwrap();
    (0..c.len())
        .find(|&i| c.status[i] == "live" && (lo..=hi).contains(&c.record_type[i]))
        .map(|i| c.stored_pathname[i].clone())
        .expect("record of that type")
}

#[test]
fn errors_carry_typed_kinds() {
    // Not a DSS file.
    let e = DssBuffer::new(b"not a dss file".to_vec()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Format);
    assert_eq!(e.kind.error_name(), "DssFormatError");
    let e = DssBuffer::new(Vec::new()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Format);

    for name in ["synthetic_pd7.dss", "synthetic_pd6.dss"] {
        let mut b = open(name);
        let missing = "/NO/SUCH/RECORD/01JAN2000/1HOUR/X/";
        assert_eq!(
            b.read_record(missing).unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            b.read_paired_data("/NO/SUCH/PD///X/").unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            b.read_grid(
                "/NO/SUCH/GRID/01JAN2000:0000/01JAN2000:0100/X/",
                &Default::default()
            )
            .unwrap_err()
            .kind,
            ErrorKind::NotFound
        );
        // A paired-data record read as a time series: typed refusal.
        let pd = first_of(&mut b, 200, 299);
        let e = b.read_record(&pd).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unsupported, "{name}: {e}");
        assert_eq!(e.kind.as_str(), "unsupported");
    }
    let mut b = open("synthetic_python7.dss");
    let ts = first_of(&mut b, 100, 199);
    let e = b.read_dataset(&ts, Some((100, 0)), true).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument);
    // A grid limit is a DSS error.
    let mut g = open("real_grid7.dss");
    let p = first_of(&mut g, 400, 499);
    let o = GridReadOptions {
        max_cells: 1,
        ..GridReadOptions::default()
    };
    assert_eq!(g.read_grid(&p, &o).unwrap_err().kind, ErrorKind::Dss);
}

#[test]
fn windowed_dataset_equals_dss_core() {
    for name in ["p20_hec_rts7.dss", "synthetic_python6.dss", "castro.dss"] {
        let path = public_dir().join(name);
        let mut b = open(name);
        let c = b.catalog().unwrap();
        let p = (0..c.len())
            .find(|&i| {
                (100..=109).contains(&c.record_type[i]) && !c.pathname[i].contains("TS-PATTERN")
            })
            .map(|i| c.stored_pathname[i].clone())
            .unwrap();
        let whole: TsData = b.read_dataset(&p, None, true).unwrap();
        let (s, e) = (whole.ts.times[0], whole.ts.times[whole.ts.times.len() / 2]);
        let mine = b.read_dataset(&p, Some((s, e)), false).unwrap();
        let native = if b.version() == 7 {
            let mut f = Dss7File::open(&path).unwrap();
            let o = TsReadOptions {
                window: Some(dss7::TsWindow { start: s, end: e }),
                trim_missing: false,
            };
            f.read_ts(p.as_bytes(), &o).unwrap().ts
        } else {
            let f = Dss6File::open(&path).unwrap();
            let o = DatasetOptions {
                window: Some(dss6::TimeWindow::new(s, e)),
                trim_missing: false,
                ..DatasetOptions::whole()
            };
            f.read_ts_dataset(p.as_bytes(), &o).unwrap().series
        };
        assert!(same_ts(&native, &mine.ts), "{name} window");
        assert!(
            mine.ts.times.first() >= Some(&s) && mine.ts.times.last() <= Some(&e),
            "{name} {p} window {s}..{e}: {:?}..{:?}",
            mine.ts.times.first(),
            mine.ts.times.last()
        );
    }
}

/// Hostile input: every truncation of two small files, read every way, never panics.
#[test]
fn truncated_buffers_do_not_panic() {
    for name in [
        "synthetic_python7.dss",
        "synthetic_pd6.dss",
        "synthetic_grid7.dss",
    ] {
        let bytes = std::fs::read(public_dir().join(name)).unwrap();
        let full = {
            let mut b = DssBuffer::new(bytes.clone()).unwrap();
            let c = b.catalog().unwrap();
            c.stored_pathname
        };
        let step = (bytes.len() / 97).max(1);
        for cut in (0..bytes.len()).step_by(step) {
            let Ok(mut b) = DssBuffer::new(bytes[..cut].to_vec()) else {
                continue;
            };
            let _ = b.catalog();
            for p in &full {
                let _ = b.read_record(p);
                let _ = b.read_dataset(p, None, true);
                let _ = b.read_paired_data(p);
                let _ = b.read_grid(p, &GridReadOptions::default());
            }
        }
    }
}

/// Regression: on every dataset of every public fixture, a window covering the whole
/// dataset fits the default block limit and reads; the estimate never undercounts the
/// blocks the whole-dataset read found.
#[test]
fn whole_dataset_windows_fit_the_default_block_limit() {
    let (mut checked, mut widest) = (0, 0);
    for path in all_public_fixtures() {
        let mut b = DssBuffer::new(std::fs::read(&path).unwrap()).unwrap();
        let c = b.catalog().unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..c.len() {
            if c.status[i] != "live" || !(100..200).contains(&c.record_type[i]) {
                continue;
            }
            let p = c.stored_pathname[i].clone();
            if !dataset_key(&p).is_some_and(|k| seen.insert(k)) {
                continue;
            }
            let Ok(w) = b.read_dataset(&p, None, false) else {
                continue;
            };
            // Irregular records may store times out of order.
            let (Some(&s), Some(&e)) = (w.ts.times.iter().min(), w.ts.times.iter().max()) else {
                continue;
            };
            let n = window_blocks(&p, s, e);
            assert!(n <= DEFAULT_MAX_WINDOW_BLOCKS, "{path:?} {p}: {n} blocks");
            widest = widest.max(n);
            assert!(
                n as usize >= w.blocks_found,
                "{path:?} {p}: {n} < {}",
                w.blocks_found
            );
            if let Err(err) = b.read_dataset(&p, Some((s, e)), false) {
                assert_ne!(err.kind, ErrorKind::InvalidArgument, "{path:?} {p}: {err}");
            }
            checked += 1;
        }
    }
    eprintln!("P35 whole-dataset windows: {checked} datasets, widest {widest} blocks");
    assert!(checked > 400, "{checked}");
}

#[test]
fn wide_windows_are_refused_before_reading() {
    let mut b = open("p21_hec_its7.dss");
    let c = b.catalog().unwrap();
    let p = c
        .stored_pathname
        .iter()
        .find(|p| p.to_ascii_uppercase().contains("/IR-DAY/"))
        .unwrap()
        .clone();
    let t = std::time::Instant::now();
    let e = b.read_dataset(&p, Some((0, 1 << 40)), true).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(t.elapsed().as_secs_f64() < 0.5);
    // Explicit limits: 1 block refuses a two-day window; 4 lets the read run.
    let day = 86_400;
    let e = b
        .read_dataset_bounded(&p, Some((0, 2 * day)), true, 1)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument);
    if let Err(e) = b.read_dataset_bounded(&p, Some((0, 2 * day)), true, 4) {
        assert_ne!(e.kind, ErrorKind::InvalidArgument, "{e}");
    }

    // Block sizes from the E part (zbegdt6/zirbeg6 rules, 15-59 minutes as daily).
    let y100 = 100 * 365 * day;
    assert_eq!(window_blocks("/A/B/C//IR-DAY/F/", 0, y100), 36_502);
    assert_eq!(window_blocks("/A/B/C//IR-Century/F/", 0, y100), 2);
    assert_eq!(window_blocks("/A/B/C//1Hour/F/", 0, y100), 1_305);
    assert_eq!(window_blocks("/A/B/C//15Minute/F/", 0, y100), 36_502);
    assert_eq!(window_blocks("/A/B/C//1Day/F/", 0, y100), 102);
    assert_eq!(window_blocks("/A/B/C//UNKNOWN/F/", 0, y100), 36_502);
    assert_eq!(window_blocks("/A/B/C//1Day/F/", 5, 5), 2);
    assert!(window_blocks("x", i64::MIN, i64::MAX) > 1 << 47);
}

#[test]
fn count_arguments_never_wrap() {
    for bad in [
        -1.0,
        0.0,
        0.5,
        1.5,
        f64::NAN,
        f64::INFINITY,
        -f64::INFINITY,
        1e300,
        4_294_967_296.0,
    ] {
        let e = count_arg(bad, "maxCells", 1 << 30).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidArgument, "{bad}");
    }
    assert_eq!(count_arg(1.0, "maxCells", 1 << 30).unwrap(), 1);
    assert_eq!(
        count_arg(1_073_741_824.0, "maxCells", 1 << 30).unwrap(),
        1 << 30
    );
}

#[test]
fn profile_records_name_themselves() {
    let mut b = open("synthetic_profile7.dss");
    let c = b.catalog().unwrap();
    let i = (0..c.len())
        .find(|&i| c.status[i] == "live" && matches!(c.record_type[i], 102 | 107 | 112 | 117))
        .unwrap();
    let e = b.read_record(&c.stored_pathname[i]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.message.contains("profile record"), "{e}");
}

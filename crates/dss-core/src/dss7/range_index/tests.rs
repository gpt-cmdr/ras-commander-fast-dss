//! Differential tests: the [`IndexedDss7Catalog`] index against the scan
//! ([`dataset_extrema_scan`]) on synthetic catalogs, on every public DSS7 fixture (range
//! extrema and end-to-end whole reads, value bits) and, when `DSS7_RANGE_GOLDEN_DIR`
//! names a directory of DSS7 files, on those files too.

use super::*;
use crate::catalog::{CatalogEntry, RecordStatus};
use crate::dss7::{CatalogOptions, Dss7EntryMeta, Dss7File, Dss7Profile, Dss7Ts, TsError};
use crate::dss7::{TsReadOptions, MAX_PATHNAME_BYTES};
use crate::header::{DssVersion, FileSignature};
use crate::timeseries::TsValues;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Deterministic xorshift64* generator (no test dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn pick<'a>(&mut self, xs: &[&'a [u8]]) -> &'a [u8] {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn mixed_case(r: &mut Rng, s: &[u8]) -> Vec<u8> {
    s.iter()
        .map(|&b| {
            if r.next() % 2 == 0 {
                b.to_ascii_uppercase()
            } else {
                b.to_ascii_lowercase()
            }
        })
        .collect()
}

fn entry(pathname: Vec<u8>, record_type: i32) -> CatalogEntry {
    CatalogEntry {
        pathname,
        record_type,
        status: RecordStatus::Other,
        raw_status: 0,
        stored_hash: 0,
        info_address: 0,
        bin_address: 0,
        last_write_ms: None,
    }
}

fn canonical_blank_d(p: [&[u8]; 6]) -> Option<Vec<u8>> {
    let i = iv::standard_interval(p[4])?;
    let ename = if i.pseudo_regular {
        format!("~{}", i.name)
    } else {
        i.name.to_string()
    };
    let mut c = p;
    c[3] = b"";
    c[4] = ename.as_bytes();
    let mut v = vec![b'/'];
    for part in c {
        v.extend_from_slice(part);
        v.push(b'/');
    }
    Some(v)
}

/// Every query a caller can make for the datasets of `cat`: each parsable stored
/// pathname as stored, upper and lower case, and blank-D with its E part canonicalised
/// as `read_ts` does; plus some that match nothing.
fn queries(cat: &Dss7Catalog) -> Vec<Vec<u8>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |q: Vec<u8>| {
        if seen.insert(q.clone()) {
            out.push(q);
        }
    };
    for e in &cat.catalog.entries {
        let Some(p) = path_parts(&e.pathname) else {
            continue;
        };
        push(e.pathname.clone());
        push(e.pathname.to_ascii_uppercase());
        push(e.pathname.to_ascii_lowercase());
        if let Some(v) = canonical_blank_d(p) {
            push(v);
        }
    }
    push(b"/NO/SUCH/DATASET//1Hour/X/".to_vec());
    push(b"//////".to_vec());
    out
}

/// Distinct blank-D canonical dataset names (what a whole read is asked for).
fn dataset_names(cat: &Dss7Catalog) -> Vec<Vec<u8>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in &cat.catalog.entries {
        if let Some(v) = path_parts(&e.pathname).and_then(canonical_blank_d) {
            if v.len() <= MAX_PATHNAME_BYTES && seen.insert(v.to_ascii_lowercase()) {
                out.push(v);
            }
        }
    }
    out.push(b"/NO/SUCH/DATASET//1Hour/X/".to_vec());
    out
}

/// Compares the index with the scan for `qs`; returns the number of queries with a
/// range. Panics (with a scrubbed message) on the first difference.
fn compare(ix: &IndexedDss7Catalog, qs: &[Vec<u8>], label: &str) -> usize {
    let mut with_range = 0;
    for (n, q) in qs.iter().enumerate() {
        let Some(want) = path_parts(q) else {
            continue;
        };
        let a = ix.dataset_extrema(&want);
        let b = dataset_extrema_scan(ix.catalog(), &want);
        assert_eq!(a, b, "{label}: query {n} differs");
        with_range += usize::from(a.is_some());
    }
    with_range
}

#[test]
fn synthetic_catalogs_match_scan() {
    let a: &[&[u8]] = &[b"", b"BASIN", b"Basin2"];
    let b: &[&[u8]] = &[b"LOC1", b"loc2", b"A B"];
    let c: &[&[u8]] = &[b"FLOW", b"STAGE", b"Flow"];
    let d: &[&[u8]] = &[
        b"01JAN2020",
        b"15Mar1999",
        b"01jan2020",
        b"",
        b"TS-PATTERN",
        b"notadate",
        b"31DEC1899",
        b"01JAN3000",
        b"01FEB2020",
    ];
    let e: &[&[u8]] = &[b"1HOUR", b"1Hour", b"~1Day", b"IR-MONTH", b"60MIN", b"1Day"];
    let f: &[&[u8]] = &[b"", b"OBS", b"obs", b"RUN:X"];
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut with_range = 0;
    for round in 0..200 {
        let n = (r.next() % 120) as usize;
        let mut cat = Dss7Catalog::default();
        for _ in 0..n {
            let path = match r.next() % 20 {
                // Malformed pathnames are skipped by both.
                0 => b"/A/B/C/D/E/".to_vec(),
                1 => b"no slashes".to_vec(),
                _ => {
                    let mut v = vec![b'/'];
                    for part in [
                        r.pick(a),
                        r.pick(b),
                        r.pick(c),
                        r.pick(d),
                        r.pick(e),
                        r.pick(f),
                    ] {
                        v.extend(mixed_case(&mut r, part));
                        v.push(b'/');
                    }
                    v
                }
            };
            let rtype = [95, 100, 105, 110, 115, 102, 199, 200, 420][(r.next() % 9) as usize];
            cat.catalog.entries.push(entry(path, rtype));
            cat.meta.push(Dss7EntryMeta {
                raw_status: [-1, 0, 1, 2, 3, 4, 11, 12][(r.next() % 8) as usize],
                ..Dss7EntryMeta::default()
            });
        }
        // Some catalogs have fewer meta rows than entries (status -1 for the rest).
        if round % 7 == 0 {
            let keep = cat.meta.len() / 2;
            cat.meta.truncate(keep);
        }
        let mut qs = queries(&cat);
        // Random queries, including ones whose parts differ only in case.
        for _ in 0..50 {
            let mut v = vec![b'/'];
            for part in [
                r.pick(a),
                r.pick(b),
                r.pick(c),
                r.pick(d),
                r.pick(e),
                r.pick(f),
            ] {
                v.extend(mixed_case(&mut r, part));
                v.push(b'/');
            }
            qs.push(v);
        }
        let ix = IndexedDss7Catalog::new(cat);
        with_range += compare(&ix, &qs, &format!("synthetic round {round}"));
    }
    assert!(with_range > 1000, "only {with_range} queries had a range");
}

#[test]
fn index_is_built_once_and_clones_answer_alike() {
    let mut cat = Dss7Catalog::default();
    cat.catalog
        .entries
        .push(entry(b"/A/B/FLOW/01JAN2020/1Hour/F/".to_vec(), 100));
    cat.meta.push(Dss7EntryMeta::default());
    let ix = IndexedDss7Catalog::new(cat);
    assert!(!ix.index_built());
    let want = path_parts(b"/a/b/flow//1HOUR/f/").unwrap();
    let j = iv::parse_date(b"01JAN2020").unwrap();
    assert_eq!(ix.dataset_extrema(&want), Some((j, j)));
    let first = ix.index_ptr().unwrap();
    assert_eq!(ix.dataset_extrema(&want), Some((j, j)));
    assert!(
        std::ptr::eq(first, ix.index_ptr().unwrap()),
        "index rebuilt"
    );
    assert_eq!(ix.index_len(), Some(1));
    // A clone is a copy of an immutable snapshot: same answers.
    let c = ix.clone();
    assert_eq!(c.dataset_extrema(&want), Some((j, j)));
    // Giving the catalog back drops the index; a new wrapper starts unbuilt.
    let back = IndexedDss7Catalog::new(ix.into_catalog());
    assert!(!back.index_built());
}

fn public_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public")
}

/// Every `.dss` file in `dir` whose signature says DSS7. A directory that cannot be
/// read fails the test.
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

/// Default and recovery-mode catalogs of a DSS7 file. Failure fails the test.
fn catalogs(f: &mut Dss7File<std::fs::File>, label: &str) -> [Dss7Catalog; 2] {
    let normal = f
        .catalog()
        .unwrap_or_else(|e| panic!("{label}: default catalog failed: {e}"));
    let recover = f
        .catalog_with(&CatalogOptions {
            recover_unlinked: true,
            ..CatalogOptions::default()
        })
        .unwrap_or_else(|e| panic!("{label}: recovery catalog failed: {e}"));
    [normal, recover]
}

fn open7(p: &Path) -> (Dss7File<std::fs::File>, String) {
    let name = p.file_name().unwrap().to_string_lossy().into_owned();
    let f = Dss7File::open(p).unwrap_or_else(|e| panic!("{name}: open failed: {e}"));
    (f, name)
}

#[test]
fn public_fixtures_match_scan() {
    let files = dss7_files(&public_dir());
    assert!(
        files.len() >= 10,
        "only {} DSS7 public fixtures",
        files.len()
    );
    let mut datasets = 0;
    for p in &files {
        let (mut f, name) = open7(p);
        for (k, cat) in catalogs(&mut f, &name).into_iter().enumerate() {
            let qs = queries(&cat);
            datasets += compare(&IndexedDss7Catalog::new(cat), &qs, &format!("{name} {k}"));
        }
    }
    assert!(datasets > 0);
}

/// The recovery fixture (`synthetic_range_recovery7.dss`): a recovered dated block supplies a later extremum than the
/// default catalog has, and orphan, duplicate and deleted dated candidates are present.
#[test]
fn recovery_fixture_extrema_differ_and_match_scan() {
    let p = public_dir().join("synthetic_range_recovery7.dss");
    let (mut f, name) = open7(&p);
    let [normal, recover] = catalogs(&mut f, &name);
    let want = path_parts(b"/P61/SITE/FLOW//1Hour/SYN/").unwrap();
    let (n, r) = (
        IndexedDss7Catalog::new(normal),
        IndexedDss7Catalog::new(recover),
    );
    let ne = n.dataset_extrema(&want);
    let re = r.dataset_extrema(&want);
    assert_eq!(ne, dataset_extrema_scan(n.catalog(), &want));
    assert_eq!(re, dataset_extrema_scan(r.catalog(), &want));
    let d = |s: &[u8]| iv::parse_date(s).unwrap();
    assert_eq!(ne, Some((d(b"01DEC2019"), d(b"01FEB2020"))));
    assert_eq!(re, Some((d(b"01DEC2019"), d(b"01MAR2020"))));
    assert!(compare(&n, &queries(n.catalog()), "fixture default") > 0);
    assert!(compare(&r, &queries(r.catalog()), "fixture recovery") > 0);
}

/// Bit-level fingerprint of a whole read: every value's bits, times, quality, all
/// metadata, or the error.
fn ts_bits(r: &Result<Dss7Ts, TsError>) -> String {
    match r {
        Ok(t) => format!(
            "ok {:?} {} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            t.ts.pathname,
            t.ts.record_type,
            t.ts.times,
            value_bits(&t.ts.values),
            t.ts.quality,
            t.ts.units,
            t.ts.data_type,
            t.ts.time_zone,
            t.meta
        ),
        Err(e) => format!("err {e:?}"),
    }
}

fn profile_bits(r: &Result<Dss7Profile, TsError>) -> String {
    match r {
        Ok(p) => format!(
            "ok {:?} {} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            p.pathname,
            p.record_type,
            p.times,
            value_bits(&p.depths),
            value_bits(&p.values),
            p.depth_units,
            p.value_units,
            p.data_type,
            p.time_zone,
            p.meta
        ),
        Err(e) => format!("err {e:?}"),
    }
}

fn value_bits(v: &TsValues) -> Vec<u64> {
    match v {
        TsValues::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
        TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
    }
}

/// End to end: every whole-dataset read (time series and profile, with and without
/// trim) of every public DSS7 fixture, default and recovery catalogs, through the scan
/// route (`read_*_with_catalog`) and the indexed route, compared bit for bit.
#[test]
fn public_fixture_whole_reads_scan_equals_indexed() {
    let files = dss7_files(&public_dir());
    let (mut reads, mut ok) = (0usize, 0usize);
    for p in &files {
        let (mut f, name) = open7(p);
        for cat in catalogs(&mut f, &name) {
            let names = dataset_names(&cat);
            let ix = IndexedDss7Catalog::new(cat.clone());
            for q in &names {
                for trim_missing in [false, true] {
                    let o = TsReadOptions {
                        window: None,
                        trim_missing,
                    };
                    let s = f.read_ts_with_catalog(&cat, q, &o);
                    let i = f.read_ts_with_indexed_catalog(&ix, q, &o);
                    assert_eq!(ts_bits(&s), ts_bits(&i), "{name}: ts read differs");
                    ok += usize::from(s.is_ok());
                    let s = f.read_ts_profile_with_catalog(&cat, q, &o);
                    let i = f.read_ts_profile_with_indexed_catalog(&ix, q, &o);
                    assert_eq!(
                        profile_bits(&s),
                        profile_bits(&i),
                        "{name}: profile differs"
                    );
                    ok += usize::from(s.is_ok());
                    reads += 2;
                }
            }
        }
    }
    assert!(ok > 100, "only {ok} of {reads} whole reads succeeded");
    eprintln!("public end-to-end: {reads} reads compared, {ok} succeeded");
}

/// External DSS7 files (opt-in, run with `--release`): `DSS7_RANGE_GOLDEN_DIR`
/// names a directory of `.dss` copies. Every query is compared on files where the scan
/// is cheap; on large files an evenly spread sample bounded by `DSS7_RANGE_SCAN_BUDGET`
/// candidate visits (default 1e9) per catalog. A DSS7 file that does not open or
/// catalog fails. Prints counts only.
#[test]
fn golden_copies_match_scan() {
    let Some(dir) = std::env::var_os("DSS7_RANGE_GOLDEN_DIR") else {
        eprintln!("DSS7_RANGE_GOLDEN_DIR not set; skipped");
        return;
    };
    let budget: usize = std::env::var("DSS7_RANGE_SCAN_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000_000);
    let files = dss7_files(Path::new(&dir));
    assert!(!files.is_empty(), "no DSS7 files in DSS7_RANGE_GOLDEN_DIR");
    for p in &files {
        let (mut f, _) = open7(p);
        let id = p.file_stem().unwrap().to_string_lossy().into_owned();
        for (k, cat) in catalogs(&mut f, &id).into_iter().enumerate() {
            let all = queries(&cat);
            let n = cat.catalog.entries.len().max(1);
            let max_q = (budget / n).max(50);
            let stride = all.len().div_ceil(max_q).max(1);
            let qs: Vec<Vec<u8>> = all.iter().step_by(stride).cloned().collect();
            let entries = cat.catalog.entries.len();
            let ix = IndexedDss7Catalog::new(cat);
            let with_range = compare(&ix, &qs, &format!("{id} catalog {k}"));
            eprintln!(
                "external {id} catalog={} entries={} queries={} compared={} with_range={} index_keys={}",
                ["default", "recover"][k],
                entries,
                all.len(),
                qs.len(),
                with_range,
                ix.index_len().unwrap_or(0)
            );
        }
    }
}

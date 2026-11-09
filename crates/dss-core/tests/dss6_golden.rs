//! DSS6 reader checks against real files and the Monolith oracle (see `docs/format/dss6.md`).
//!
//! These tests need files that are not in this repository, so they skip when the
//! variables are unset or the files are absent:
//!
//! - `DSS6_PUBLIC_DIR`: public HEC samples
//!   extracted read-only from the HEC-HMS sample zips. `00_castro.dss` is HMS 3.3
//!   `castro/castro.dss`.
//! - `DSS6_GOLDEN_EXPECT`: one TSV per
//!   reference file, exported from its HEC Monolith catalog. Header lines `#file\t<path>`, then
//!   `<monolith type>\t<pathname as hex>` per catalogued record.
//!
//! Output names fixtures by id only, never record pathnames.

use dss_core::dss6::{word_byte_offset, CatalogOptions, Dss6Error, Dss6File, InfoCheck};
use dss_core::AnomalyKind;
use std::path::{Path, PathBuf};

/// The directory a variable names. Unset, it is a path that does not exist, so the test skips.
fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("<{var} unset>")))
}

/// HMS 3.3 castro.dss address map reference values.
#[test]
fn castro_address_map_golden() {
    let path = env_dir("DSS6_PUBLIC_DIR").join("00_castro.dss");
    if !path.exists() {
        eprintln!("{} absent; skipping", path.display());
        return;
    }
    let f = Dss6File::open(&path).unwrap();
    let h = f.header();
    assert_eq!(h.first_bin_address, 382);
    assert_eq!(word_byte_offset(382), Some(1536));
    assert_eq!(h.file_size_words, 56008);
    assert_eq!(f.eof_word(), Some(-9630));
    assert_eq!(f.read_words(56008, 1).unwrap(), vec![-9630]);
    assert_eq!(h.expected_file_bytes(), Some(f.size_bytes()));
    assert_eq!(h.lock_record(), 3);
    assert_eq!(h.lock_byte_range(), (1024, 1536));
    assert_eq!(h.version_string, "6-ND");
    assert!(f.warnings().is_empty());
    let cat = f
        .catalog_with(&CatalogOptions {
            verify_info: InfoCheck::All,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(cat.live().count() as i32, h.number_records);
    assert!(cat.anomalies.is_empty(), "{:?}", cat.anomalies);
}

/// Every public sample: header, full catalog with all checks, and point lookups.
#[test]
fn public_samples_catalog_cleanly() {
    let dir = env_dir("DSS6_PUBLIC_DIR");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!("{} absent; skipping", dir.display());
        return;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| has_dss_ext(p))
        .collect();
    files.sort();
    for p in files {
        let f = Dss6File::open(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let walk = f
            .walk_bins(&CatalogOptions {
                verify_info: InfoCheck::All,
                ..Default::default()
            })
            .unwrap();
        let live = walk.live().count();
        assert_eq!(live as i32, f.header().number_records, "{}", p.display());
        assert!(
            walk.anomalies.is_empty(),
            "{}: {:?}",
            p.display(),
            walk.anomalies
        );
        for e in walk.live() {
            let r = f.lookup(&e.pathname).unwrap().expect("lookup");
            assert_eq!(r.entry.entry_address, e.entry_address);
        }
        eprintln!(
            "public {:<40} {} live={live} entries={} blocks={} overflow={}",
            p.file_name().unwrap().to_string_lossy(),
            f.header().version_string,
            walk.entries.len(),
            walk.blocks,
            walk.overflow.len()
        );
    }
}

fn has_dss_ext(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dss"))
}

struct Expect {
    id: String,
    file: PathBuf,
    records: Vec<(i32, Vec<u8>)>,
}

fn load_expect(path: &Path) -> Option<Expect> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut file = None;
    let mut records = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#file\t") {
            file = Some(PathBuf::from(rest));
        } else if !line.starts_with('#') && !line.is_empty() {
            let (t, hex) = line.split_once('\t')?;
            records.push((t.parse().ok()?, unhex(hex)?));
        }
    }
    Some(Expect {
        id: path.file_stem()?.to_string_lossy().into_owned(),
        file: file?,
        records,
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Catalog equality with Monolith for every DSS6 reference
/// file (public and private), plus negative fixtures that must fail cleanly.
#[test]
fn golden_catalogs_match_monolith() {
    let dir = env_dir("DSS6_GOLDEN_EXPECT");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!("{} absent; skipping", dir.display());
        return;
    };
    let mut tsvs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    tsvs.sort();
    let mut compared = 0;
    for t in tsvs {
        let Some(exp) = load_expect(&t) else { continue };
        if !exp.file.exists() {
            eprintln!("{}: fixture absent; skipped", exp.id);
            continue;
        }
        let opened = Dss6File::open(&exp.file);
        if exp.id.starts_with("neg_") || exp.id.contains("snodas") {
            let err = opened.expect_err(&exp.id);
            assert!(!matches!(err, Dss6Error::Io(_)), "{}: {err}", exp.id);
            eprintln!("golden {:<45} rejected: {err}", exp.id);
            continue;
        }
        let f = opened.unwrap_or_else(|e| panic!("{}: {e}", exp.id));
        let big = exp.records.len() > 20_000;
        let opts = CatalogOptions {
            verify_info: if big {
                InfoCheck::Sample(3000)
            } else {
                InfoCheck::All
            },
            ..Default::default()
        };
        let walk = f.walk_bins(&opts).unwrap();
        let mut ours: Vec<(i32, Vec<u8>)> = walk
            .live()
            .map(|e| (e.record_type, e.pathname.clone()))
            .collect();
        let mut theirs = exp.records.clone();
        ours.sort_by(|a, b| a.1.cmp(&b.1));
        theirs.sort_by(|a, b| a.1.cmp(&b.1));
        let our_paths: Vec<&Vec<u8>> = ours.iter().map(|r| &r.1).collect();
        let their_paths: Vec<&Vec<u8>> = theirs.iter().map(|r| &r.1).collect();
        assert_eq!(our_paths.len(), their_paths.len(), "{}: live count", exp.id);
        assert!(our_paths == their_paths, "{}: pathname sets differ", exp.id);
        // Monolith reports type 0 for the Latin-1 pathnames it cannot look up.
        let mut type_zero_nonascii = 0;
        for (o, m) in ours.iter().zip(&theirs) {
            if m.0 == 0 && o.1.iter().any(|&b| b >= 0x80) {
                type_zero_nonascii += 1;
                continue;
            }
            assert_eq!(o.0, m.0, "{}: record type", exp.id);
        }
        let kinds: Vec<AnomalyKind> = walk.anomalies.iter().map(|a| a.kind).collect();
        assert!(
            kinds.iter().all(|k| *k == AnomalyKind::RecordCountMismatch),
            "{}: unexpected anomalies {:?}",
            exp.id,
            walk.anomalies
                .iter()
                .map(|a| (a.kind, a.address))
                .collect::<Vec<_>>()
        );
        // Point lookups through the hash table (sampled on big files).
        let step = (walk.live().count() / 2000).max(1);
        let mut looked = 0;
        for e in walk.live().step_by(step) {
            let r = f
                .lookup(&e.pathname)
                .unwrap()
                .unwrap_or_else(|| panic!("{}: lookup failed", exp.id));
            assert_eq!(r.entry.entry_address, e.entry_address, "{}", exp.id);
            looked += 1;
        }
        compared += 1;
        eprintln!(
            "golden {:<45} {} live={} entries={} info_checked={} lookups={looked} nonascii_type0={type_zero_nonascii} anomalies={:?}",
            exp.id,
            f.header().version_string,
            ours.len(),
            walk.entries.len(),
            walk.info_checked,
            kinds
        );
    }
    eprintln!("compared {compared} DSS6 golden catalogs");
}

//! DSS7 catalog walk over real files listed in a corpus manifest (P06).
//!
//! Set `DSS_CORPUS_LIST` to a text file with one DSS path per line (DSS6 lines are
//! skipped). Without it the test is a no-op. Every DSS7 file must open and catalog
//! without panicking; damage must surface as a typed error or as anomalies, and the
//! file's size and mtime must not change. Use the harness in
//! `examples/dss7_corpus.rs` for per-file numbers.

use dss_core::dss7::{CatalogOptions, Dss7File, InfoCheck};
use std::fs::File;
use std::io::Read;

#[test]
fn corpus_dss7_catalogs() {
    let Ok(list) = std::env::var("DSS_CORPUS_LIST") else {
        eprintln!("DSS_CORPUS_LIST not set; skipping DSS7 corpus catalog test");
        return;
    };
    let text = std::fs::read_to_string(list).expect("read corpus list");
    let opts = CatalogOptions {
        check_info: InfoCheck::LiveUpTo(3000),
        ..CatalogOptions::default()
    };
    let (mut files, mut live, mut anomalies, mut errors) = (0u64, 0u64, 0u64, Vec::new());
    for path in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut head = [0u8; 20];
        let is_v7 = File::open(path)
            .and_then(|mut f| f.read_exact(&mut head))
            .is_ok()
            && &head[0..4] == b"ZDSS"
            && head[16] == b'7';
        if !is_v7 {
            continue;
        }
        let before = std::fs::metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        match Dss7File::open(path).and_then(|mut f| f.catalog_with(&opts)) {
            Ok(cat) => {
                live += cat.stats.live;
                anomalies += cat.catalog.anomalies.len() as u64;
            }
            Err(e) => errors.push(format!("{path}: {e}")),
        }
        let after = std::fs::metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        assert_eq!(before, after, "{path} changed while reading");
        files += 1;
    }
    eprintln!(
        "catalogued {files} DSS7 files: {live} live entries, {anomalies} anomalies, {} errors",
        errors.len()
    );
    for e in &errors {
        eprintln!("  {e}");
    }
}

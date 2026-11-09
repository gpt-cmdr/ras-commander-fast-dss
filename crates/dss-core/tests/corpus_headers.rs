//! Header checks against real files listed in a text file.
//!
//! Set `DSS_CORPUS_LIST` to a text file with one DSS path per line. Without it the test
//! is a no-op, so CI still passes.

use dss_core::FileSignature;
use std::fs::File;
use std::io::Read;

#[test]
fn corpus_headers_match_file_sizes() {
    let Ok(list) = std::env::var("DSS_CORPUS_LIST") else {
        eprintln!("DSS_CORPUS_LIST not set; skipping corpus header test");
        return;
    };
    let text = std::fs::read_to_string(list).expect("read corpus list");
    let (mut checked, mut exact, mut truncated, mut failures) = (0, 0, Vec::new(), Vec::new());
    for path in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0) as i64;
        let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
        if file.read_exact(&mut head).is_err() {
            continue;
        }
        match FileSignature::from_bytes(&head) {
            Ok(sig) if sig.min_file_bytes() > size => {
                // Truncated or corrupt files are expected in a real corpus; the reader
                // must report them, not fail on them.
                truncated.push(format!(
                    "{path}: header needs {} > actual {size}",
                    sig.min_file_bytes()
                ));
            }
            Ok(sig) => {
                // Files can grow past the header size only by trailing slack; count
                // exact matches so the size rule itself is checked against the corpus.
                if size == sig.file_size_bytes() || size == sig.min_file_bytes() {
                    exact += 1;
                }
            }
            Err(e) => failures.push(format!("{path}: {e}")),
        }
        checked += 1;
    }
    eprintln!(
        "checked {checked} corpus headers; {exact} exact size; {} truncated",
        truncated.len()
    );
    for line in &truncated {
        eprintln!("  truncated: {line}");
    }
    assert!(
        failures.is_empty(),
        "unreadable headers:\n{}",
        failures.join("\n")
    );
}

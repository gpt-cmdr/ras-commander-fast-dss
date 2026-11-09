//! DSS7 header parse with typed errors (dss7.md §2, §9 step 1–2).

mod dss7_common;

use dss7_common::{build, get_word, set_word, standard_spec};
use dss_core::dss7::{Dss7Error, Dss7File};

fn base() -> Vec<u8> {
    build(&standard_spec()).bytes
}

fn open(bytes: &[u8]) -> Result<Dss7File<std::io::Cursor<&[u8]>>, Dss7Error> {
    Dss7File::from_bytes(bytes)
}

#[test]
fn synthetic_header_parses() {
    let b = base();
    let f = open(&b).unwrap();
    let h = f.header();
    assert_eq!(h.version_string, "7-IU");
    assert_eq!(h.fver, 70921);
    assert_eq!(h.hash_table_address, 178);
    assert_eq!(h.max_hash, 64);
    assert_eq!(h.bin_size, 40);
    assert!(!h.old_locking);
    assert_eq!(h.lock_area_end, 178);
    assert_eq!(h.file_size_words as usize * 8 + 8, b.len());
}

#[test]
fn header_size_zero_is_accepted_as_100() {
    let mut b = base();
    set_word(&mut b, 1, 0);
    let f = open(&b).unwrap();
    assert_eq!(f.header().header_size, 100);
    assert!(f.header().header_size_was_zero);
}

#[test]
fn bad_signature() {
    let mut b = base();
    b[0] = b'X';
    assert!(matches!(open(&b), Err(Dss7Error::BadSignature)));
}

#[test]
fn version_not_7() {
    let mut b = base();
    b[16] = b'6';
    assert!(matches!(open(&b), Err(Dss7Error::NotVersion7 { version }) if version[0] == b'6'));
    b[16] = b'8';
    assert!(matches!(open(&b), Err(Dss7Error::NotVersion7 { .. })));
}

#[test]
fn bad_end_header_flag() {
    let mut b = base();
    set_word(&mut b, 99, 0);
    assert!(matches!(open(&b), Err(Dss7Error::BadEndHeaderFlag(0))));
}

#[test]
fn detune_rejected() {
    let mut b = base();
    set_word(&mut b, 89, 1);
    assert!(matches!(open(&b), Err(Dss7Error::DetuneSet(1))));
}

#[test]
fn bin_size_out_of_range() {
    for v in [19i64, 1001, 0, -5] {
        let mut b = base();
        set_word(&mut b, 23, v);
        assert!(
            matches!(open(&b), Err(Dss7Error::BadBinSize(x)) if x == v),
            "bin size {v}"
        );
    }
}

#[test]
fn bins_per_block_and_max_hash_out_of_range() {
    let mut b = base();
    set_word(&mut b, 21, 0);
    assert!(matches!(open(&b), Err(Dss7Error::BadBinsPerBlock(0))));
    let mut b = base();
    set_word(&mut b, 15, 0);
    assert!(matches!(open(&b), Err(Dss7Error::BadMaxHash(0))));
}

#[test]
fn pointers_outside_data_area() {
    let eof = get_word(&base(), 5);
    // Hash table inside the lock area / header, or past EOF.
    for (word, v) in [
        (19usize, 99i64),
        (19, 150),
        (19, eof),
        (24, 50),
        (24, eof),
        (24, eof + 10),
    ] {
        let mut b = base();
        set_word(&mut b, word, v);
        assert!(
            matches!(open(&b), Err(Dss7Error::PointerOutOfRange { .. })),
            "word {word} = {v}"
        );
    }
    // Hash table that runs past EOF.
    let mut b = base();
    set_word(&mut b, 15, eof);
    assert!(matches!(open(&b), Err(Dss7Error::PointerOutOfRange { .. })));
}

#[test]
fn truncated_file() {
    let b = base();
    let cut = &b[..b.len() - 8];
    assert!(matches!(open(cut), Err(Dss7Error::Truncated { .. })));
    assert!(matches!(open(&b[..799]), Err(Dss7Error::TooShort { .. })));
    assert!(matches!(open(&b[..10]), Err(Dss7Error::TooShort { .. })));
    assert!(matches!(open(&[]), Err(Dss7Error::TooShort { .. })));
}

#[test]
fn bad_header_size_word() {
    let mut b = base();
    set_word(&mut b, 1, -3);
    assert!(matches!(open(&b), Err(Dss7Error::BadHeaderSize(-3))));
}

#[test]
fn public_headers_match_p04_golden_offsets() {
    // Acceptance check 2 (header part). Skips when the public fixtures are absent.
    if let Some(p) = dss7_common::bald_eagle() {
        let f = Dss7File::open(&p).unwrap();
        let h = f.header();
        assert_eq!(h.version_string, "7-HL");
        assert_eq!(
            (h.hash_table_address, h.max_hash, h.bin_size),
            (178, 8192, 200)
        );
        assert_eq!(h.first_bin_address, 9372);
        assert_eq!(h.number_records, 102);
        assert_eq!(h.file_size_words, 92939);
    } else {
        eprintln!("BaldEagleDamBrk.dss not available; skipped");
    }
    if let Some(p) = dss7_common::sample7() {
        let f = Dss7File::open(&p).unwrap();
        let h = f.header();
        assert_eq!(h.version_string, "7-10");
        assert!(h.header_size_was_zero);
        assert_eq!(h.header_size, 100);
        assert!(h.old_locking);
        assert_eq!(
            (h.hash_table_address, h.max_hash, h.bin_size),
            (110, 4096, 100)
        );
        assert_eq!(h.number_records, 594);
        assert!(h.fver < 70801);
    } else {
        eprintln!("sample7.dss not available; skipped");
    }
}

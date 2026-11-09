//! DSS7 location supplemental text stays Latin-1.
//!
//! The engine decodes DSS6 user-header text as UTF-8 (docs/format/dss6.md §14.2); DSS7 location
//! records keep the Latin-1 decode. The record here stores `C3 BD` in values2, which is
//! valid UTF-8 for `ý` but must read as the two Latin-1 characters `Ã½`.
//!
//! `fixtures/public/synthetic_location_latin1_7.dss` is generated here (no HEC software;
//! regenerate with `DSS_PY_FIXTURE_OUT=<dir>`); the test fails when the committed bytes
//! differ from the generator. All names and values are invented. The location layout is
//! `Public/zlocationStore.c:zlocationStore()`, as in `python_regression_fixtures.rs`.

mod dss7_common;

use dss7_common::{build, set_word, Rec, Spec};
use dss_core::dss7::{info::key, Dss7File};
use dss_core::location::SupplementalSource;
use std::path::PathBuf;

const PATH: &str = "/P28 LOC/GAGE L7/Location Info////";
/// values2: valid UTF-8 bytes (`C3 BD`) plus a lone `E9`, NUL-padded to whole words.
const V2: &[u8] = b"Note:Y\xc3\xbd;Owner:Caf\xe9;\0\0\0\0";

fn file() -> Vec<u8> {
    let built = build(&Spec {
        recs: vec![Rec::live(PATH, 20)],
        ..Default::default()
    });
    let mut bytes = built.bytes;
    let info = built.info_addr[0] as usize;
    let eof = bytes.len() / 8 - 1; // replace the old EOF with the payload
    bytes.truncate(eof * 8);
    // values1 = x, y, z doubles, six codes, time-zone name.
    let mut v1: Vec<u8> = Vec::new();
    for v in [1.5f64, 2.5, 3.5] {
        v1.extend_from_slice(&v.to_le_bytes());
    }
    for c in [1i32, 0, 3, 1, 1, 1] {
        v1.extend_from_slice(&c.to_le_bytes());
    }
    v1.extend_from_slice(b"UTC\0");
    let v1_ints = v1.len() / 4;
    let v2_ints = V2.len() / 4;
    let v1_addr = bytes.len() / 8;
    bytes.extend_from_slice(&v1);
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let v2_addr = bytes.len() / 8;
    bytes.extend_from_slice(V2);
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(&mut bytes, info + key::VALUES1_ADDRESS, v1_addr as i64);
    set_word(&mut bytes, info + key::VALUES1_NUMBER, v1_ints as i64);
    set_word(&mut bytes, info + key::VALUES2_ADDRESS, v2_addr as i64);
    set_word(&mut bytes, info + key::VALUES2_NUMBER, v2_ints as i64);
    set_word(&mut bytes, info + key::NUMBER_DATA, v1_ints as i64);
    set_word(
        &mut bytes,
        info + key::ALLOCATED_SIZE,
        (v1_ints + v2_ints) as i64,
    );
    set_word(&mut bytes, 5, eof as i64);
    bytes
}

#[test]
fn dss7_supplemental_is_latin1() {
    let bytes = file();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let loc = f.read_location(PATH.as_bytes()).unwrap().location;
    assert_eq!(
        loc.supplemental_source(),
        Some(SupplementalSource::Dss7LocationRecord)
    );
    assert_eq!(
        loc.supplemental.as_deref(),
        Some(&b"Note:Y\xc3\xbd;Owner:Caf\xe9;"[..])
    );
    // Latin-1, not UTF-8: `C3 BD` is two characters.
    assert_eq!(
        loc.supplemental_text().as_deref(),
        Some("Note:Y\u{c3}\u{bd};Owner:Caf\u{e9};")
    );
    assert_eq!(loc.supplemental_encoding(), Some("latin-1"));
}

#[test]
fn committed_fixture_matches_generator() {
    let bytes = file();
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_location_latin1_7.dss"), &bytes).unwrap();
    }
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_location_latin1_7.dss");
    let on_disk = std::fs::read(&committed).expect("public fixture missing");
    assert_eq!(
        on_disk, bytes,
        "committed fixture differs from the generator"
    );
}

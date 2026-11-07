//! DSS6 user-header text decoding (packet P28).
//!
//! Maintainer decision (2026-10-28): DSS6 user-header text is "probably UTF-8". The text
//! bytes, after the NUL cut and the trailing-blank trim (P26), are decoded as UTF-8 when
//! they are valid UTF-8 (else Java modified UTF-8); otherwise, since P31, as UTF-8 with
//! each malformed sequence replaced by one U+FFFD as Java does (P28 used whole-text
//! Latin-1; see `dss6_user_header_lossy.rs` for the P31 cases). Monolith reads
//! the same bytes through JNI `NewStringUTF` (`javaHeclib/src/Hec_ztsRetrieve.c`, "User
//! header (supplemental)"). The DSS6 location supplemental text (P27) uses the same
//! decode. Raw bytes are unchanged.
//!
//! `fixtures/public/synthetic_userheader_utf8_6.dss` is generated here (no HEC software;
//! regenerate with `DSS_PY_FIXTURE_OUT=<dir>`); the test fails when the committed bytes
//! differ from the generator. All names and values are invented.

mod dss6_ts_support;

use dss6_ts_support::{f32w, f64w, its_header, jul, rts_header, text, ts_file, Payload, Rec};
use dss_core::dss6::{DatasetOptions, Dss6File};
use dss_core::location::Location;
use dss_core::user_header::{
    parse_user_header_items, user_header_bytes, user_header_text, user_header_text_encoding,
    UserHeaderEncoding,
};
use std::path::PathBuf;

/// Valid UTF-8: two-byte `ý` (`C3 BD`, the g54 byte pair) and three-byte `€`.
const P_UTF8: &str = "/P28/GAGE U/FLOW/01JAN2000/1HOUR/OBS/";
const B_UTF8: &[u8] = b"Gage: Y\xc3\xbd;VerticalDatum: NAVD88;Note: \xe2\x82\xac 5";
/// Invalid UTF-8 (lone `E9`): one U+FFFD, the valid `C3 BD` keeps its character (P31).
const P_LATIN1: &str = "/P28/GAGE L/FLOW/01JAN2000/1HOUR/OBS/";
const B_LATIN1: &[u8] = b"Gage: Caf\xe9;Note: Y\xc3\xbd";
/// Pure ASCII.
const P_ASCII: &str = "/P28/GAGE A/FLOW/01JAN2000/1HOUR/OBS/";
const B_ASCII: &[u8] = b"Gage: plain;Note: ascii";
/// `C3 BD` split by a NUL: the text is `Gage: Y` + `C3`, which is one U+FFFD (P31).
const P_SPLIT_NUL: &str = "/P28/GAGE N/FLOW/01JAN2000/1HOUR/OBS/";
const B_SPLIT_NUL: &[u8] = b"Gage: Y\xc3\x00\xbd;Hidden: 1";
/// `C3` as the last stored byte (two whole words, no NUL, no padding): one U+FFFD.
const P_SPLIT_END: &str = "/P28/GAGE E/FLOW/01JAN2000/1HOUR/OBS/";
const B_SPLIT_END: &[u8] = b"Gage: Y\xc3";
/// `E2 82` (two bytes of `€`) then blank padding: the trim leaves an incomplete
/// sequence, one U+FFFD.
const P_SPLIT_TRIM: &str = "/P28/GAGE T/FLOW/01JAN2000/1HOUR/OBS/";
const B_SPLIT_TRIM: &[u8] = b"Gage: x\xe2\x82    ";
/// Valid UTF-8 before the NUL, invalid bytes after it: UTF-8 (the junk is not text).
const P_AFTER_NUL: &str = "/P28/GAGE V/FLOW/01JAN2000/1HOUR/OBS/";
const B_AFTER_NUL: &[u8] = b"Gage: Y\xc3\xbd\x00\xe9\xe9\xe9";
/// Irregular record with a NUL-terminated UTF-8 header (`é` = `C3 A9`).
const P_IRR: &str = "/P28/GAGE I/STAGE/01JAN2000/IR-MONTH/OBS/";
const B_IRR: &[u8] = b"Gage: \xc3\xa9t\xc3\xa9\x00";
/// Regular dataset of two blocks: JAN valid UTF-8, FEB invalid; FEB is reported.
const P_D_JAN: &str = "/P28/GAGE D/FLOW/01JAN2000/1HOUR/OBS/";
const P_D_FEB: &str = "/P28/GAGE D/FLOW/01FEB2000/1HOUR/OBS/";
const B_D_JAN: &[u8] = b"Gage: D \xc3\xa9";
const B_D_FEB: &[u8] = b"Gage: D \xe9";
/// Java modified UTF-8 (review m5): U+1F30A as the surrogate pair D83C DF0A, each as a
/// 3-byte sequence (how JNI writes a character outside the BMP), NUL-terminated.
const P_MUTF8_PAIR: &str = "/P28/GAGE P/FLOW/01JAN2000/1HOUR/OBS/";
const B_MUTF8_PAIR: &[u8] = b"Note: wave \xed\xa0\xbc\xed\xbc\x8a\x00";
/// Java modified UTF-8 U+0000 (`C0 80`), which is not a NUL byte, so the text goes on.
const P_MUTF8_NUL: &str = "/P28/GAGE Z/FLOW/01JAN2000/1HOUR/OBS/";
const B_MUTF8_NUL: &[u8] = b"Gage: Z\xc0\x80;Note: after";

const XYZ: [f64; 3] = [-90.5, 30.25, 12.0];
/// Lat/long, decimal degrees, NAD83, feet, vertical datum 0 (unset).
const DESC: [i32; 6] = [1, 0, 3, 1, 1, 0];

/// Bytes to words, blank-padded to a whole word (Fortran `HOLCHR` fill).
fn words(b: &[u8]) -> Vec<i32> {
    let mut b = b.to_vec();
    while b.len() % 4 != 0 {
        b.push(b' ');
    }
    b.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Coordinates and descriptors at word offset `c0` (0-based) of an internal header:
/// regular words 13-24 (`zsrtsi6.f` lines 878-887), irregular 17-28 (`zritsi6.f`
/// lines 737-749).
fn put_location(h: &mut Vec<i32>, c0: usize) {
    if h.len() < c0 + 12 {
        h.resize(c0 + 12, 0);
    }
    for (i, v) in XYZ.iter().enumerate() {
        let w = f64w(*v);
        h[c0 + 2 * i] = w[0];
        h[c0 + 2 * i + 1] = w[1];
    }
    h[c0 + 6..c0 + 12].copy_from_slice(&DESC);
}

fn regular(f: &mut dss6_ts_support::TsFile, path: &str, uh: &[u8], n: i32) {
    let mut h = rts_header(0, "CFS", "INST-VAL", 24);
    put_location(&mut h, 12);
    f.attach(
        path,
        &Payload {
            internal_header: h,
            user_header: words(uh),
            data: (0..n).map(|i| f32w(i as f32)).collect(),
            kilnda: n,
            ..Default::default()
        },
    );
}

fn file() -> Vec<u8> {
    let recs = vec![
        Rec::live(P_UTF8, 100),
        Rec::live(P_LATIN1, 100),
        Rec::live(P_ASCII, 100),
        Rec::live(P_SPLIT_NUL, 100),
        Rec::live(P_SPLIT_END, 100),
        Rec::live(P_SPLIT_TRIM, 100),
        Rec::live(P_AFTER_NUL, 100),
        Rec::live(P_IRR, 110),
        Rec::live(P_D_JAN, 100),
        Rec::live(P_D_FEB, 100),
        Rec::live(P_MUTF8_PAIR, 100),
        Rec::live(P_MUTF8_NUL, 100),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    for (p, b) in [
        (P_UTF8, B_UTF8),
        (P_LATIN1, B_LATIN1),
        (P_ASCII, B_ASCII),
        (P_SPLIT_NUL, B_SPLIT_NUL),
        (P_SPLIT_END, B_SPLIT_END),
        (P_SPLIT_TRIM, B_SPLIT_TRIM),
        (P_AFTER_NUL, B_AFTER_NUL),
        (P_D_JAN, B_D_JAN),
        (P_MUTF8_PAIR, B_MUTF8_PAIR),
        (P_MUTF8_NUL, B_MUTF8_NUL),
    ] {
        regular(&mut f, p, b, 744);
    }
    regular(&mut f, P_D_FEB, B_D_FEB, 696);
    let j = jul(2000, 1, 1) as i32;
    let mut ih = its_header(2, 2, j, 0, "FEET", "INST-VAL");
    ih[3] = 120; // time of the last value (zritsi6.f lines 724-746)
    ih.extend(text("", 6));
    put_location(&mut ih, 16);
    f.attach(
        P_IRR,
        &Payload {
            internal_header: ih,
            user_header: words(B_IRR),
            data: vec![60, f32w(1.0), 120, f32w(2.0)],
            kilnda: 2,
            ..Default::default()
        },
    );
    f.finish()
}

/// (pathname, expected text, expected encoding) for every single record.
fn cases() -> Vec<(&'static str, String, UserHeaderEncoding)> {
    use UserHeaderEncoding::{ModifiedUtf8, Utf8, Utf8Replaced};
    vec![
        (
            P_UTF8,
            "Gage: Y\u{fd};VerticalDatum: NAVD88;Note: \u{20ac} 5".into(),
            Utf8,
        ),
        (
            P_LATIN1,
            "Gage: Caf\u{fffd};Note: Y\u{fd}".into(),
            Utf8Replaced,
        ),
        (P_ASCII, "Gage: plain;Note: ascii".into(), Utf8),
        (P_SPLIT_NUL, "Gage: Y\u{fffd}".into(), Utf8Replaced),
        (P_SPLIT_END, "Gage: Y\u{fffd}".into(), Utf8Replaced),
        (P_SPLIT_TRIM, "Gage: x\u{fffd}".into(), Utf8Replaced),
        (P_AFTER_NUL, "Gage: Y\u{fd}".into(), Utf8),
        (P_IRR, "Gage: \u{e9}t\u{e9}".into(), Utf8),
        (P_D_JAN, "Gage: D \u{e9}".into(), Utf8),
        (P_D_FEB, "Gage: D \u{fffd}".into(), Utf8Replaced),
        (P_MUTF8_PAIR, "Note: wave \u{1f30a}".into(), ModifiedUtf8),
        (P_MUTF8_NUL, "Gage: Z\u{0};Note: after".into(), ModifiedUtf8),
    ]
}

#[test]
fn record_text_and_encoding() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    for (p, want, enc) in cases() {
        let r = f.read_ts_record(p.as_bytes()).unwrap();
        assert_eq!(r.user_header_text(), want, "{p}");
        assert_eq!(r.user_header_encoding(), Some(enc), "{p}");
        assert_eq!(user_header_text_encoding(&r.user_header), Some(enc), "{p}");
    }
    // Raw bytes are kept unchanged, including the bytes after a NUL.
    let r = f.read_ts_record(P_SPLIT_NUL.as_bytes()).unwrap();
    assert_eq!(&user_header_bytes(&r.user_header)[..], B_SPLIT_NUL);
    let r = f.read_ts_record(P_SPLIT_END.as_bytes()).unwrap();
    assert_eq!(r.user_header.len(), 2);
    assert_eq!(&user_header_bytes(&r.user_header)[..], B_SPLIT_END);
    // Items come from the decoded text.
    let r = f.read_ts_record(P_UTF8.as_bytes()).unwrap();
    assert_eq!(
        parse_user_header_items(&r.user_header_text()),
        vec![
            ("Gage".to_string(), " Y\u{fd}".to_string()),
            ("VerticalDatum".to_string(), " NAVD88".to_string()),
            ("Note".to_string(), " \u{20ac} 5".to_string()),
        ]
    );
}

#[test]
fn location_supplemental_uses_the_same_decode() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    for (p, want, enc) in cases() {
        let r = f.read_ts_record(p.as_bytes()).unwrap();
        let l = Location::from_dss6_ts(&r.pathname, r.coordinates, r.coordinate_descriptors)
            .unwrap()
            .with_dss6_user_header(&r.user_header);
        assert_eq!(l.supplemental_text().as_deref(), Some(want.as_str()), "{p}");
        assert_eq!(l.supplemental_encoding(), Some(enc.as_str()), "{p}");
        assert_eq!(l.supplemental_text(), Some(r.user_header_text()), "{p}");
    }
}

/// Conflicts between blocks are still counted on bytes (P26); the reported header is
/// decoded on its own.
#[test]
fn dataset_reports_last_block_decoded_alone() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let d = f
        .read_ts_dataset(b"/P28/GAGE D/FLOW//1HOUR/OBS/", &DatasetOptions::whole())
        .unwrap();
    assert_eq!(
        d.report.user_header_block.as_deref(),
        Some(P_D_FEB.as_bytes())
    );
    assert_eq!(user_header_text(&d.report.user_header), "Gage: D \u{fffd}");
    assert_eq!(
        user_header_text_encoding(&d.report.user_header),
        Some(UserHeaderEncoding::Utf8Replaced)
    );
    assert_eq!(d.report.user_header_conflicts, 1);
}

#[test]
fn committed_fixture_matches_generator() {
    let bytes = file();
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_userheader_utf8_6.dss"), &bytes).unwrap();
    }
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_userheader_utf8_6.dss");
    let on_disk = std::fs::read(&committed).expect("public fixture missing");
    assert_eq!(
        on_disk, bytes,
        "committed fixture differs from the generator"
    );
}

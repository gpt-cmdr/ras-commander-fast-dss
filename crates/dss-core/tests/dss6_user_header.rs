//! DSS6 time-series user headers (packet P26) on `fixtures/public/synthetic_userheader6.dss`,
//! written by HEC Monolith 3.3.27 (`evidence/P26/scripts/UhMonolith.java`).
//! Expected texts are what Monolith returns for the same reads (P26 REPORT §4).

use dss_core::dss6::{DatasetOptions, Dss6File, TimeWindow};
use dss_core::user_header::{
    parse_user_header_items, user_header_bytes, user_header_text, user_header_unkeyed_segments,
};
use std::path::PathBuf;

fn open() -> Dss6File<dss_core::dss6::FileSource> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_userheader6.dss");
    assert!(p.exists(), "public fixture missing: {}", p.display());
    Dss6File::open(p).expect("open fixture")
}

const GAGE_A: &str = "State: Example State;StreamSample Creek;DrainageArea: 12.50;\
Operator: USGS;GageDatum: 10.25;VerticalDatum: NAVD88;County: Sample;\
LocationSample Bridge, EX;DataLocation: Sample Bridge, EX;USGS No: 00000001;HUC: 00000000";

/// HEC seconds since 1899-12-31 00:00 of a calendar date (proleptic Gregorian).
fn secs(y: i64, m: i64, d: i64, hh: i64, mm: i64) -> i64 {
    // Days from 1899-12-31 via a civil-from-days inverse.
    let days = |y: i64, m: i64, d: i64| {
        let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
        365 * y + y / 4 - y / 100 + y / 400 + (153 * (m - 3) + 2) / 5 + d
    };
    (days(y, m, d) - days(1899, 12, 31)) * 86_400 + hh * 3600 + mm * 60
}

#[test]
fn record_header_words_text_and_items() {
    let f = open();
    let r = f
        .read_ts_record(b"/P26/GAGE A/FLOW/01JAN2020/1HOUR/SYNTH/")
        .unwrap();
    assert_eq!(r.user_header.len(), 55);
    let bytes = r.user_header_bytes();
    assert_eq!(bytes.len(), 220);
    assert!(!bytes.contains(&0), "DSS6 pads with blanks, no NUL");
    assert_eq!(r.user_header_text(), GAGE_A);
    let items = r.user_header_items();
    assert_eq!(items.len(), 9);
    assert_eq!(
        items[0],
        ("State".to_string(), " Example State".to_string())
    );
    assert!(items.iter().all(|(k, _)| k != "Stream" && k != "Location"));
    assert_eq!(
        user_header_unkeyed_segments(&r.user_header_text()),
        vec!["StreamSample Creek", "LocationSample Bridge, EX"]
    );
}

#[test]
fn whole_word_header_without_nul() {
    let f = open();
    let r = f
        .read_ts_record(b"/P26/GAGE C/FLOW/01JAN2020/1HOUR/SYNTH/")
        .unwrap();
    assert_eq!(r.user_header.len(), 8);
    assert_eq!(r.user_header_text(), "Block: january;Operator: Example");
}

#[test]
fn irregular_record_and_empty_header() {
    let f = open();
    let r = f
        .read_ts_record(b"/P26/GAGE D/STAGE/01JAN2020/IR-MONTH/SYNTH/")
        .unwrap();
    assert_eq!(r.record_type, 110);
    assert_eq!(r.user_header_text(), "Block: january;VerticalDatum: NAVD88");
    let r = f
        .read_ts_record(b"/P26/GAGE F/FLOW/01JAN2020/1DAY/SYNTH/")
        .unwrap();
    assert!(r.user_header.is_empty());
    assert_eq!(r.user_header_text(), "");
    assert!(r.user_header_items().is_empty());
}

#[test]
fn parser_matches_monolith_on_edge_cases() {
    let f = open();
    let r = f
        .read_ts_record(b"/P26/GAGE E/FLOW/01JAN2020/1DAY/SYNTH/")
        .unwrap();
    let t = r.user_header_text();
    assert_eq!(
        t,
        "Key: first;Key: second;Time: 12:00;;  Lead: x  ;NoColonHere"
    );
    assert_eq!(
        parse_user_header_items(&t),
        vec![
            ("Key".to_string(), " first".to_string()),
            ("Key".to_string(), " second".to_string()),
            ("  Lead".to_string(), " x".to_string()),
        ]
    );
}

#[test]
fn non_ascii_bytes_decode_utf8() {
    // P28: Monolith stored its `Café` as `43 61 66 C3 83 C2 A9`, valid UTF-8 for "CafÃ©",
    // which is what Monolith reads back (JNI `NewStringUTF`). Before P28 the engine
    // decoded Latin-1 ("Caf\u{c3}\u{83}\u{c2}\u{a9}").
    let f = open();
    let r = f
        .read_ts_record(b"/P26/GAGE G/FLOW/01JAN2020/1DAY/SYNTH/")
        .unwrap();
    assert_eq!(
        &user_header_bytes(&r.user_header)[..],
        &b"Note: Caf\xc3\x83\xc2\xa9   "[..]
    );
    assert_eq!(user_header_text(&r.user_header), "Note: Caf\u{c3}\u{a9}");
    assert_eq!(
        r.user_header_encoding(),
        Some(dss_core::user_header::UserHeaderEncoding::Utf8)
    );
    assert_eq!(user_header_bytes(&r.user_header).len(), 16);
}

#[test]
fn regular_dataset_reports_last_block() {
    let f = open();
    let d = f
        .read_ts_dataset(b"/P26/GAGE C/FLOW//1HOUR/SYNTH/", &DatasetOptions::whole())
        .unwrap();
    assert_eq!(
        user_header_text(&d.report.user_header),
        "Block: february;Operator: Example"
    );
    assert_eq!(
        d.report.user_header_block.as_deref(),
        Some(&b"/P26/GAGE C/FLOW/01FEB2020/1HOUR/SYNTH/"[..])
    );
    assert_eq!(d.report.user_header_conflicts, 1);
    let d = f
        .read_ts_dataset(b"/P26/GAGE A/FLOW//1HOUR/SYNTH/", &DatasetOptions::whole())
        .unwrap();
    assert_eq!(user_header_text(&d.report.user_header), GAGE_A);
    assert_eq!(d.report.user_header_conflicts, 0);
}

#[test]
fn irregular_window_ending_in_absent_block_has_no_header() {
    let f = open();
    let p = b"/P26/GAGE D/STAGE//IR-MONTH/SYNTH/";
    let d = f.read_ts_dataset(p, &DatasetOptions::whole()).unwrap();
    assert_eq!(
        user_header_text(&d.report.user_header),
        "Block: march;VerticalDatum: NAVD88"
    );
    assert_eq!(d.report.user_header_conflicts, 1);
    let opts = DatasetOptions {
        window: Some(TimeWindow::new(
            secs(2020, 1, 9, 0, 0),
            secs(2020, 2, 15, 23, 59),
        )),
        ..DatasetOptions::whole()
    };
    let d = f.read_ts_dataset(p, &opts).unwrap();
    assert_eq!(d.series.times.len(), 2);
    assert!(d.report.user_header.is_empty());
    assert!(d.report.user_header_block.is_none());
    assert_eq!(d.report.user_header_conflicts, 1);
}

#[test]
fn regular_window_ending_in_absent_block_keeps_last_found() {
    // Unlike irregular data (zreadx6 NUHEAD = 0), zrrtsi.f lines 345-349 leave IUHEAD
    // alone for an absent block, so a regular window running into the missing March
    // block still reports February. Monolith agrees (P26 REPORT §4, request w5).
    let f = open();
    let opts = DatasetOptions {
        window: Some(TimeWindow::new(
            secs(2020, 1, 31, 22, 0),
            secs(2020, 3, 5, 23, 59),
        )),
        ..DatasetOptions::whole()
    };
    let d = f
        .read_ts_dataset(b"/P26/GAGE C/FLOW//1HOUR/SYNTH/", &opts)
        .unwrap();
    assert!(d.report.blocks_missing >= 1);
    assert_eq!(
        user_header_text(&d.report.user_header),
        "Block: february;Operator: Example"
    );
    assert_eq!(
        d.report.user_header_block.as_deref(),
        Some(&b"/P26/GAGE C/FLOW/01FEB2020/1HOUR/SYNTH/"[..])
    );
    assert_eq!(d.report.user_header_conflicts, 1);
}

// --- NUL-terminated user header on a synthetic DSS6 file (dss6_ts_support) ------------

mod dss6_ts_support;

const NUL_PATH: &str = "/P26/NUL/FLOW/01JAN2000/1HOUR/SYN/";
const NUL_IRR: &str = "/P26/NUL/STAGE/01JAN2000/IR-MONTH/SYN/";

/// Header bytes: text, a NUL, then junk that must not reach the text, zero-filled to
/// whole words (the layout `stringToUserHeader` produces, `Utilities/verticalDatum.c`).
fn nul_header() -> Vec<i32> {
    let mut b = b"Gage: N1;VerticalDatum: NGVD29;\0junk;Hidden: 1".to_vec();
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn nul_file() -> Vec<u8> {
    use dss6_ts_support::{f32w, its_header, rts_header, ts_file, Payload, Rec};
    let recs = vec![Rec::live(NUL_PATH, 100), Rec::live(NUL_IRR, 110)];
    let mut f = ts_file(&recs, *b"6-YN");
    f.attach(
        NUL_PATH,
        &Payload {
            internal_header: rts_header(0, "CFS", "INST-VAL", 24),
            user_header: nul_header(),
            data: (0..744).map(|i| f32w(i as f32)).collect(),
            kilnda: 744,
            ..Default::default()
        },
    );
    // Two (time minutes from block start, value) pairs.
    let jul = dss6_ts_support::jul(2000, 1, 1) as i32;
    let mut ih = its_header(2, 2, jul, 0, "FEET", "INST-VAL");
    ih[3] = 120; // time of the last value (zritsi6.f lines 724-746)
    f.attach(
        NUL_IRR,
        &Payload {
            internal_header: ih,
            user_header: nul_header(),
            data: vec![60, f32w(1.0), 120, f32w(2.0)],
            kilnda: 2,
            ..Default::default()
        },
    );
    f.finish()
}

#[test]
fn nul_terminated_header_on_synthetic_file() {
    let bytes = nul_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    for p in [NUL_PATH, NUL_IRR] {
        let r = f.read_ts_record(p.as_bytes()).unwrap();
        assert_eq!(r.user_header, nul_header(), "{p}: raw words kept");
        assert!(r.user_header_bytes().contains(&0));
        assert_eq!(r.user_header_text(), "Gage: N1;VerticalDatum: NGVD29;");
        let items = r.user_header_items();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|(k, _)| k != "Hidden"));
    }
    let d = f
        .read_ts_dataset(b"/P26/NUL/FLOW//1HOUR/SYN/", &DatasetOptions::whole())
        .unwrap();
    assert_eq!(
        user_header_text(&d.report.user_header),
        "Gage: N1;VerticalDatum: NGVD29;"
    );
    // The committed Python fixture is this file (regenerate with DSS_PY_FIXTURE_OUT).
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_userheader_nul6.dss"), &bytes).unwrap();
    }
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_userheader_nul6.dss");
    let on_disk = std::fs::read(&committed).expect("public fixture missing");
    assert_eq!(
        on_disk, bytes,
        "committed fixture differs from the generator"
    );
}

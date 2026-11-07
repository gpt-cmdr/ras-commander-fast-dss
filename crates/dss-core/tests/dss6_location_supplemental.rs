//! DSS6 location supplemental text from the time-series user header (packet P27).
//!
//! Monolith fills a DSS6 time series' `supplementalInfo` from the record's user header
//! (`javaHeclib/src/Hec_ztsRetrieve.c`, "User header (supplemental)"); HEC's C library
//! leaves the location's supplemental text empty (`Dss6/ztsRetrieveReg6.c`). The engine
//! follows Monolith for the text only: coordinates, codes and the presence rule (HEC
//! `LCOORDS`) are unchanged, and a datum named in the text stays informational (D37).
//!
//! `fixtures/public/synthetic_location_uh6.dss` is generated here (no HEC software;
//! regenerate with `DSS_PY_FIXTURE_OUT=<dir>`); the test fails when the committed bytes
//! differ from the generator.

mod dss6_ts_support;

use dss6_ts_support::{f32w, f64w, its_header, jul, rts_header, text, ts_file, Payload, Rec};
use dss_core::dss6::{DatasetIndex, DatasetOptions, Dss6File};
use dss_core::location::{Location, LocationSource, SupplementalSource, VerticalDatum};
use dss_core::user_header::user_header_text;
use std::path::PathBuf;

/// Three regular blocks with the same coordinates and different headers (no NUL,
/// blank-padded as Monolith writes them). Catalog order is JAN, DEC, MAR (DSS6 bins are
/// listed in order of first use; JAN and DEC hash to one bin, MAR to another): the
/// date-last block (DEC) is neither first nor last in catalog order.
const H_JAN: &str = "/P27/GAGE H/FLOW/01JAN2000/1HOUR/OBS/";
const H_MAR: &str = "/P27/GAGE H/FLOW/01MAR2000/1HOUR/OBS/";
const H_DEC: &str = "/P27/GAGE H/FLOW/01DEC2000/1HOUR/OBS/";
/// Irregular dataset, catalog order JAN, MAR (no FEB block): JAN has a NUL-terminated
/// header followed by junk, MAR a plain one.
const I_IRR: &str = "/P27/GAGE I/STAGE/01JAN2000/IR-MONTH/OBS/";
const I_MAR: &str = "/P27/GAGE I/STAGE/01MAR2000/IR-MONTH/OBS/";
/// Coordinates, no user-header words.
const J_NONE: &str = "/P27/GAGE J/FLOW/01JAN2000/1HOUR/OBS/";
/// User-header text, but no coordinates: no location (presence rule unchanged).
const K_NOCOORD: &str = "/P27/GAGE K/FLOW/01JAN2000/1HOUR/OBS/";
/// Coordinates, user header of blanks only: no text.
const L_BLANK: &str = "/P27/GAGE L/FLOW/01JAN2000/1HOUR/OBS/";
/// Irregular dataset whose second block (in catalog order) has a D part that is not a
/// block date (`parse_d_part` gives `None`): it sorts first, as in `DatasetIndex`.
const N_JAN: &str = "/P27/GAGE N/STAGE/01JAN2000/IR-MONTH/OBS/";
const N_BAD: &str = "/P27/GAGE N/STAGE/NOTADATE/IR-MONTH/OBS/";

const XYZ_H: [f64; 3] = [-90.5, 30.25, 12.0];
/// Lat/long, decimal degrees, NAD83, feet, vertical datum 0 (unset).
const DESC_H: [i32; 6] = [1, 0, 3, 1, 1, 0];
const XYZ_I: [f64; 3] = [-91.0, 31.0, 0.0];
/// As DESC_H with vertical datum 2 (NGVD-29).
const DESC_I: [i32; 6] = [1, 0, 3, 1, 1, 2];

const TEXT_H_JAN: &str = "Gage: H1;VerticalDatum: NAVD88;Note: january";
const TEXT_H_MAR: &str = "Gage: H2;VerticalDatum: NAVD88;Note: march";
const TEXT_H_DEC: &str = "Gage: H3;VerticalDatum: NAVD88;Note: december";
const TEXT_I: &str = "Gage: I1;VerticalDatum: NAVD88;";
const TEXT_I_MAR: &str = "Gage: I3;Note: march";
const TEXT_N_JAN: &str = "Gage: N1;Note: dated block";
const TEXT_N_BAD: &str = "Gage: N0;Note: undated block";

fn words(b: &[u8]) -> Vec<i32> {
    let mut b = b.to_vec();
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Coordinates and descriptors at word offset `c0` (0-based) of an internal header:
/// regular words 13-24 (`zsrtsi6.f` lines 878-887), irregular 17-28 (`zritsi6.f`
/// lines 737-749).
fn put_location(h: &mut Vec<i32>, c0: usize, c: [f64; 3], d: [i32; 6]) {
    if h.len() < c0 + 12 {
        h.resize(c0 + 12, 0);
    }
    for (i, v) in c.iter().enumerate() {
        let w = f64w(*v);
        h[c0 + 2 * i] = w[0];
        h[c0 + 2 * i + 1] = w[1];
    }
    h[c0 + 6..c0 + 12].copy_from_slice(&d);
}

fn rts(c: Option<([f64; 3], [i32; 6])>) -> Vec<i32> {
    let mut h = rts_header(0, "CFS", "INST-VAL", 24);
    if let Some((c, d)) = c {
        put_location(&mut h, 12, c, d);
    }
    h
}

fn regular(f: &mut dss6_ts_support::TsFile, path: &str, h: Vec<i32>, uh: Vec<i32>, n: i32) {
    f.attach(
        path,
        &Payload {
            internal_header: h,
            user_header: uh,
            data: (0..n).map(|i| f32w(i as f32)).collect(),
            kilnda: n,
            ..Default::default()
        },
    );
}

/// Irregular block starting at (y, m, 1) with two values and the location of GAGE I:
/// 10 header words, a blank 24-character time-zone name (words 11-16), then the
/// location (words 17-28).
fn irregular(f: &mut dss6_ts_support::TsFile, path: &str, y: i64, m: u32, uh: Vec<i32>) {
    let j = jul(y, m, 1) as i32;
    let mut ih = its_header(2, 2, j, 0, "FEET", "INST-VAL");
    ih[3] = 120; // time of the last value (zritsi6.f lines 724-746)
    ih.extend(text("", 6));
    put_location(&mut ih, 16, XYZ_I, DESC_I);
    f.attach(
        path,
        &Payload {
            internal_header: ih,
            user_header: uh,
            data: vec![60, f32w(1.0), 120, f32w(2.0)],
            kilnda: 2,
            ..Default::default()
        },
    );
}

fn file() -> Vec<u8> {
    // Catalog order differs from date order; see the constants above.
    let recs = vec![
        Rec::live(H_JAN, 100),
        Rec::live(H_DEC, 100),
        Rec::live(H_MAR, 100),
        Rec::live(I_IRR, 110),
        Rec::live(I_MAR, 110),
        Rec::live(J_NONE, 100),
        Rec::live(K_NOCOORD, 100),
        Rec::live(L_BLANK, 100),
        Rec::live(N_JAN, 110),
        Rec::live(N_BAD, 110),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    let loc_h = Some((XYZ_H, DESC_H));
    regular(&mut f, H_JAN, rts(loc_h), text(TEXT_H_JAN, 12), 744);
    regular(&mut f, H_MAR, rts(loc_h), text(TEXT_H_MAR, 12), 744);
    regular(&mut f, H_DEC, rts(loc_h), text(TEXT_H_DEC, 12), 744);
    regular(&mut f, J_NONE, rts(loc_h), Vec::new(), 744);
    regular(&mut f, K_NOCOORD, rts(None), text("Gage: K1", 2), 744);
    regular(&mut f, L_BLANK, rts(loc_h), text("", 3), 744);
    let mut uh = TEXT_I.as_bytes().to_vec();
    uh.extend_from_slice(b"\x00junk;Hidden: 1");
    irregular(&mut f, I_IRR, 2000, 1, words(&uh));
    irregular(&mut f, I_MAR, 2000, 3, text(TEXT_I_MAR, 5));
    irregular(&mut f, N_JAN, 2000, 1, text(TEXT_N_JAN, 7));
    irregular(&mut f, N_BAD, 2000, 2, text(TEXT_N_BAD, 7));
    f.finish()
}

/// The location the engine reports for one block: coordinates from the internal header,
/// supplemental text from the same block's user header.
fn block_location(f: &Dss6File<&[u8]>, p: &str) -> Option<Location> {
    let r = f.read_ts_record(p.as_bytes()).unwrap();
    Location::from_dss6_ts(&r.pathname, r.coordinates, r.coordinate_descriptors)
        .map(|l| l.with_dss6_user_header(&r.user_header))
}

#[test]
fn supplemental_from_user_header() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();

    let jan = block_location(&f, H_JAN).unwrap();
    assert_eq!(jan.source, LocationSource::Dss6TsHeader);
    assert_eq!(jan.supplemental_text().as_deref(), Some(TEXT_H_JAN));
    assert_eq!(
        jan.supplemental_source(),
        Some(SupplementalSource::Dss6UserHeader)
    );
    // Typed fields come from the internal header only (D37).
    assert_eq!((jan.x, jan.y, jan.z), (XYZ_H[0], XYZ_H[1], XYZ_H[2]));
    assert_eq!(jan.vertical_datum(), VerticalDatum::Unset);
    assert_eq!(jan.vertical_datum_code, 0);
    assert_eq!(
        jan.supplemental_vertical_datum().as_deref(),
        Some(" NAVD88")
    );
    assert!(jan.time_zone_name.is_none());

    let irr = block_location(&f, I_IRR).unwrap();
    assert_eq!(irr.supplemental_text().as_deref(), Some(TEXT_I));
    assert_eq!(irr.vertical_datum(), VerticalDatum::Ngvd29);
    assert_eq!(
        irr.supplemental_vertical_datum().as_deref(),
        Some(" NAVD88")
    );

    // No header words, or a blank header: no text, no source.
    for p in [J_NONE, L_BLANK] {
        let l = block_location(&f, p).unwrap();
        assert!(l.supplemental.is_none(), "{p}");
        assert!(l.supplemental_source().is_none(), "{p}");
    }
    // Header text without coordinates: still no location.
    let k = f.read_ts_record(K_NOCOORD.as_bytes()).unwrap();
    assert_eq!(k.user_header_text(), "Gage: K1");
    assert!(block_location(&f, K_NOCOORD).is_none());
}

/// The dataset's reported header (P26, dss6.md §14.4) is the last block by date, which
/// is also the last block of `DatasetIndex` (the order the Python fallback uses).
#[test]
fn dataset_reports_last_block_header() {
    let bytes = file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let d = f
        .read_ts_dataset(b"/P27/GAGE H/FLOW//1HOUR/OBS/", &DatasetOptions::whole())
        .unwrap();
    assert_eq!(user_header_text(&d.report.user_header), TEXT_H_DEC);
    assert_eq!(
        d.report.user_header_block.as_deref(),
        Some(H_DEC.as_bytes())
    );
    assert_eq!(d.report.user_header_conflicts, 2);
    let index = DatasetIndex::build(&f).unwrap();
    let blocks = index.blocks(H_JAN.as_bytes());
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks.last().unwrap().pathname, H_DEC.as_bytes());

    // Irregular whole dataset: the last block looked up, MAR (no FEB block stored).
    let d = f
        .read_ts_dataset(
            b"/P27/GAGE I/STAGE//IR-MONTH/OBS/",
            &DatasetOptions::whole(),
        )
        .unwrap();
    assert_eq!(user_header_text(&d.report.user_header), TEXT_I_MAR);
    assert_eq!(
        d.report.user_header_block.as_deref(),
        Some(I_MAR.as_bytes())
    );
    let blocks = index.blocks(I_IRR.as_bytes());
    assert_eq!(blocks.last().unwrap().pathname, I_MAR.as_bytes());

    // A D part that is not a block date sorts first (julian `None`).
    let blocks = index.blocks(N_JAN.as_bytes());
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].julian, None);
    assert_eq!(blocks[0].pathname, N_BAD.as_bytes());
    assert_eq!(blocks[1].pathname, N_JAN.as_bytes());
    let n = block_location(&f, N_BAD).unwrap();
    assert_eq!(n.supplemental_text().as_deref(), Some(TEXT_N_BAD));
    let n = block_location(&f, N_JAN).unwrap();
    assert_eq!(n.supplemental_text().as_deref(), Some(TEXT_N_JAN));
}

#[test]
fn committed_fixture_matches_generator() {
    let bytes = file();
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_location_uh6.dss"), &bytes).unwrap();
    }
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_location_uh6.dss");
    let on_disk = std::fs::read(&committed).expect("public fixture missing");
    assert_eq!(
        on_disk, bytes,
        "committed fixture differs from the generator"
    );
}

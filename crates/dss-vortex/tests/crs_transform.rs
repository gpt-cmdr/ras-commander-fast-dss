//! Coordinate transformations against bits from PROJ 7.2.0 inside Vortex 1.1.0.
//!
//! `crs_golden.json` and `crs_golden.bin` hold the recorded results of a full run against
//! Vortex. Each point stores its input, PROJ's output and the portable build's
//! output.
//!
//! - Every build: the failure mask must equal PROJ's.
//! - The portable build: every output must have the recorded portable bits, on every platform.
//!   That is the cross-platform identity the platform-parity decision requires, and it fails on
//!   any changed bit.
//! - The record itself: the portable bits must be PROJ's, apart from the listed points, where
//!   they must differ. So this holds PROJ's bits on every platform, not only agreement between
//!   platforms.
//!
//! The portable maths, not the host's, is what these bits check (see `crs_maths_gate.rs`).

use dss_vortex::crs::{CrsDef, DatumPolicy, Transformer};
use std::fs;
use std::path::Path;

fn data(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(name),
    )
    .unwrap()
}

fn f64_at(bytes: &[u8], i: usize) -> f64 {
    f64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
}

fn same_bits(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

/// What differs from the record. `disturb` is how the controls show that this notices.
fn golden_problems(disturb: impl Fn(f64) -> f64) -> (usize, Vec<String>) {
    let manifest: serde_json::Value = serde_json::from_slice(&data("crs_golden.json")).unwrap();
    let bin = data("crs_golden.bin");
    let mut at = 0;
    let mut points = 0;
    let mut wrong = Vec::new();
    for case in manifest["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let parse = |k: &str| CrsDef::parse_wkt(case[k].as_str().unwrap()).unwrap();
        let policy = if case["opt_in"].as_bool().unwrap_or(false) {
            DatumPolicy::AssumeNoShift
        } else {
            DatumPolicy::Refuse
        };
        let center = case["center_long"].as_f64();
        let t = Transformer::new(&parse("src"), &parse("dst"), policy)
            .unwrap()
            .with_center_long(center, center);
        let n = case["n"].as_u64().unwrap();
        for k in 0..n {
            let (x, y) = (f64_at(&bin, at), f64_at(&bin, at + 1));
            let ox = f64_at(&bin, at + 2);
            let (px, py) = (f64_at(&bin, at + 4), f64_at(&bin, at + 5));
            at += 6;
            let (rx, ry) = t.transform(x, y).unwrap_or((f64::INFINITY, f64::INFINITY));
            let (rx, ry) = (disturb(rx), disturb(ry));
            if (ox == f64::INFINITY) != (rx == f64::INFINITY) {
                wrong.push(format!("{id}: failure mask differs at ({x}, {y})"));
            }
            let (wx, wy) = (px, py);
            if !same_bits(rx, wx) || !same_bits(ry, wy) {
                wrong.push(format!(
                    "{id}: point {k} is ({rx:e}, {ry:e}), expected ({wx:e}, {wy:e})"
                ));
            }
        }
        points += n as usize;
    }
    assert_eq!(at * 8, bin.len());
    (points, wrong)
}

#[test]
fn golden_points_have_the_recorded_bits() {
    let (points, wrong) = golden_problems(|v| v);
    assert!(points > 1000, "only {points} points");
    assert!(wrong.is_empty(), "{} problems: {wrong:#?}", wrong.len());
}

/// Points where the record's portable bits differ from PROJ's, by whether the case lists them.
fn record_differences() -> (Vec<String>, usize) {
    let manifest: serde_json::Value = serde_json::from_slice(&data("crs_golden.json")).unwrap();
    let bin = data("crs_golden.bin");
    let mut at = 0;
    let (mut unlisted, mut listed_seen) = (Vec::new(), 0);
    for case in manifest["cases"].as_array().unwrap() {
        let listed = case["listed"].as_array().unwrap();
        for k in 0..case["n"].as_u64().unwrap() {
            let differs = !same_bits(f64_at(&bin, at + 2), f64_at(&bin, at + 4))
                || !same_bits(f64_at(&bin, at + 3), f64_at(&bin, at + 5));
            let is_listed = listed.iter().any(|e| e["k"].as_u64() == Some(k));
            if is_listed {
                assert!(differs, "{}: listed point {k} equals PROJ", case["id"]);
                listed_seen += 1;
            } else if differs {
                unlisted.push(format!("{}: point {k}", case["id"]));
            }
            at += 6;
        }
    }
    (unlisted, listed_seen)
}

#[test]
fn the_portable_record_is_proj_apart_from_the_listed_d50_points() {
    let (unlisted, listed) = record_differences();
    assert!(unlisted.is_empty(), "{unlisted:#?}");
    assert_eq!(listed, 21, "three listed points in each of the 7 cases");
}

/// Negative controls: a large error on every finite output, and one low bit.
#[test]
fn golden_check_notices_any_changed_output() {
    let (_, wrong) = golden_problems(|v| if v.is_finite() { v + 1.0e6 } else { v });
    assert!(wrong.len() > 1000, "{}", wrong.len());
    let (_, wrong) = golden_problems(|v| {
        if v.is_finite() {
            f64::from_bits(v.to_bits() ^ 1)
        } else {
            v
        }
    });
    assert!(wrong.len() > 1000, "{}", wrong.len());
}

fn nad83_lonlat() -> CrsDef {
    CrsDef::parse_wkt(
        "GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
    )
    .unwrap()
}

#[test]
fn same_crs_and_geographic_pairs_keep_every_bit() {
    let shg = CrsDef::shg();
    let t = Transformer::new(&shg, &shg, DatumPolicy::Refuse).unwrap();
    assert_eq!(t.transform(1.5e6, 2.25e6), Ok((1.5e6, 2.25e6)));
    let t = Transformer::new(
        &CrsDef::sphere_lonlat(6371229.0),
        &nad83_lonlat(),
        DatumPolicy::Refuse,
    )
    .unwrap();
    let p = (-96.12345678901234, 39.987654321098765);
    assert_eq!(t.transform(p.0, p.1), Ok(p));
}

/// The datum policy treats NAD83 and WGS84 as no shift everywhere. PROJ 7.2.0 applies "NAD83 to WGS 84 (3)"
/// around Hawaii and "(2)" around the Aleutians on some CRS pairs; those points are listed
/// in `crs_golden.json` (see `the_portable_record_is_proj_apart_from_the_listed_d50_points`).
#[test]
fn nad83_and_wgs84_are_no_shift_even_where_proj_shifts() {
    let wgs84 = CrsDef::parse_wkt(
        "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
    )
    .unwrap();
    let t = Transformer::new(&wgs84, &nad83_lonlat(), DatumPolicy::Refuse).unwrap();
    assert_eq!(t.transform(-157.0, 21.0), Ok((-157.0, 21.0)));
}

#[test]
fn center_long_wraps_only_the_geographic_side() {
    let t = Transformer::new(
        &CrsDef::shg(),
        &CrsDef::sphere_lonlat(6371229.0),
        DatumPolicy::Refuse,
    )
    .unwrap();
    let plain = t.clone().transform(-3_000_000.0, 7_000_000.0).unwrap();
    let wrapped = t
        .with_center_long(Some(0.0), Some(plain.0 + 180.0 - 1.0))
        .transform(-3_000_000.0, 7_000_000.0)
        .unwrap();
    assert_eq!(wrapped.0, plain.0);
    let t = Transformer::new(
        &CrsDef::shg(),
        &CrsDef::sphere_lonlat(6371229.0),
        DatumPolicy::Refuse,
    )
    .unwrap()
    .with_center_long(None, Some(plain.0 + 200.0));
    let moved = t.transform(-3_000_000.0, 7_000_000.0).unwrap();
    assert_eq!(moved.0, plain.0 + 360.0);
}

#[test]
fn failed_points_are_reported_not_invented() {
    let t = Transformer::new(&nad83_lonlat(), &CrsDef::shg(), DatumPolicy::Refuse).unwrap();
    assert!(t.transform(-96.0, 91.0).is_err());
    assert!(t.transform(f64::INFINITY, 40.0).is_err());
    assert!(t.transform(-1000.0, 40.0).is_err());
}

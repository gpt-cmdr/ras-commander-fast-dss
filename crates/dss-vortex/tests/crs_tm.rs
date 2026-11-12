//! Transverse Mercator against bits from PROJ 7.2.0 inside Vortex 1.1.0.
//!
//! `crs_tm_golden.json` and `crs_tm_golden.bin` hold the recorded results of a full run against
//! Vortex. Each point stores its input, PROJ's output and
//! the portable build's output, as `crs_transform.rs` does for its cases:
//!
//! - the failure mask must equal PROJ's;
//! - every output must have the recorded portable bits, on every platform;
//! - the recorded portable bits must be PROJ's, apart from the listed points, where they must
//!   differ. Those are the inverse's `exp` residuals, listed per case in `crs_tm_golden.json`.

use dss_vortex::crs::{CrsDef, Datum, DatumPolicy, LinearUnit, Projection, Transformer};
use dss_vortex::VortexError;
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
fn golden_problems(disturb: impl Fn(f64) -> f64) -> (usize, usize, Vec<String>) {
    let manifest: serde_json::Value = serde_json::from_slice(&data("crs_tm_golden.json")).unwrap();
    let bin = data("crs_tm_golden.bin");
    let (mut at, mut points, mut failed) = (0, 0, 0);
    let mut wrong = Vec::new();
    for case in manifest["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let parse = |k: &str| CrsDef::parse_wkt(case[k].as_str().unwrap()).unwrap();
        let t = Transformer::new(&parse("src"), &parse("dst"), DatumPolicy::Refuse).unwrap();
        let n = case["n"].as_u64().unwrap();
        for k in 0..n {
            let (x, y) = (f64_at(&bin, at), f64_at(&bin, at + 1));
            let ox = f64_at(&bin, at + 2);
            let (px, py) = (f64_at(&bin, at + 4), f64_at(&bin, at + 5));
            at += 6;
            let (rx, ry) = t.transform(x, y).unwrap_or((f64::INFINITY, f64::INFINITY));
            let (rx, ry) = (disturb(rx), disturb(ry));
            failed += usize::from(ox == f64::INFINITY);
            if (ox == f64::INFINITY) != (rx == f64::INFINITY) {
                wrong.push(format!("{id}: failure mask differs at ({x}, {y})"));
            }
            if !same_bits(rx, px) || !same_bits(ry, py) {
                wrong.push(format!(
                    "{id}: point {k} is ({rx:e}, {ry:e}), expected ({px:e}, {py:e})"
                ));
            }
        }
        points += n as usize;
    }
    assert_eq!(at * 8, bin.len());
    (points, failed, wrong)
}

#[test]
fn golden_points_have_the_recorded_bits() {
    let (points, failed, wrong) = golden_problems(|v| v);
    assert_eq!(points, 3989);
    // Points PROJ fails are in the record too, so the mask is compared on both kinds.
    assert!(failed > 200, "only {failed} points PROJ fails");
    assert!(wrong.is_empty(), "{} problems: {wrong:#?}", wrong.len());
}

#[test]
fn the_portable_record_is_proj_apart_from_the_listed_exp_residuals() {
    let manifest: serde_json::Value = serde_json::from_slice(&data("crs_tm_golden.json")).unwrap();
    let bin = data("crs_tm_golden.bin");
    let mut at = 0;
    let (mut unlisted, mut listed_seen) = (Vec::new(), 0);
    for case in manifest["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let listed = case["listed"].as_array().unwrap();
        // The residuals come from `exp`, which only the inverse calls.
        assert!(
            listed.is_empty() || !(id.contains(".fwd.") || id.contains(".from_shg")),
            "{id} lists a residual on a forward-only case"
        );
        for k in 0..case["n"].as_u64().unwrap() {
            let differs = !same_bits(f64_at(&bin, at + 2), f64_at(&bin, at + 4))
                || !same_bits(f64_at(&bin, at + 3), f64_at(&bin, at + 5));
            if listed.iter().any(|e| e["k"].as_u64() == Some(k)) {
                assert!(differs, "{id}: listed point {k} equals PROJ");
                listed_seen += 1;
            } else if differs {
                unlisted.push(format!("{id}: point {k}"));
            }
            at += 6;
        }
    }
    assert!(unlisted.is_empty(), "{unlisted:#?}");
    assert_eq!(
        listed_seen, 33,
        "every residual of the full run is in the record"
    );
}

/// Controls: a large error on every finite output, and one low bit.
#[test]
fn golden_check_notices_any_changed_output() {
    let (_, _, wrong) = golden_problems(|v| if v.is_finite() { v + 1.0e6 } else { v });
    assert!(wrong.len() > 3000, "{}", wrong.len());
    let (_, _, wrong) = golden_problems(|v| {
        if v.is_finite() {
            f64::from_bits(v.to_bits() ^ 1)
        } else {
            v
        }
    });
    assert!(wrong.len() > 3000, "{}", wrong.len());
    // And a point made to fail where PROJ succeeds.
    let (_, _, wrong) = golden_problems(|v| if v.is_finite() { f64::INFINITY } else { v });
    assert!(wrong.iter().any(|w| w.contains("failure mask")));
}

const NAD83: &str = "GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";

fn tm(geog: &str, values: [&str; 5], unit: &str) -> String {
    let [lat_0, lon_0, k, fe, fn_] = values;
    format!(
        "PROJCS[\"p\",{geog},PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"latitude_of_origin\",{lat_0}],PARAMETER[\"central_meridian\",{lon_0}],PARAMETER[\"scale_factor\",{k}],PARAMETER[\"false_easting\",{fe}],PARAMETER[\"false_northing\",{fn_}],{unit}]"
    )
}

const METRE: &str = "UNIT[\"Meter\",1]";
const UTM15: [&str; 5] = ["0", "-93", "0.9996", "500000", "0"];

#[test]
fn utm_is_recognised_by_projs_rule_and_nothing_near_it_is() {
    let zone = |values: [&str; 5], unit: &str| {
        CrsDef::parse_wkt(&tm(NAD83, values, unit))
            .unwrap()
            .utm_zone()
    };
    assert_eq!(zone(UTM15, METRE), Some((15, false)));
    assert_eq!(
        zone(["0", "15", "0.9996", "500000", "10000000"], METRE),
        Some((33, true))
    );
    assert_eq!(
        zone(["0", "-177", "0.9996", "500000", "0"], METRE),
        Some((1, false))
    );
    assert_eq!(
        zone(["0", "177", "0.9996", "500000", "0"], METRE),
        Some((60, false))
    );
    // Each differs from UTM in one value, and PROJ then uses `tmerc` (measured: the cases
    // `tm_utm_but_*` and `tm_utm_shape_ftus`).
    for values in [
        ["0", "-93", "0.9996", "500000", "100"],
        ["0", "-93", "1", "500000", "0"],
        ["0", "-92.5", "0.9996", "500000", "0"],
        ["1", "-93", "0.9996", "500000", "0"],
        ["0", "-93", "0.9996", "500001", "0"],
        ["0", "-189", "0.9996", "500000", "0"],
    ] {
        assert_eq!(zone(values, METRE), None, "{values:?}");
    }
    assert_eq!(
        zone(
            ["0", "-93", "0.9996", "1640416.666666667", "0"],
            "UNIT[\"Foot_US\",0.3048006096012192]"
        ),
        None
    );
    assert_eq!(
        CrsDef::utm(Datum::Nad83, 6378137.0, 298.257222101, 15, false),
        Some(CrsDef::parse_wkt(&tm(NAD83, UTM15, METRE)).unwrap())
    );
    assert_eq!(
        CrsDef::utm(Datum::Wgs84, 6378137.0, 298.257223563, 0, false),
        None
    );
    assert_eq!(
        CrsDef::utm(Datum::Wgs84, 6378137.0, 298.257223563, 61, true),
        None
    );
}

#[test]
fn feet_are_read_as_feet_and_converted_never_relabelled() {
    let us = "UNIT[\"Foot_US\",0.3048006096012192]";
    let intl = "UNIT[\"Foot\",0.3048]";
    let values = ["31", "-110", "0.9999", "700000", "0"];
    let in_unit = |unit: &str| CrsDef::parse_wkt(&tm(NAD83, values, unit)).unwrap();
    let geographic = CrsDef::parse_wkt(NAD83).unwrap();
    let at = |def: &CrsDef| {
        Transformer::new(&geographic, def, DatumPolicy::Refuse)
            .unwrap()
            .transform(-109.5, 33.0)
            .unwrap()
    };
    assert_eq!(in_unit(us).metres_per_unit(), 0.3048006096012192);
    assert_eq!(in_unit(intl).metres_per_unit(), 0.3048);
    assert_eq!(in_unit(METRE).metres_per_unit(), 1.0);
    let (m, f_us, f_intl) = (at(&in_unit(METRE)), at(&in_unit(us)), at(&in_unit(intl)));
    // The false easting is 700000 of each CRS's own unit, so the three differ in more than scale:
    // the easting from the central meridian is what scales.
    let east_m = m.0 - 700000.0;
    let ratio_us = east_m / (f_us.0 - 700000.0);
    let ratio_intl = east_m / (f_intl.0 - 700000.0);
    assert!((ratio_us - 1200.0 / 3937.0).abs() < 1e-12, "{ratio_us}");
    assert!((ratio_intl - 0.3048).abs() < 1e-12, "{ratio_intl}");
    // The two feet differ by 2 parts per million: 0.4 ft at this northing, which a relabelling
    // or a swapped factor would show.
    let north = f_us.1 - f_intl.1;
    assert!(north.abs() > 0.3 && north.abs() < 3.0, "{north}");
    // A unit whose name contradicts its value is refused, as is a unit not compared.
    for unit in [
        "UNIT[\"Meter\",0.3048]",
        "UNIT[\"Foot\",0.3048006096012192]",
        "UNIT[\"Foot_US\",0.3048]",
        "UNIT[\"Foot_US\",1]",
        "UNIT[\"kilometre\",1000]",
        "UNIT[\"x\",0.5]",
    ] {
        let result = CrsDef::parse_wkt(&tm(NAD83, values, unit));
        assert!(
            matches!(result, Err(VortexError::Unsupported { .. })),
            "{unit}: {result:?}"
        );
    }
    // Feet stay refused on a method that has not been compared in them.
    let lambert = "PROJCS[\"p\",GEOGCS[\"g\",DATUM[\"NAD83\",SPHEROID[\"s\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Lambert_Conformal_Conic_2SP\"],PARAMETER[\"latitude_of_origin\",40],PARAMETER[\"central_meridian\",-97],PARAMETER[\"standard_parallel_1\",33],PARAMETER[\"standard_parallel_2\",45],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0],UNIT[\"Foot_US\",0.3048006096012192]]";
    assert!(matches!(
        CrsDef::parse_wkt(lambert),
        Err(VortexError::Unsupported { .. })
    ));
    assert_eq!(
        LinearUnit::of_metres(0.304800609601219),
        Some(LinearUnit::UsSurveyFoot)
    );
    assert_eq!(
        LinearUnit::of_metres(0.3048),
        Some(LinearUnit::InternationalFoot)
    );
    assert_eq!(LinearUnit::of_metres(0.30481), None);
}

#[test]
fn written_text_reads_back_as_the_same_definition() {
    for (values, unit) in [
        (UTM15, METRE),
        (
            [
                "24.33333333333333",
                "-81",
                "0.9999411764705882",
                "656166.6666666665",
                "0",
            ],
            "UNIT[\"Foot_US\",0.3048006096012192]",
        ),
        (
            ["31", "-111.9166666666667", "0.9999", "700000", "0"],
            "UNIT[\"Foot\",0.3048]",
        ),
    ] {
        let def = CrsDef::parse_wkt(&tm(NAD83, values, unit)).unwrap();
        let text = def.to_wkt();
        assert!(
            text.contains("PROJECTION[\"Transverse_Mercator\"]"),
            "{text}"
        );
        let again = CrsDef::parse_wkt(&text).unwrap();
        // `%.15g` may shorten a stated value; what PROJ reads from either is the same 15 digits.
        let Projection::TransverseMercator { to_metre, .. } = again.projection else {
            panic!("{text}")
        };
        assert_eq!(to_metre, def.metres_per_unit(), "{text}");
        let geographic = CrsDef::parse_wkt(NAD83).unwrap();
        let through = |d: &CrsDef| {
            Transformer::new(&geographic, d, DatumPolicy::Refuse)
                .unwrap()
                .transform(-90.25, 30.5)
                .unwrap()
        };
        let (a, b) = (through(&def), through(&again));
        assert_eq!(
            (a.0.to_bits(), a.1.to_bits()),
            (b.0.to_bits(), b.1.to_bits()),
            "{text}"
        );
    }
}

/// PROJ's pipeline between two texts of one CRS is `noop`, also past the projection's limits
/// (measured in Vortex 1.1.0: 161,111 points returned unchanged, none failed).
#[test]
fn two_texts_of_one_crs_are_the_identity() {
    let values = [
        "24.33333333333333",
        "-81",
        "0.9999411764705882",
        "656166.6666666665",
        "0",
    ];
    let a = CrsDef::parse_wkt(&tm(NAD83, values, "UNIT[\"Foot_US\",0.3048006096012192]")).unwrap();
    let b = CrsDef::parse_wkt(&tm(
        NAD83,
        values,
        "UNIT[\"US survey foot\",0.304800609601219]",
    ))
    .unwrap();
    assert_ne!(a, b);
    let t = Transformer::new(&a, &b, DatumPolicy::Refuse).unwrap();
    for (x, y) in [(602000.0, 801000.0), (0.1, -0.3), (1e12, 1e12)] {
        let (rx, ry) = t.transform(x, y).unwrap();
        assert_eq!((rx.to_bits(), ry.to_bits()), (x.to_bits(), y.to_bits()));
    }
    // A false easting that differs within 15 digits is another CRS.
    let mut other = values;
    other[3] = "656166.667";
    let c = CrsDef::parse_wkt(&tm(NAD83, other, "UNIT[\"Foot_US\",0.3048006096012192]")).unwrap();
    let (rx, _) = Transformer::new(&a, &c, DatumPolicy::Refuse)
        .unwrap()
        .transform(602000.0, 801000.0)
        .unwrap();
    assert!((rx - 602000.0).abs() > 1e-4, "{rx}");
}

#[test]
fn a_sphere_nad27_and_a_malformed_definition_are_refused() {
    // PROJ's spherical form is another algorithm, not ported.
    let sphere = "GEOGCS[\"unknown\",DATUM[\"unknown\",SPHEROID[\"sphere\",6371229,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";
    let on_sphere = CrsDef::parse_wkt(&tm(sphere, UTM15, METRE)).unwrap();
    let geographic = CrsDef::parse_wkt(sphere).unwrap();
    assert!(matches!(
        Transformer::new(&geographic, &on_sphere, DatumPolicy::Refuse),
        Err(VortexError::Unsupported { .. })
    ));
    // NAD27 to NAD83 is refused unless the caller accepts no datum shift.
    let nad27 = "GEOGCS[\"NAD27\",DATUM[\"North_American_Datum_1927\",SPHEROID[\"Clarke 1866\",6378206.4,294.978698213898]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";
    let utm27 = CrsDef::parse_wkt(&tm(nad27, UTM15, METRE)).unwrap();
    let nad83 = CrsDef::parse_wkt(NAD83).unwrap();
    assert!(matches!(
        Transformer::new(&nad83, &utm27, DatumPolicy::Refuse),
        Err(VortexError::Refused { .. })
    ));
    assert!(Transformer::new(&nad83, &utm27, DatumPolicy::AssumeNoShift).is_ok());
    // Every parameter is required, and none may be repeated or unknown.
    let whole = tm(NAD83, UTM15, METRE);
    for name in [
        "latitude_of_origin",
        "central_meridian",
        "scale_factor",
        "false_easting",
        "false_northing",
    ] {
        let start = whole.find(&format!("PARAMETER[\"{name}\"")).unwrap();
        let end = start + whole[start..].find("],").unwrap() + 2;
        let without = format!("{}{}", &whole[..start], &whole[end..]);
        assert!(CrsDef::parse_wkt(&without).is_err(), "{name}");
        let twice = format!("{}{}{}", &whole[..end], &whole[start..end], &whole[end..]);
        assert!(CrsDef::parse_wkt(&twice).is_err(), "{name} twice");
    }
    let extra = whole.replace(
        "PARAMETER[\"scale_factor\"",
        "PARAMETER[\"standard_parallel_1\",30],PARAMETER[\"scale_factor\"",
    );
    assert!(matches!(
        CrsDef::parse_wkt(&extra),
        Err(VortexError::Unsupported { .. })
    ));
}

#[test]
fn a_point_past_projs_limit_fails_and_one_inside_it_does_not() {
    let utm = CrsDef::parse_wkt(&tm(NAD83, UTM15, METRE)).unwrap();
    let geographic = CrsDef::parse_wkt(NAD83).unwrap();
    let forward = Transformer::new(&geographic, &utm, DatumPolicy::Refuse).unwrap();
    let inverse = Transformer::new(&utm, &geographic, DatumPolicy::Refuse).unwrap();
    // On the zone's meridian, and 80 degrees from it at the equator.
    assert!(forward.transform(-93.0, 45.0).is_ok());
    assert!(forward.transform(-13.0, 0.0).is_ok());
    // 90 degrees from it at the equator is past the limit on the normalised easting.
    assert!(forward.transform(-3.0, 0.0).is_err());
    assert!(forward.transform(-93.0, 91.0).is_err());
    assert!(inverse.transform(500000.0, 4000000.0).is_ok());
    assert!(inverse.transform(1.0e9, 0.0).is_err());
    assert!(inverse.transform(f64::INFINITY, 0.0).is_err());
}

/// A cell size is metres. By default it becomes the CRS's own foot at the spacing a record
/// stores, the nearest `f32`: checked here against the exact ratios, 3937/1200 US survey feet and
/// 1250/381 international feet to the metre. Under `Align::Vortex` it is Vortex's rule
/// (`Resampler.getTargetCellSize`), unrounded: every foot taken as the international one. Measured
/// on Vortex 1.1.0 and 0.13.3: a 10,000 m cell on a State Plane zone in US survey feet is stored
/// as 32808.3984375, the `f32` of 32808.39895, which is 10,000.0198 m.
#[test]
fn a_cell_size_is_metres_and_becomes_the_crs_foot_as_stored_or_vortexs_international_feet() {
    use dss_vortex::warp::{cell_size_in_target_units, Align, CellSize};
    let values = ["31", "-110", "0.9999", "700000", "0"];
    let metres = CrsDef::parse_wkt(&tm(NAD83, values, METRE)).unwrap();
    let us = CrsDef::parse_wkt(&tm(NAD83, values, "UNIT[\"Foot_US\",0.3048006096012192]")).unwrap();
    let us15 = CrsDef::parse_wkt(&tm(
        NAD83,
        values,
        "UNIT[\"US survey foot\",0.304800609601219]",
    ))
    .unwrap();
    let intl = CrsDef::parse_wkt(&tm(NAD83, values, "UNIT[\"Foot\",0.3048]")).unwrap();
    for align in [Align::Refuse, Align::Vortex] {
        // A metre CRS is untouched by either rule, whatever the size: no rounding is applied.
        for size in [10000.0, 2000.0, 1234.5678, 0.1] {
            assert_eq!(
                cell_size_in_target_units(size, &metres, align).to_bits(),
                size.to_bits()
            );
            assert_eq!(
                cell_size_in_target_units(size, &CrsDef::shg(), align).to_bits(),
                size.to_bits()
            );
            assert_eq!(
                cell_size_in_target_units(size, &CrsDef::hrap(), align).to_bits(),
                size.to_bits()
            );
        }
        assert_eq!(cell_size_in_target_units(3048.0, &intl, align), 10000.0);
    }
    // International feet: Vortex plans with the unrounded size, the default with the stored one.
    assert_eq!(
        cell_size_in_target_units(10000.0, &intl, Align::Vortex),
        1.0e8 / 3048.0
    );
    assert_eq!(
        cell_size_in_target_units(10000.0, &intl, Align::Refuse),
        f64::from((1.0e8 / 3048.0) as f32)
    );
    for def in [&us, &us15] {
        // Vortex's rule on a US survey foot zone: the international foot.
        let feet = cell_size_in_target_units(10000.0, def, Align::Vortex);
        assert_eq!((feet as f32).to_bits(), 0x4700_2866);
        assert_eq!(feet, 1.0e8 / 3048.0);
        // The default: the CRS's own foot, whichever digits the text gives it, as a record holds it.
        let own = cell_size_in_target_units(10000.0, def, Align::Refuse);
        assert_eq!(own, f64::from((39_370_000.0f64 / 1200.0) as f32));
        assert_eq!(own, 32808.33203125);
        assert!((feet - own).abs() > 0.06);
        // 1200 m is 3937 US survey feet exactly, and 6000 m is 19685.
        assert_eq!(
            cell_size_in_target_units(1200.0, def, Align::Refuse),
            3937.0
        );
        assert_eq!(
            cell_size_in_target_units(6000.0, def, Align::Refuse),
            19685.0
        );
        // What a caller is told. 10 km is stored 0.397 mm short, and 2 km 49.6 micrometres short.
        let ten = CellSize::of(10000.0, def, Align::Refuse);
        assert_eq!(
            (ten.stored, ten.in_target_units),
            (32808.332, 32808.33203125)
        );
        assert!(
            (ten.stored_metres - 9999.999603124206).abs() < 1e-9,
            "{}",
            ten.stored_metres
        );
        assert!((ten.difference_metres() + 0.000396875794).abs() < 1e-9);
        let two = CellSize::of(2000.0, def, Align::Refuse);
        assert_eq!(two.stored.to_bits(), 6561.6665f32.to_bits());
        assert!(
            (two.stored_metres - 1999.9999503905258).abs() < 1e-9,
            "{}",
            two.stored_metres
        );
        assert_eq!(
            CellSize::of(1200.0, def, Align::Refuse).difference_metres(),
            0.0
        );
        // Vortex's is 2 parts per million long.
        let vortex = CellSize::of(10000.0, def, Align::Vortex);
        assert!(
            (vortex.stored_metres - 10000.019843789689).abs() < 1e-8,
            "{}",
            vortex.stored_metres
        );
    }
    let shg = CellSize::of(2000.0, &CrsDef::shg(), Align::Refuse);
    assert_eq!((shg.stored, shg.difference_metres()), (2000.0, 0.0));
}

/// The rule through a warp plan: the destination lattice a caller gets. A 1,200 m cell is 3,937 US
/// survey feet exactly, so the default grid is on that lattice, with the spacing asked for; the
/// same request under Vortex's alignment has cells of 3,937.0079 ft and a lattice that has moved.
#[test]
fn a_plan_on_a_us_survey_foot_target_has_the_spacing_asked_for_by_default() {
    use dss_vortex::crs::Envelope;
    use dss_vortex::frame::GeoTransform;
    use dss_vortex::warp::{Align, Method, SourceGrid, WarpEnvelope, WarpPlan, WarpSpec};
    let florida = [
        "24.33333333333333",
        "-81",
        "0.9999411764705882",
        "656166.6666666665",
        "0",
    ];
    let target =
        CrsDef::parse_wkt(&tm(NAD83, florida, "UNIT[\"Foot_US\",0.3048006096012192]")).unwrap();
    // A geographic grid over the Florida peninsula.
    let source = SourceGrid {
        nx: 400,
        ny: 400,
        transform: GeoTransform::axis_aligned(-83.0, 0.01, 29.0, -0.01),
        crs: CrsDef::parse_wkt(NAD83).unwrap(),
    };
    let plan = |align: Align| {
        let mut spec = WarpSpec::new(Method::Near);
        spec.target = Some(target.clone());
        spec.cell_size = Some(1200.0);
        spec.align = align;
        // Whole cells of 3,937 ft: columns 127 to 203, rows 229 to 305.
        spec.envelope = Some(WarpEnvelope {
            envelope: Envelope {
                min_x: 3937.0 * 127.0,
                max_x: 3937.0 * 203.0,
                min_y: 3937.0 * 229.0,
                max_y: 3937.0 * 305.0,
            },
            crs: Some(target.clone()),
        });
        WarpPlan::new(&source, &spec).unwrap()
    };
    let own = plan(Align::Refuse);
    assert_eq!((own.nx(), own.ny()), (76, 76));
    assert_eq!(
        own.geotransform(),
        [3937.0 * 127.0, 3937.0, 0.0, 3937.0 * 305.0, 0.0, -3937.0]
    );
    let vortex = plan(Align::Vortex);
    let cell = 1200.0 * 1250.0 / 381.0;
    assert_eq!(vortex.geotransform()[1], cell);
    assert!((cell - 3937.007874015748).abs() < 1e-11);
    // Vortex's lattice starts a foot west of the one asked for, at column 127 of its own cells.
    assert_ne!(vortex.geotransform()[0], own.geotransform()[0]);
}

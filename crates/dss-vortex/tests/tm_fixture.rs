//! Transverse Mercator through the importer, with no oracle at run time.
//!
//! `fixtures/public/vortex_p53b/expected.json` holds every record Vortex 1.1.0's importer wrote from
//! 20 cases: a GRIB2 source to a UTM zone, to a State Plane
//! zone in US survey feet and to a zone in international feet, and rasters in each of those three
//! as they stand and warped to SHG by each method. This runs the same cases and compares placement,
//! names, statistics and a digest of every cell, in DSS7 and DSS6.
//!
//! Where Vortex stores GDAL's wording of a CRS, the file holds this engine's text beside it. Each
//! such pair was accepted only when rasterio read both as one CRS with the grid's middle unmoved;
//! here this engine's text must be the recorded one exactly.

use dss_core::dss6::Dss6File;
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::import::{import_gridded, Bounds, ImportParts, ImportRequest, Selection};
use dss_vortex::warp::{Align, Method};
use dss_vortex::{Crs, NamedCrs};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const CASES: usize = 20;
const RECORDS: usize = 25;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/vortex_p53b")
}

fn expected() -> Vec<Value> {
    let all: Value =
        serde_json::from_str(&fs::read_to_string(dir().join("expected.json")).unwrap()).unwrap();
    all["cases"].as_array().unwrap().clone()
}

fn case_named(id: &str) -> Value {
    expected()
        .into_iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("{id}"))
}

fn fresh(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = format!("{name}.{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("tm_fixture")
        .join(unique);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d.join("out.dss")
}

fn digest(cells: &[f32]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in cells {
        for b in c.to_bits().to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

fn request(case: &Value, output: PathBuf, version: u8) -> ImportRequest {
    let inputs = case["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| dir().join(i.as_str().unwrap()))
        .collect();
    let method = match case["method"].as_str().unwrap() {
        "near" => Method::Near,
        "bilinear" => Method::Bilinear,
        "average" => Method::Average,
        other => panic!("{other}"),
    };
    let selection = Selection::Variable(case["variable"].as_str().unwrap().to_string());
    let mut r = ImportRequest::new(inputs, output, selection, method);
    r.target = match case["target"].as_str() {
        None => None,
        Some("shg") => Some(Crs::Named(NamedCrs::Shg)),
        Some(wkt) => Some(Crs::Wkt(wkt.to_string())),
    };
    r.cell_size = case["cell"].as_f64();
    if let Some(e) = case["envelope"].as_array() {
        let e: Vec<f64> = e.iter().map(|v| v.as_f64().unwrap()).collect();
        r.bounds = Bounds::Envelope {
            envelope: Envelope {
                min_x: e[0],
                max_x: e[1],
                min_y: e[2],
                max_y: e[3],
            },
            crs: Some(Crs::Wkt(case["envelope_wkt"].as_str().unwrap().to_string())),
        };
    }
    r.align = match case["align"].as_str().unwrap() {
        "refuse" => Align::Refuse,
        "vortex" => Align::Vortex,
        other => panic!("{other}"),
    };
    let w = &case["write"];
    let part = |key: &str| w[key].as_str().unwrap_or_default().to_string();
    r.parts = ImportParts {
        a: part("partA"),
        b: part("partB"),
        c: w["partC"].as_str().map(str::to_string),
        f: part("partF"),
    };
    r.version = Some(version);
    r
}

fn read_back(path: &Path, version: u8) -> BTreeMap<String, Grid> {
    let mut out = BTreeMap::new();
    if version == 6 {
        let file = Dss6File::open(path).unwrap();
        for e in &file.catalog().unwrap().entries {
            let name = String::from_utf8_lossy(&e.pathname).into_owned();
            assert!(out
                .insert(name, file.read_grid(&e.pathname).unwrap())
                .is_none());
        }
    } else {
        let mut file = Dss7File::open(path).unwrap();
        for e in &file.catalog().unwrap().catalog.entries {
            let name = String::from_utf8_lossy(&e.pathname).into_owned();
            assert!(out
                .insert(name, file.read_grid(&e.pathname).unwrap())
                .is_none());
        }
    }
    out
}

fn f32_bits(v: Option<f32>) -> Value {
    v.map_or(Value::Null, |x| Value::from(x.to_bits()))
}

/// Every difference between what Vortex wrote and what a file holds. Empty means identical. `upper`
/// is DSS6, whose pathnames are upper case.
fn differences(wanted: &[Value], got: &BTreeMap<String, Grid>, upper: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if wanted.is_empty() || got.is_empty() {
        problems.push("a side holds no records: nothing is compared".to_string());
    }
    let mut seen = BTreeMap::new();
    for w in wanted {
        let raw = w["pathname"].as_str().unwrap_or_default();
        let name = if upper {
            raw.to_ascii_uppercase()
        } else {
            raw.to_string()
        };
        if seen.insert(name.clone(), ()).is_some() {
            problems.push(format!("the reference repeats {name}"));
        }
        let Some(g) = got.get(&name) else {
            problems.push(format!("missing here: {name}"));
            continue;
        };
        let mut field = |key: &str, want: &Value, have: Value| {
            if *want != have {
                problems.push(format!("{name}: {key}: {want} and {have}"));
            }
        };
        field("record_type", &w["record_type"], Value::from(g.record_type));
        field("nx", &w["nx"], Value::from(g.nx));
        field("ny", &w["ny"], Value::from(g.ny));
        field(
            "cell_size",
            &w["cell_size"],
            Value::from(g.cell_size.to_bits()),
        );
        field(
            "lower_left_cell_x",
            &w["lower_left_cell_x"],
            Value::from(g.lower_left_cell_x),
        );
        field(
            "lower_left_cell_y",
            &w["lower_left_cell_y"],
            Value::from(g.lower_left_cell_y),
        );
        // In DSS6 an SHG grid is an Albers record holding parameters, not the text DSS7 holds: its
        // CRS text is not compared, as in `import_fixture.rs`. A specified
        // grid's text is compared in both.
        if !(upper && w["grid_type"] == "albers") {
            field("srs_name", &w["srs_name"], Value::from(g.srs_name.clone()));
            // This engine's text of the CRS Vortex states, recorded beside Vortex's in the fixture.
            field(
                "srs_definition",
                &w["engine_srs_definition"],
                Value::from(g.srs_definition.clone()),
            );
        }
        field(
            "data_units",
            &w["data_units"],
            Value::from(g.data_units.clone()),
        );
        field("data_type", &w["data_type"], Value::from(g.data_type));
        field("min", &w["min"], f32_bits(g.min_data_value));
        field("max", &w["max"], f32_bits(g.max_data_value));
        field("mean", &w["mean"], f32_bits(g.mean_data_value));
        field("digest", &w["digest"], Value::from(digest(&g.data)));
        if g.data.len() != g.nx * g.ny || g.data.is_empty() {
            problems.push(format!("{name}: {} cells", g.data.len()));
        }
    }
    for name in got.keys() {
        if !seen.contains_key(name) {
            problems.push(format!("extra here: {name}"));
        }
    }
    problems
}

fn run_case(case: &Value, version: u8) -> BTreeMap<String, Grid> {
    let id = case["id"].as_str().unwrap();
    let out = fresh(&format!("{id}.{version}"));
    let r = request(case, out.clone(), version);
    let written = import_gridded(&r, &mut |_| ControlFlow::Continue(()))
        .unwrap_or_else(|e| panic!("{id}: {e}"));
    let got = read_back(&out, version);
    assert_eq!(written.len(), got.len(), "{id}");
    got
}

#[test]
fn every_case_matches_vortex_exactly_in_dss7_and_holds_the_same_in_dss6() {
    let cases = expected();
    assert_eq!(cases.len(), CASES);
    let (mut records, mut distinct) = (0, std::collections::BTreeSet::new());
    for case in &cases {
        let wanted = case["records"].as_array().unwrap();
        records += wanted.len();
        for w in wanted {
            distinct.insert(w["digest"].as_str().unwrap().to_string());
        }
        for version in [7, 6] {
            let problems = differences(wanted, &run_case(case, version), version == 6);
            assert!(
                problems.is_empty(),
                "{} in DSS{version}: {problems:#?}",
                case["id"]
            );
        }
    }
    assert_eq!(records, RECORDS);
    // The cases do not all hold one grid: 22 distinct cell digests among 25 records.
    assert_eq!(distinct.len(), 22);
}

#[test]
fn the_fixture_covers_each_unit_as_source_and_as_target() {
    let cases = expected();
    let text = |c: &Value| {
        c["records"][0]["engine_srs_definition"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let with = |prefix: &str, unit: &str| {
        cases
            .iter()
            .filter(|c| c["id"].as_str().unwrap().starts_with(prefix) && text(c).contains(unit))
            .count()
    };
    assert_eq!(with("target.utm.", "UNIT[\"Meter\",1]"), 3);
    assert_eq!(with("target.ftus.", "UNIT[\"US survey foot\""), 2);
    assert_eq!(with("target.ft.", "UNIT[\"Foot\",0.3048]"), 2);
    assert_eq!(with("source.utm.asis", "Transverse_Mercator"), 1);
    assert_eq!(with("source.ftus.asis", "UNIT[\"Foot_US\""), 1);
    assert_eq!(with("source.ft.asis", "UNIT[\"Foot\",0.3048]"), 1);
    // And each raster warps to SHG by each method.
    let shg = cases
        .iter()
        .filter(|c| c["id"].as_str().unwrap().contains(".shg."))
        .count();
    assert_eq!(shg, 9);
}

/// Storing a grid whose corner is off whole cells moves it, in Vortex and under `Align::Vortex`
/// alike, and the default refuses it. Vortex's own `hms_cn_grid.tif` moves 14.357 m this way; this
/// file's corner is 736 ft east and 1411 ft south of whole cells, and it moves 287 m.
#[test]
fn a_grid_off_whole_cells_is_refused_by_default_and_moved_only_when_asked() {
    let case = case_named("source.ftus_offset.asis.snapped");
    assert_eq!(case["align"], "vortex");
    // Vortex's placement, which the main test holds this engine to under `Align::Vortex`.
    let stored = &case["records"][0];
    let on_cells = &case_named("source.ftus.asis")["records"][0];
    assert_eq!(stored["lower_left_cell_x"], on_cells["lower_left_cell_x"]);
    assert_eq!(stored["lower_left_cell_x"], -323);
    assert_eq!(
        (
            stored["lower_left_cell_y"].clone(),
            on_cells["lower_left_cell_y"].clone()
        ),
        (1032.into(), 1033.into())
    );
    // The file's lower-left corner, and where the record puts it, in US survey feet.
    let (file_x, file_y) = (-646_000.0 + 736.0, 2_124_000.0 - 1411.0 - 29.0 * 2000.0);
    let (moved_x, moved_y): (f64, f64) = (-323.0 * 2000.0 - file_x, 1032.0 * 2000.0 - file_y);
    assert_eq!((moved_x, moved_y), (-736.0, -589.0));
    let metres = moved_x.hypot(moved_y) * 1200.0 / 3937.0;
    assert!((metres - 287.33).abs() < 0.01, "{metres}");

    // By default: refused, naming the corner, with no file left.
    for version in [7, 6] {
        let out = fresh(&format!("offset.default.{version}"));
        let mut r = request(&case, out.clone(), version);
        r.align = Align::Refuse;
        let error = import_gridded(&r, &mut |_| ControlFlow::Continue(())).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("refused") && text.contains("-645264") && text.contains("whole cells"),
            "{text}"
        );
        assert!(!out.exists(), "a refused import left a file");
    }
    // The same file on whole cells is written by default, where it stands.
    assert_eq!(case_named("source.ftus.asis")["align"], "refuse");
}

/// A cell size is metres. On a US survey foot zone the default uses the zone's own foot, and only
/// `Align::Vortex` uses Vortex's international foot: the two requests give different grids.
#[test]
fn a_cell_size_on_a_us_survey_foot_target_follows_align() {
    let case = case_named("target.ftus.vortex_rule");
    // Vortex: 6096 m is 20,000 international feet, stored exactly.
    assert_eq!(case["records"][0]["cell_size"], 20000.0f32.to_bits());
    // By default 6096 m is 19,999.96 US survey feet, the zone's own. The same envelope then holds
    // 9 by 6 of those cells where Vortex's 20,000 ft cells make 8 by 5: another grid, as asked.
    let out = fresh("ftus.default");
    let mut r = request(&case, out.clone(), 7);
    r.align = Align::Refuse;
    import_gridded(&r, &mut |_| ControlFlow::Continue(())).unwrap();
    let got = read_back(&out, 7);
    let grid = got.values().next().unwrap();
    assert_eq!(
        grid.cell_size.to_bits(),
        nearest_f32(6096, 3937, 1200).to_bits()
    );
    assert_eq!(grid.cell_size, 19999.96);
    assert_eq!((grid.nx, grid.ny), (9, 6));
    assert_eq!(
        (
            case["records"][0]["nx"].clone(),
            case["records"][0]["ny"].clone()
        ),
        (8.into(), 5.into())
    );
    // 1200 m is 3,937 US survey feet exactly. On an envelope of whole such cells the default
    // writes the grid asked for: this is arithmetic, not Vortex's result, which has 3,937.008 ft cells.
    let out = fresh("ftus.own_unit");
    let mut r = request(&case, out.clone(), 7);
    r.align = Align::Refuse;
    r.cell_size = Some(1200.0);
    let Bounds::Envelope { envelope, .. } = &mut r.bounds else {
        panic!("the case has an envelope")
    };
    *envelope = Envelope {
        min_x: 3937.0 * 585.0,
        max_x: 3937.0 * 625.0,
        min_y: 3937.0 * 498.0,
        max_y: 3937.0 * 523.0,
    };
    import_gridded(&r, &mut |_| ControlFlow::Continue(())).unwrap();
    let got = read_back(&out, 7);
    let grid = got.values().next().unwrap();
    assert_eq!((grid.nx, grid.ny), (40, 25));
    assert_eq!(grid.cell_size, 3937.0);
    assert_eq!((grid.lower_left_cell_x, grid.lower_left_cell_y), (585, 498));
    assert!(grid
        .srs_definition
        .contains("UNIT[\"US survey foot\",0.304800609601219]"));
    // Every cell lies inside the source, and they are not one value.
    assert_eq!(grid.data.iter().filter(|c| c.is_finite()).count(), 1000);
    assert!(grid.max_data_value > grid.min_data_value);
}

// --- Negative controls, through the same comparison and real output.

#[test]
fn the_comparison_fails_on_each_field_and_on_one_cell_bit() {
    let case = case_named("target.ftus.vortex_rule.average");
    let wanted = case["records"].as_array().unwrap().clone();
    let got = run_case(&case, 7);
    assert!(differences(&wanted, &got, false).is_empty());
    let name = wanted[0]["pathname"].as_str().unwrap().to_string();
    type Change = fn(&mut Grid);
    let changes: [(&str, Change); 9] = [
        ("nx", |g| g.nx += 1),
        ("cell_size", |g| {
            g.cell_size = f32::from_bits(g.cell_size.to_bits() + 1)
        }),
        ("lower_left_cell_x", |g| g.lower_left_cell_x += 1),
        ("lower_left_cell_y", |g| g.lower_left_cell_y -= 1),
        ("srs_name", |g| g.srs_name.push('x')),
        // A foot relabelled as a metre, the earlier defect.
        ("srs_definition", |g| {
            g.srs_definition = g.srs_definition.replace(
                "UNIT[\"US survey foot\",0.304800609601219]",
                "UNIT[\"Meter\",1]",
            )
        }),
        ("data_units", |g| g.data_units = "IN".into()),
        ("mean", |g| g.mean_data_value = Some(1.0e6)),
        ("digest", |g| {
            let c = &mut g.data[3];
            *c = f32::from_bits(c.to_bits() ^ 1);
        }),
    ];
    for (key, change) in changes {
        let mut changed = got.clone();
        change(changed.get_mut(&name).unwrap());
        let found = differences(&wanted, &changed, false);
        assert!(found.iter().any(|p| p.contains(key)), "{key}: {found:?}");
    }
    assert!(differences(&wanted[1..], &got, false)
        .iter()
        .any(|p| p.starts_with("extra here")));
    assert!(!differences(&[], &got, false).is_empty());
    assert!(!differences(&wanted, &BTreeMap::new(), false).is_empty());
    // Another case's records are not this one's.
    let other = run_case(&case_named("target.ft.near"), 7);
    assert!(!differences(&wanted, &other, false).is_empty());
}

/// The `f32` nearest `metres * num / den`, by exact integer arithmetic: independent of how the
/// library converts. Ties do not occur for the sizes used here.
fn nearest_f32(metres: u128, num: u128, den: u128) -> f32 {
    // value = metres * num / den. Search the positive f32s, whose bit patterns order as they do.
    let (mut low, mut high) = (0u32, f32::MAX.to_bits());
    let below = |bits: u32| {
        // f32 = m * 2^e exactly, with m an integer: compare m * 2^e * den with metres * num.
        let v = f32::from_bits(bits);
        let e = ((bits >> 23) & 0xff) as i32 - 150;
        let m = u128::from((bits & 0x7f_ffff) | 0x80_0000);
        assert!(v.is_normal());
        if e >= 0 {
            (m << e) * den <= metres * num
        } else {
            m * den <= (metres * num) << (-e)
        }
    };
    low = low.max(1.0f32.to_bits());
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        if below(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    // `low` is at or below the value and `high` above it: the nearer of the two, in exact terms.
    let exact = |bits: u32| {
        let e = ((bits >> 23) & 0xff) as i32 - 150;
        let m = u128::from((bits & 0x7f_ffff) | 0x80_0000);
        assert!(e < 0);
        // Scaled by den * 2^-e so everything is an integer.
        (m * den, (metres * num) << (-e))
    };
    let (lo, want) = exact(low);
    let (hi, _) = exact(high);
    if want - lo <= hi - want {
        f32::from_bits(low)
    } else {
        f32::from_bits(high)
    }
}

/// Regression: with the CRS's own foot, `cell_size=2000` on a US survey foot zone and
/// on an international foot zone was refused as off whole cells, because the lattice was built on
/// the converted `f64` and the record checked against its `f32`. The lattice is now built on the
/// spacing the record stores, so 2,000 m, 1,200 m and 10,000 m are written at any origin, on cells
/// whose size and placement are those of an independent calculation.
#[test]
fn ordinary_cell_sizes_on_foot_targets_are_written_by_default_at_any_origin() {
    let zones = [
        // (case holding the target, metres to feet as a ratio, its envelope)
        (
            "target.ftus.vortex_rule",
            (3937u128, 1200u128),
            [2_300_000.0, 2_460_000.0, 1_960_000.0, 2_060_000.0],
        ),
        (
            "target.ft.bilinear",
            (1250, 381),
            [620_000.0, 780_000.0, 1_780_000.0, 1_850_000.0],
        ),
    ];
    let origins = [
        (0.0, 0.0),
        (12_345.5, -777.25),
        (-3.0, 41_111.0),
        (20_001.0, 19_999.0),
    ];
    let mut written = 0;
    for (id, (num, den), base) in zones {
        let case = case_named(id);
        for metres in [2000u32, 1200, 10000] {
            let cell = nearest_f32(u128::from(metres), num, den);
            // Within a part in 16 million of the size asked for, and the nearest a record holds.
            let stored_metres = f64::from(cell) * den as f64 / num as f64;
            assert!((stored_metres - f64::from(metres)).abs() < f64::from(metres) * 6.0e-8);
            for (dx, dy) in origins {
                let out = fresh(&format!("{id}.{metres}"));
                let mut r = request(&case, out.clone(), 7);
                r.align = Align::Refuse;
                r.cell_size = Some(f64::from(metres));
                let e = Envelope {
                    min_x: base[0] + dx,
                    max_x: base[1] + dx,
                    min_y: base[2] + dy,
                    max_y: base[3] + dy,
                };
                r.bounds = Bounds::Envelope {
                    envelope: e,
                    crs: Some(Crs::Wkt(case["envelope_wkt"].as_str().unwrap().to_string())),
                };
                import_gridded(&r, &mut |_| ControlFlow::Continue(()))
                    .unwrap_or_else(|err| panic!("{id} {metres} m at ({dx}, {dy}): {err}"));
                let got = read_back(&out, 7);
                let grid = got.values().next().unwrap();
                assert_eq!(grid.cell_size.to_bits(), cell.to_bits(), "{id} {metres}");
                // The envelope snapped outwards to whole cells of that size.
                let c = f64::from(cell);
                let (x0, x1) = ((e.min_x / c).floor(), (e.max_x / c).ceil());
                let (y0, y1) = ((e.min_y / c).floor(), (e.max_y / c).ceil());
                assert_eq!(
                    (grid.lower_left_cell_x, grid.lower_left_cell_y),
                    (x0 as i32, y0 as i32),
                    "{id} {metres} m at ({dx}, {dy})"
                );
                assert_eq!((grid.nx, grid.ny), ((x1 - x0) as usize, (y1 - y0) as usize));
                written += 1;
            }
        }
    }
    assert_eq!(written, 24);
    // The sizes themselves, stated: 2,000 m is 6561.6665 US survey feet and 6561.68 international.
    assert_eq!(
        nearest_f32(2000, 3937, 1200).to_bits(),
        6561.6665f32.to_bits()
    );
    assert_eq!(nearest_f32(1200, 3937, 1200), 3937.0);
    assert_eq!(nearest_f32(10000, 3937, 1200), 32808.332);
    assert_eq!(nearest_f32(3048, 1250, 381), 10000.0);
}

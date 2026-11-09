//! Importer cases with no oracle at run time, so CI on every platform checks the same bits.
//!
//! `fixtures/public/vortex_p46/` holds synthetic GRIB2 inputs and, in `expected.json`, every record
//! Vortex 1.1.0's importer wrote from them: pathnames with
//! their D and E parts, data types, units, placement, CRS, statistics and a digest of every cell.
//! This test runs the same cases and compares through one function, which negative controls below
//! prove can fail. DSS6 repeats each case: Vortex 1.x cannot write DSS6, so there only the pathname
//! case differs.

use dss_core::dss6::Dss6File;
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::import::{import_gridded, Bounds, ImportParts, ImportRequest, Selection};
use dss_vortex::warp::Method;
use dss_vortex::{Crs, DataType};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const CASES: usize = 22;
const RECORDS: usize = 40;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/vortex_p46")
}

fn expected() -> Value {
    serde_json::from_str(&fs::read_to_string(dir().join("expected.json")).unwrap()).unwrap()
}

fn fresh(name: &str) -> PathBuf {
    // Tests run at once, and several run the same case.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = format!("{name}.{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("import_fixture")
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

fn data_type_named(text: &str) -> DataType {
    match text {
        "INST-VAL" => DataType::InstVal,
        "PER-AVER" => DataType::PerAver,
        "PER-CUM" => DataType::PerCum,
        "INST-CUM" => DataType::InstCum,
        other => panic!("{other}"),
    }
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
        _ => Method::Average,
    };
    let selection = Selection::Variable(case["variable"].as_str().unwrap().to_string());
    let mut r = ImportRequest::new(inputs, output, selection, method);
    let e: Vec<f64> = case["envelope"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    r.bounds = Bounds::Envelope {
        envelope: Envelope {
            min_x: e[0],
            max_x: e[1],
            min_y: e[2],
            max_y: e[3],
        },
        crs: Some(Crs::Wkt(case["envelope_wkt"].as_str().unwrap().to_string())),
    };
    r.target = Some(Crs::Named(dss_vortex::NamedCrs::Shg));
    r.cell_size = Some(case["cell"].as_f64().unwrap());
    let w = &case["write"];
    let part = |key: &str| w[key].as_str().unwrap_or_default().to_string();
    r.parts = ImportParts {
        a: part("partA"),
        b: part("partB"),
        c: w["partC"].as_str().map(str::to_string),
        f: part("partF"),
    };
    r.units = w["units"].as_str().map(str::to_string);
    r.data_type = w["dataType"].as_str().map(data_type_named);
    r.version = Some(version);
    r
}

// Everything stored, by pathname. A repeated pathname fails here: nothing could be compared by name.
fn read_back(path: &Path, version: u8) -> Result<BTreeMap<String, Grid>, String> {
    let mut out = BTreeMap::new();
    let mut add = |name: String, grid: Grid| -> Result<(), String> {
        match out.insert(name.clone(), grid) {
            None => Ok(()),
            Some(_) => Err(format!("{name} is stored twice")),
        }
    };
    if version == 6 {
        let file = Dss6File::open(path).map_err(|e| e.to_string())?;
        let catalog = file.catalog().map_err(|e| e.to_string())?;
        for e in &catalog.entries {
            let grid = file.read_grid(&e.pathname).map_err(|x| x.to_string())?;
            add(String::from_utf8_lossy(&e.pathname).into_owned(), grid)?;
        }
    } else {
        let mut file = Dss7File::open(path).map_err(|e| e.to_string())?;
        let catalog = file.catalog().map_err(|e| e.to_string())?;
        for e in &catalog.catalog.entries {
            let grid = file.read_grid(&e.pathname).map_err(|x| x.to_string())?;
            add(String::from_utf8_lossy(&e.pathname).into_owned(), grid)?;
        }
    }
    Ok(out)
}

fn f32_bits(v: Option<f32>) -> Value {
    v.map_or(Value::Null, |x| Value::from(x.to_bits()))
}

/// Every difference between what Vortex wrote and what a file holds. Empty means identical. `upper`
/// is DSS6: pathnames are upper case, and an Albers record holds parameters where DSS7 holds WKT
/// text, so the CRS text is not compared here (Vortex 1.x cannot write DSS6).
fn differences(wanted: &[Value], got: &BTreeMap<String, Grid>, upper: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if wanted.is_empty() {
        problems.push("the reference holds no records: nothing is compared".to_string());
    }
    if got.is_empty() {
        problems.push("the file holds no records: nothing is compared".to_string());
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
        let mut field = |key: &str, have: Value| {
            if w[key] != have {
                problems.push(format!("{name}: {key}: {} and {have}", w[key]));
            }
        };
        field("record_type", Value::from(g.record_type));
        field("nx", Value::from(g.nx));
        field("ny", Value::from(g.ny));
        field("cell_size", Value::from(g.cell_size.to_bits()));
        field("lower_left_cell_x", Value::from(g.lower_left_cell_x));
        field("lower_left_cell_y", Value::from(g.lower_left_cell_y));
        if !upper {
            field("srs_name", Value::from(g.srs_name.clone()));
            field("srs_definition", Value::from(g.srs_definition.clone()));
        }
        field("data_units", Value::from(g.data_units.clone()));
        field("data_type", Value::from(g.data_type));
        field("min", f32_bits(g.min_data_value));
        field("max", f32_bits(g.max_data_value));
        field("mean", f32_bits(g.mean_data_value));
        field("digest", Value::from(digest(&g.data)));
        if g.data.len() != g.nx * g.ny || g.data.is_empty() {
            problems.push(format!(
                "{name}: {} cells for {} x {}",
                g.data.len(),
                g.nx,
                g.ny
            ));
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
    let mut seen = 0;
    let written = import_gridded(&r, &mut |_| {
        seen += 1;
        ControlFlow::Continue(())
    })
    .unwrap_or_else(|e| panic!("{id}: {e}"));
    let got = read_back(&out, version).unwrap_or_else(|e| panic!("{id}: {e}"));
    assert_eq!(written.len(), got.len(), "{id}");
    assert_eq!(seen, got.len(), "{id}: progress calls");
    got
}

#[test]
fn every_case_matches_vortex_exactly_in_dss7() {
    let all = expected();
    let cases = all["cases"].as_array().unwrap();
    assert_eq!(cases.len(), CASES);
    let mut records = 0;
    for case in cases {
        let wanted = case["records"].as_array().unwrap();
        records += wanted.len();
        let got = run_case(case, 7);
        let problems = differences(wanted, &got, false);
        assert!(problems.is_empty(), "{}: {problems:#?}", case["id"]);
    }
    assert_eq!(records, RECORDS);
}

#[test]
fn every_case_holds_the_same_record_in_dss6() {
    let all = expected();
    for case in all["cases"].as_array().unwrap() {
        if case["id"] == "write.inst" {
            let r = request(case, fresh("write.inst.6"), 6);
            let e = import_gridded(&r, &mut |_| ControlFlow::Continue(())).unwrap_err();
            assert!(e.to_string().contains("in DSS6"), "{e}");
            continue;
        }
        let got = run_case(case, 6);
        let problems = differences(case["records"].as_array().unwrap(), &got, true);
        assert!(problems.is_empty(), "{}: {problems:#?}", case["id"]);
    }
}

// The stamps HEC-HMS reads, stated here literally so a change to the fixture cannot move them.
#[test]
fn period_stamps_are_those_vortex_writes() {
    let all = expected();
    let names = |id: &str| -> Vec<String> {
        let case = all["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("{id}"));
        run_case(case, 7).into_keys().collect()
    };
    assert_eq!(
        names("inputs.apcp.bilinear"),
        [
            "/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/",
            "/SHG/P46/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/CI/",
        ]
    );
    assert_eq!(
        names("inputs.mrms.bilinear"),
        [
            "/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/",
            "/SHG/P46/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/CI/",
        ]
    );
    assert_eq!(
        names("instant.tmp.bilinear"),
        ["/SHG/P46/TEMPERATURE/15JUN2024:1200//CI/"]
    );
    assert_eq!(
        names("rule.hrrr.bilinear"),
        ["/SHG/P46/TEMPERATURE/15JUN2024:0600/15JUN2024:0700/CI/"]
    );
}

// --- Negative controls, through the same comparison and real output.

fn sample() -> (Vec<Value>, BTreeMap<String, Grid>) {
    let all = expected();
    let case = all["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "joined.apcp.bilinear")
        .unwrap()
        .clone();
    let got = run_case(&case, 7);
    let wanted = case["records"].as_array().unwrap().clone();
    assert!(
        differences(&wanted, &got, false).is_empty(),
        "the control's baseline must pass"
    );
    (wanted, got)
}

#[test]
fn an_extra_or_missing_record_either_side_fails() {
    let (wanted, got) = sample();
    assert!(differences(&wanted[1..], &got, false)
        .iter()
        .any(|p| p.starts_with("extra here")));
    let mut short = got.clone();
    let first = short.keys().next().unwrap().clone();
    short.remove(&first);
    assert!(differences(&wanted, &short, false)
        .iter()
        .any(|p| p.starts_with("missing here")));
}

#[test]
fn empty_sides_and_a_repeated_record_fail() {
    let (wanted, got) = sample();
    assert!(!differences(&[], &got, false).is_empty());
    assert!(!differences(&wanted, &BTreeMap::new(), false).is_empty());
    assert!(!differences(&[], &BTreeMap::new(), false).is_empty());
    let mut twice = wanted.clone();
    twice.push(wanted[0].clone());
    assert!(differences(&twice, &got, false)
        .iter()
        .any(|p| p.contains("repeats")));
}

#[test]
fn a_record_under_another_window_fails() {
    let (mut wanted, got) = sample();
    // The same cells under another hour: an E part one hour on.
    let name = wanted[0]["pathname"]
        .as_str()
        .unwrap()
        .replace(":2300/", ":2359/");
    wanted[0]["pathname"] = Value::from(name);
    assert!(!differences(&wanted, &got, false).is_empty());
    // Two records' cells exchanged, with their names kept.
    let (mut wanted, mut got) = sample();
    let names: Vec<String> = got.keys().cloned().collect();
    let a = got.remove(&names[0]).unwrap();
    let b = got.remove(&names[1]).unwrap();
    got.insert(names[0].clone(), b);
    got.insert(names[1].clone(), a);
    let found = differences(&wanted, &got, false);
    assert!(found.iter().any(|p| p.contains("digest")), "{found:?}");
    // Windows differ although every other field agrees.
    wanted.truncate(1);
    assert!(!differences(&wanted, &got, false).is_empty());
}

#[test]
fn every_metadata_field_can_fail() {
    let (wanted, got) = sample();
    let name = wanted[0]["pathname"].as_str().unwrap().to_string();
    type Change = fn(&mut Grid);
    let changes: [(&str, Change); 13] = [
        ("record_type", |g| g.record_type += 1),
        ("nx", |g| g.nx += 1),
        ("ny", |g| g.ny += 1),
        ("cell_size", |g| {
            g.cell_size = f32::from_bits(g.cell_size.to_bits() + 1)
        }),
        ("lower_left_cell_x", |g| g.lower_left_cell_x += 1),
        ("lower_left_cell_y", |g| g.lower_left_cell_y -= 1),
        ("srs_name", |g| g.srs_name.push('x')),
        ("srs_definition", |g| g.srs_definition.push('x')),
        ("data_units", |g| g.data_units = "IN".into()),
        ("data_type", |g| g.data_type = 0),
        ("max", |g| g.max_data_value = Some(1.0e6)),
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
    // A short cell array.
    let mut changed = got.clone();
    changed.get_mut(&name).unwrap().data.pop();
    assert!(differences(&wanted, &changed, false)
        .iter()
        .any(|p| p.contains("cells for")));
}

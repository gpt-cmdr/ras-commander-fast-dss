//! The Zarr source against what xarray 2026.9.0 and zarr-python 3.4.2 read from the same stores
//! (`oracle/vortex/make_zarr_fixtures.py record`): cells in bits after scale and offset, mask,
//! stored items, times and grid. The real AORC v1.1 chunks are in `aorc_2019_subset.zarr`.

mod zarr_support;

use dss_vortex::source::zarr::{Window, ZarrStore};
use dss_vortex::VortexError;
use serde_json::Value;
use std::fs;
use zarr_support::*;

/// Cases `expected.json` must hold. A file with fewer would pass a loop over them.
const CASES: usize = 28;
const AORC_CASES: usize = 5;

fn expected() -> Value {
    let text = fs::read_to_string(fixtures().join("expected.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn pair(case: &Value, key: &str) -> (usize, usize) {
    let list = case[key].as_array().unwrap();
    (
        list[0].as_u64().unwrap() as usize,
        list[1].as_u64().unwrap() as usize,
    )
}

/// What a case reads, shifted by `(steps, rows, columns)` for the negative controls.
struct Read {
    cells: Vec<f32>,
    stored: Result<Vec<u8>, VortexError>,
    instants: Vec<Option<i64>>,
    transform: [f64; 6],
    nx: usize,
    ny: usize,
    accumulation: Option<i64>,
    units: String,
}

fn read(case: &Value, shift: (usize, usize, usize)) -> Read {
    let store = ZarrStore::open(&fixtures().join(case["store"].as_str().unwrap())).unwrap();
    let v = store.variable(case["variable"].as_str().unwrap()).unwrap();
    let (s0, s1) = pair(case, "steps");
    let (r0, r1) = pair(case, "rows");
    let (c0, c1) = pair(case, "cols");
    let steps = s0 + shift.0..s1 + shift.0;
    let window = Window {
        rows: r0 + shift.1..r1 + shift.1,
        cols: c0 + shift.2..c1 + shift.2,
    };
    let (nx, ny, t) = v.grid(&window).unwrap();
    Read {
        cells: v.read(steps.clone(), &window).unwrap(),
        stored: v.read_stored(steps.clone(), &window),
        instants: steps.clone().map(|s| v.instant(s).unwrap()).collect(),
        transform: [
            t.origin_x,
            t.dx,
            t.row_rotation,
            t.origin_y,
            t.column_rotation,
            t.dy,
        ],
        nx,
        ny,
        accumulation: v.stated_accumulation(),
        units: v.units().to_string(),
    }
}

/// Every way a read differs from its case. Empty means equal.
fn differences(case: &Value, got: &Read) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |name: &str, same: bool| {
        if !same {
            out.push(name.to_string());
        }
    };
    let (s0, s1) = pair(case, "steps");
    let cells = (s1 - s0) * got.nx * got.ny;
    check("cell count", got.cells.len() == cells && cells > 0);
    check(
        "shape",
        (got.nx as u64, got.ny as u64)
            == (case["nx"].as_u64().unwrap(), case["ny"].as_u64().unwrap()),
    );
    check(
        "cells",
        sha256(&cells_bytes(&got.cells)) == case["cells_sha256"].as_str().unwrap(),
    );
    let valid = got.cells.iter().filter(|c| c.is_finite()).count() as u64;
    let missing = got.cells.iter().filter(|c| c.is_nan()).count() as u64;
    check("valid count", valid == case["valid"].as_u64().unwrap());
    check(
        "missing count",
        missing == case["missing"].as_u64().unwrap(),
    );
    check(
        "every cell is valid or missing",
        valid + missing == cells as u64,
    );
    for sample in case["samples"].as_array().unwrap() {
        let at = sample[0].as_u64().unwrap() as usize;
        let bits = u32::from_str_radix(sample[1].as_str().unwrap(), 16).unwrap();
        check(
            "sample cell",
            got.cells.get(at).map(|c| c.to_bits()) == Some(bits),
        );
    }
    let extreme = |pick: fn(f32, f32) -> f32| {
        got.cells
            .iter()
            .copied()
            .filter(|c| c.is_finite())
            .reduce(pick)
            .map(|v| format!("{:08x}", v.to_bits()))
    };
    check(
        "minimum",
        extreme(f32::min).as_deref() == case["min_bits"].as_str(),
    );
    check(
        "maximum",
        extreme(f32::max).as_deref() == case["max_bits"].as_str(),
    );
    match (case["stored_sha256"].as_str(), &got.stored) {
        (Some(want), Ok(stored)) => check("stored items", sha256(stored) == want),
        // Recorded as null where a chunk has no file: the engine has no stored bytes either.
        (None, Err(_)) => {}
        _ => check("stored items", false),
    }
    let instants: Vec<Option<i64>> = case["instants"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_i64)
        .collect();
    check("instants", instants == got.instants && !instants.is_empty());
    let transform: Vec<String> = got
        .transform
        .iter()
        .map(|v| format!("{:016x}", v.to_bits()))
        .collect();
    let want: Vec<&str> = case["transform_bits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    check("transform", transform == want);
    check(
        "accumulation",
        got.accumulation == case["accumulation"].as_i64(),
    );
    check("units", got.units == case["units"].as_str().unwrap());
    out
}

#[test]
fn every_case_reads_as_xarray_does() {
    let expected = expected();
    let cases = expected["cases"].as_array().unwrap();
    assert_eq!(cases.len(), CASES);
    let mut cells = 0usize;
    let mut aorc = 0usize;
    let mut refused = 0usize;
    for case in cases {
        // One case xarray reads and this reader refuses. Its `_FillValue` is the JSON double
        // -1e30 on a `>f4` array, whose nearest stored value is -1.0000000150474662e30. xarray
        // compares in float64, finds them unequal, and keeps the cell as a value. Its value as a
        // cell is the missing cells' marker, so a record would hold it as missing: refused.
        if case["variable"] == "v_f4be" && pair(case, "steps") == (0, 5) {
            let store = ZarrStore::open(&fixtures().join("syn_types.zarr")).unwrap();
            let v = store.variable("v_f4be").unwrap();
            let error = v.read(0..5, &v.full_window()).unwrap_err().to_string();
            assert!(
                error.contains("the marker its missing cells have"),
                "{error}"
            );
            assert_eq!(case["missing"], 0, "xarray masks nothing in it");
            refused += 1;
            continue;
        }
        let got = read(case, (0, 0, 0));
        let found = differences(case, &got);
        assert!(
            found.is_empty(),
            "{} {}: {found:?}",
            case["store"],
            case["variable"]
        );
        cells += got.cells.len();
        aorc += usize::from(case["store"] == "aorc_2019_subset.zarr");
    }
    assert_eq!(refused, 1);
    assert_eq!(aorc, AORC_CASES);
    // The real chunks: one whole one of 144 x 128 x 256, and the windows.
    assert_eq!(cells, 6_566_701, "cells compared");
}

/// The gate must fail when the read is another step, row or column, and when a cell or the mask
/// changes. Without these, a comparison that hashed nothing would pass.
#[test]
fn the_comparison_fails_on_a_shifted_or_changed_read() {
    let expected = expected();
    let cases = expected["cases"].as_array().unwrap();
    // The Texas Panhandle window: steps, rows and columns all have room to shift by one.
    let case = cases
        .iter()
        .find(|c| c["store"] == "aorc_2019_subset.zarr" && c["nx"] == 157)
        .unwrap();
    assert!(differences(case, &read(case, (0, 0, 0))).is_empty());
    for (shift, name) in [
        ((1, 0, 0), "a step later"),
        ((0, 1, 0), "a row north"),
        ((0, 0, 1), "a column east"),
    ] {
        let found = differences(case, &read(case, shift));
        assert!(found.contains(&"cells".to_string()), "{name}: {found:?}");
    }
    let found = differences(case, &read(case, (1, 0, 0)));
    assert!(found.contains(&"instants".to_string()), "{found:?}");
    let found = differences(case, &read(case, (0, 1, 1)));
    assert!(found.contains(&"transform".to_string()), "{found:?}");

    let mut changed = read(case, (0, 0, 0));
    let at = changed.cells.iter().position(|c| *c > 0.0).unwrap();
    changed.cells[at] = f32::from_bits(changed.cells[at].to_bits() + 1);
    assert_eq!(differences(case, &changed), ["cells"]);
    let mut masked = read(case, (0, 0, 0));
    masked.cells[at] = f32::NAN;
    let found = differences(case, &masked);
    assert!(found.contains(&"missing count".to_string()), "{found:?}");
    let mut short = read(case, (0, 0, 0));
    short.cells.pop();
    assert!(differences(case, &short).contains(&"cell count".to_string()));
    let mut empty = read(case, (0, 0, 0));
    empty.cells.clear();
    assert!(differences(case, &empty).contains(&"cell count".to_string()));

    // A coastal case: unmasking one sea cell is seen.
    let coast = cases
        .iter()
        .find(|c| c["missing"].as_u64() > Some(0) && c["valid"].as_u64() > Some(100_000))
        .unwrap();
    let mut unmasked = read(coast, (0, 0, 0));
    let sea = unmasked.cells.iter().position(|c| c.is_nan()).unwrap();
    unmasked.cells[sea] = 0.0;
    let found = differences(coast, &unmasked);
    assert!(found.contains(&"missing count".to_string()), "{found:?}");
}

/// `MANIFEST.sha256` names every fixture file with its size and hash, and nothing else.
fn manifest_differences(manifest: &str) -> Vec<String> {
    let root = fixtures();
    let mut out = Vec::new();
    let mut listed = Vec::new();
    for line in manifest.lines() {
        let mut parts = line.splitn(3, "  ");
        let (Some(hash), Some(size), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            out.push(format!("malformed line {line:?}"));
            continue;
        };
        if listed.contains(&name.to_string()) {
            out.push(format!("{name}: listed twice"));
        }
        listed.push(name.to_string());
        match fs::read(root.join(name)) {
            Ok(bytes) => {
                if bytes.len().to_string() != size || sha256(&bytes) != hash {
                    out.push(format!("{name}: its bytes differ from the manifest"));
                }
            }
            Err(_) => out.push(format!("{name}: listed and absent")),
        }
    }
    for name in files_under(&root) {
        if name != "MANIFEST.sha256" && !listed.contains(&name) {
            out.push(format!("{name}: present and not listed"));
        }
    }
    if listed.is_empty() {
        out.push("the manifest lists nothing".into());
    }
    out
}

#[test]
fn the_manifest_names_every_fixture_file() {
    let manifest = fs::read_to_string(fixtures().join("MANIFEST.sha256")).unwrap();
    assert!(!manifest.contains('\r'), "the manifest has CRLF line ends");
    assert_eq!(manifest_differences(&manifest), Vec::<String>::new());
    assert_eq!(manifest.lines().count(), 348);

    // Negative controls: a missing row, an extra row, a repeated row, a wrong hash, nothing.
    let lines: Vec<&str> = manifest.lines().collect();
    let without_first = lines[1..].join("\n");
    assert!(manifest_differences(&without_first)[0].contains("present and not listed"));
    let extra = format!("{manifest}{}  1  no_such_file\n", "0".repeat(64));
    assert!(manifest_differences(&extra)[0].contains("listed and absent"));
    let twice = format!("{manifest}{}\n", lines[0]);
    assert!(manifest_differences(&twice)[0].contains("listed twice"));
    let flipped = manifest.replacen(
        &lines[0][..1],
        if &lines[0][..1] == "0" { "1" } else { "0" },
        1,
    );
    assert!(manifest_differences(&flipped)[0].contains("differ from the manifest"));
    assert!(manifest_differences("")
        .iter()
        .any(|d| d.contains("lists nothing")));
}

/// Metadata is text, and Git must not rewrite its line ends: the manifest's hashes are of LF files.
#[test]
fn fixture_metadata_has_no_crlf() {
    let root = fixtures();
    let mut checked = 0;
    for name in files_under(&root) {
        let leaf = name.rsplit('/').next().unwrap();
        if leaf.starts_with(".z") || leaf.ends_with(".json") {
            let bytes = fs::read(root.join(&name)).unwrap();
            assert!(!bytes.contains(&b'\r'), "{name} has a carriage return");
            checked += 1;
        }
    }
    assert!(checked >= 40, "{checked} metadata files");
    let attributes = fs::read_to_string(root.join("../../../.gitattributes")).unwrap_or_default();
    assert!(attributes.contains("fixtures/public/zarr/** -text"));
}

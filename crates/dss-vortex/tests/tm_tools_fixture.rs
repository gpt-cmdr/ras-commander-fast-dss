//! Transverse Mercator through the grid tools, grid to point and the time tools, with no oracle at
//! run time. The importer's routes are in `tm_fixture.rs`.
//!
//! `fixtures/public/vortex_p53b/expected_tools.json` holds what Vortex 1.1.0 stored in thirteen
//! matrix cases: the calculator and
//! the clipper on a UTM grid and on a State Plane grid in US survey feet; grid to point with zones
//! in WGS84 UTM, NAD83 UTM and US survey feet over an SHG grid; grid to point on the UTM and US
//! survey foot grids themselves (`zonal_tm_grid`), with zones in each grid's CRS and in lon/lat;
//! and the time shifter and resampler on a UTM grid. Every value is Vortex's. Where Vortex stores
//! GDAL's wording of a CRS, this engine's text is recorded beside it, accepted only when
//! rasterio reads both as one CRS.

use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::{DatumPolicy, Envelope};
use dss_vortex::grid_tools::{calculate, clip, ClipBounds, ClipRequest, Operation, OutputOptions};
use dss_vortex::sink_dss::Parts;
use dss_vortex::temporal::{resample_times, shift_times, Rule, TemporalRequest};
use dss_vortex::warp::Align;
use dss_vortex::zonal::{self, Compat, ZonalRequest, ZoneLimits, MISSING};
use dss_vortex::Crs;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/vortex_p53b")
}

fn expected() -> Value {
    serde_json::from_str(&fs::read_to_string(dir().join("expected_tools.json")).unwrap()).unwrap()
}

fn fresh(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = format!("{name}.{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("tm_tools_fixture")
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

fn read(path: &Path) -> BTreeMap<String, Grid> {
    let mut file = Dss7File::open(path).unwrap();
    let mut out = BTreeMap::new();
    for e in &file.catalog().unwrap().catalog.entries {
        let name = String::from_utf8_lossy(&e.pathname).into_owned();
        assert!(out
            .insert(name, file.read_grid(&e.pathname).unwrap())
            .is_none());
    }
    out
}

/// Every difference between one file and a case's records. Empty means identical.
fn differences(case: &Value, grids: &BTreeMap<String, Grid>) -> Vec<String> {
    let want = case["records"].as_array().unwrap();
    let mut out = Vec::new();
    if want.is_empty() {
        out.push("the case lists no record".to_string());
    }
    if grids.len() != want.len() {
        out.push(format!("{} records for {}", grids.len(), want.len()));
    }
    for w in want {
        let name = w["pathname"].as_str().unwrap();
        let Some(g) = grids.get(name) else {
            out.push(format!("no {name}"));
            continue;
        };
        let bits = |v: Option<f32>| v.map_or(Value::Null, |x| Value::from(x.to_bits()));
        let checks: [(&str, Value); 14] = [
            ("record_type", Value::from(g.record_type)),
            ("data_type", Value::from(g.data_type)),
            ("nx", Value::from(g.nx)),
            ("ny", Value::from(g.ny)),
            ("llx", Value::from(g.lower_left_cell_x)),
            ("lly", Value::from(g.lower_left_cell_y)),
            ("units", Value::from(g.data_units.clone())),
            ("cell_bits", Value::from(g.cell_size.to_bits())),
            ("min_bits", bits(g.min_data_value)),
            ("max_bits", bits(g.max_data_value)),
            ("mean_bits", bits(g.mean_data_value)),
            ("srs_name", Value::from(g.srs_name.clone())),
            (
                "engine_srs_definition",
                Value::from(g.srs_definition.clone()),
            ),
            ("digest", Value::from(digest(&g.data))),
        ];
        for (key, have) in checks {
            if w[key] != have {
                out.push(format!("{name}: {key}: {} and {have}", w[key]));
            }
        }
        if w["cells"].as_u64() != Some(g.data.len() as u64) || g.data.is_empty() {
            out.push(format!("{name}: {} cells", g.data.len()));
        }
    }
    out
}

fn run_grid_tool(case: &Value, out: &Path) {
    let part = |key: &str| case[key].as_str().unwrap_or_default().to_string();
    let options = OutputOptions {
        part_a: part("write_a"),
        part_b: part("write_b"),
        part_f: part("write_f"),
        align: if case["align"].as_str() == Some("vortex") {
            Align::Vortex
        } else {
            Align::Refuse
        },
        ..OutputOptions::new(out)
    };
    let input = dir().join(case["input"].as_str().unwrap());
    match case["tool"].as_str().unwrap() {
        "calc" => {
            assert_eq!(case["operation"], "multiply");
            calculate(
                &input,
                None,
                Operation::Multiply,
                case["value"].as_f64().unwrap(),
                &options,
            )
            .unwrap();
        }
        "clip" => {
            let b = case["envelope"].as_array().unwrap();
            let v = |i: usize| b[i].as_f64().unwrap();
            let request = ClipRequest {
                bounds: ClipBounds::Envelope {
                    envelope: Envelope {
                        min_x: v(0),
                        min_y: v(1),
                        max_x: v(2),
                        max_y: v(3),
                    },
                    crs: Crs::Wkt(case["envelope_crs"].as_str().unwrap().to_string()),
                },
            };
            clip(&input, None, &request, &options).unwrap();
        }
        other => panic!("{other}"),
    }
}

#[test]
fn the_calculator_and_the_clipper_store_what_vortex_stores_on_utm_and_feet_grids() {
    let all = expected();
    let cases = all["grid_tools"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    let mut units = Vec::new();
    for case in cases {
        let out = fresh(case["id"].as_str().unwrap());
        run_grid_tool(case, &out);
        let problems = differences(case, &read(&out));
        assert!(problems.is_empty(), "{}: {problems:#?}", case["id"]);
        let text = case["records"][0]["srs_definition"].as_str().unwrap();
        assert!(
            text.contains("PROJECTION[\"Transverse_Mercator\"]"),
            "{text}"
        );
        units.push(text.contains("US survey foot"));
    }
    // A UTM grid in metres and a State Plane grid in US survey feet, each through both tools.
    assert_eq!(units, [false, true, false, true]);
    // The clip is a real subset: 4 by 3 of the input's 8 by 6.
    let clip = &cases[2]["records"][0];
    assert_eq!(
        (clip["nx"].clone(), clip["ny"].clone()),
        (4.into(), 3.into())
    );
}

fn bits(text: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(text, 16).unwrap())
}

/// One grid-to-point case against Vortex's recorded masks and statistics, every mask cell and every
/// statistic in bits. Returns (mask cells, statistics, statistics not missing, empty zones).
fn check_zonal_case(case: &Value) -> (usize, usize, usize, usize) {
    let id = case["id"].as_str().unwrap();
    let table = zonal::compute(&ZonalRequest {
        grids: dir().join(case["grids"].as_str().unwrap()),
        variables: None,
        zones: dir().join(case["zones"].as_str().unwrap()),
        field: case["field"].as_str().unwrap().to_string(),
        zones_crs: None,
        datum_policy: DatumPolicy::Refuse,
        compat: Compat::Engine,
        limits: ZoneLimits::default(),
        area_weighted: false,
    })
    .unwrap_or_else(|e| panic!("{id}: {e}"));
    let (mut cells, mut compared, mut not_missing) = (0usize, 0usize, 0usize);
    let zones = case["masks"].as_object().unwrap();
    assert_eq!(zones.len(), table.zones.len(), "{id}: zone count");
    for zone in &table.zones {
        let want: Vec<u32> = zones[&zone.id]
            .as_array()
            .unwrap_or_else(|| panic!("{id}: {} is extra", zone.id))
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        assert_eq!(want, zone.cells, "{id}: {}", zone.id);
        cells += want.len();
    }
    let frames = case["frames"].as_array().unwrap();
    assert_eq!(frames.len(), table.frames.len(), "{id}: frames");
    for (f, want) in frames.iter().enumerate() {
        assert_eq!(table.frames[f].start, want["start"].as_i64(), "{id}");
        assert_eq!(table.frames[f].end, want["end"].as_i64(), "{id}");
        for (z, zone) in table.zones.iter().enumerate() {
            let s = &table.stats[f * table.zones.len() + z];
            let w = &want["stats"][&zone.id];
            for (name, got) in [
                ("average", s.average),
                ("min", s.min),
                ("max", s.max),
                ("median", s.median),
                ("q1", s.first_quartile),
                ("q3", s.third_quartile),
                ("pct0", s.pct_gt_zero),
                ("pctq1", s.pct_gt_first_quartile),
            ] {
                let value = bits(w[name].as_str().unwrap());
                assert_eq!(
                    got.to_bits(),
                    value.to_bits(),
                    "{id} frame {f} {} {name}",
                    zone.id
                );
                compared += 1;
                not_missing += usize::from(value != MISSING);
            }
        }
    }
    let empty = zones
        .values()
        .filter(|z| z.as_array().unwrap().is_empty())
        .count();
    (cells, compared, not_missing, empty)
}

/// As `zonal.rs` holds P51's fixture: every mask cell and every statistic, in bits.
#[test]
fn zones_in_utm_and_in_feet_give_vortexs_masks_and_statistics() {
    let all = expected();
    let cases = all["zonal"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    let (mut cells, mut compared, mut not_missing) = (0usize, 0usize, 0usize);
    for case in cases {
        let (c, s, n, empty) = check_zonal_case(case);
        // One zone of each file lies outside the grid, and one across the UTM zone edge at 96 W.
        assert_eq!(empty, 1, "{}", case["id"]);
        cells += c;
        compared += s;
        not_missing += n;
    }
    // 489 mask cells and 192 statistics in each case, 144 of them not missing.
    assert_eq!((cells, compared, not_missing), (1467, 576, 432));
}

/// Grid to point on a Transverse Mercator *grid*: P53b's UTM grid in metres and its State Plane
/// grid in US survey feet, each with zones in its own CRS and in lon/lat. This is the grid side of
/// `zonal::dss_frame`, which the cases over the SHG grid above do not reach.
#[test]
fn zones_on_utm_and_feet_grids_give_vortexs_masks_and_statistics() {
    let all = expected();
    let cases = all["zonal_tm_grid"].as_array().unwrap();
    let ids: Vec<&str> = cases.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "zones.tm_grid.utm",
            "zones.tm_grid.utm_lonlat",
            "zones.tm_grid.tm_feet",
            "zones.tm_grid.tm_feet_lonlat"
        ]
    );
    let grids: Vec<&str> = cases.iter().map(|c| c["grids"].as_str().unwrap()).collect();
    assert_eq!(grids, ["utm.dss", "utm.dss", "tm_feet.dss", "tm_feet.dss"]);
    let (mut cells, mut compared, mut not_missing) = (0usize, 0usize, 0usize);
    for case in cases {
        let (c, s, n, empty) = check_zonal_case(case);
        // Each file has one zone off the grid; the others hold cells.
        assert_eq!(empty, 1, "{}", case["id"]);
        cells += c;
        compared += s;
        not_missing += n;
    }
    // 17 mask cells in each grid's own CRS and 20 in lon/lat; 32 statistics a case, 21 not missing.
    assert_eq!((cells, compared, not_missing), (74, 128, 84));
}

/// A zone moved by one cell's width does not give these masks: the comparison is not vacuous.
#[test]
fn the_zonal_comparison_fails_when_the_zones_are_read_in_another_crs() {
    let all = expected();
    let case = &all["zonal"][0];
    // The WGS84 UTM 14 zones, with zone 15's `.prj`: the coordinates then mean somewhere else.
    let work = fresh("wrong_prj");
    let work = work.parent().unwrap();
    for suffix in ["shp", "shx", "dbf"] {
        fs::copy(
            dir().join(format!("utm14_wgs84.{suffix}")),
            work.join(format!("z.{suffix}")),
        )
        .unwrap();
    }
    fs::copy(dir().join("utm15_nad83.prj"), work.join("z.prj")).unwrap();
    let table = zonal::compute(&ZonalRequest {
        grids: dir().join(case["grids"].as_str().unwrap()),
        variables: None,
        zones: work.join("z.shp"),
        field: "Name".to_string(),
        zones_crs: None,
        datum_policy: DatumPolicy::Refuse,
        compat: Compat::Engine,
        limits: ZoneLimits::default(),
        area_weighted: false,
    })
    .unwrap();
    let want = case["masks"].as_object().unwrap();
    let same = table
        .zones
        .iter()
        .filter(|z| !z.cells.is_empty())
        .all(|z| want[&z.id].as_array().unwrap().len() == z.cells.len());
    let any = table.zones.iter().any(|z| !z.cells.is_empty());
    assert!(!(same && any), "the wrong CRS gave the right masks");
}

fn temporal_request(input: &Path, output: &Path) -> TemporalRequest {
    TemporalRequest {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        selection: None,
        parts: Parts::default(),
        c_part: None,
        units: None,
        data_type: None,
        version: None,
        sync: dss_core::dss7::write::SyncMode::None,
        allow_undetectable_readers: true,
        fault: None,
        limits: Default::default(),
    }
}

#[test]
fn the_time_tools_store_what_vortex_stores_on_a_utm_grid() {
    let all = expected();
    let cases = all["temporal"].as_array().unwrap();
    assert_eq!(cases.len(), 2);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let out = fresh(id);
        let request = temporal_request(&dir().join(case["input"].as_str().unwrap()), &out);
        let amount = case["amount"].as_i64().unwrap();
        let mut go = |_: usize, _: usize| ControlFlow::Continue(());
        match case["tool"].as_str().unwrap() {
            "shift" => shift_times(&request, amount, amount, &mut go).map(|_| ()),
            "resample" => resample_times(&request, amount, Rule::Vortex, &mut go).map(|_| ()),
            other => panic!("{other}"),
        }
        .unwrap_or_else(|e| panic!("{id}: {e}"));
        let problems = differences(case, &read(&out));
        assert!(problems.is_empty(), "{id}: {problems:#?}");
        // Vortex's text is GDAL's wording of the input's, which this engine keeps as it is.
        let r = &case["records"][0];
        assert_ne!(r["srs_definition"], r["engine_srs_definition"]);
        assert!(r["engine_srs_definition"]
            .as_str()
            .unwrap()
            .contains("PARAMETER[\"central_meridian\",-117]"));
    }
}

/// Negative controls, through the same comparison and real output.
#[test]
fn the_grid_comparison_fails_on_each_field_and_on_one_cell_bit() {
    let all = expected();
    let case = &all["grid_tools"][3];
    assert_eq!(case["id"], "clip.tm_feet");
    let out = fresh("control");
    run_grid_tool(case, &out);
    let got = read(&out);
    assert!(differences(case, &got).is_empty());
    let name = case["records"][0]["pathname"].as_str().unwrap().to_string();
    type Change = fn(&mut Grid);
    let changes: [(&str, Change); 8] = [
        ("nx", |g| g.nx += 1),
        ("llx", |g| g.lower_left_cell_x += 1),
        ("lly", |g| g.lower_left_cell_y -= 1),
        ("cell_bits", |g| {
            g.cell_size = f32::from_bits(g.cell_size.to_bits() + 1)
        }),
        ("srs_name", |g| g.srs_name.push('x')),
        // A foot relabelled as a metre.
        ("engine_srs_definition", |g| {
            g.srs_definition = g.srs_definition.replace("US survey foot", "metre")
        }),
        ("mean_bits", |g| g.mean_data_value = Some(1.0e6)),
        ("digest", |g| {
            let c = &mut g.data[1];
            *c = f32::from_bits(c.to_bits() ^ 1);
        }),
    ];
    for (key, change) in changes {
        let mut changed = got.clone();
        change(changed.get_mut(&name).unwrap());
        let found = differences(case, &changed);
        assert!(found.iter().any(|p| p.contains(key)), "{key}: {found:?}");
    }
    assert!(!differences(case, &BTreeMap::new()).is_empty());
    // The calculator's output is not the clipper's.
    let other = fresh("control.other");
    run_grid_tool(&all["grid_tools"][1], &other);
    assert!(!differences(case, &read(&other)).is_empty());
}

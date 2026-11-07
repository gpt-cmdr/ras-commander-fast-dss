//! A Zarr store through the importer: AORC's end-labelled hour, its stamps, type and units in
//! DSS7 and DSS6, the warp to SHG against the same cells given to `import_arrays`, and what is
//! refused. The real chunks are `fixtures/public/zarr/aorc_2019_subset.zarr`.

#![allow(clippy::result_large_err)] // `ImportError`: see `import.rs`.
mod zarr_support;

use dss_core::dss6::Dss6File;
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::frame::{Crs, DataType, FrameTime, NamedCrs};
use dss_vortex::import::Bounds;
use dss_vortex::import::{self, ArrayInput, ImportError, ImportRequest, ImportedRecord, Selection};
use dss_vortex::products::window::Accumulation;
use dss_vortex::products::WindowRule;
use dss_vortex::source::netcdf::NetcdfFile;
use dss_vortex::source::zarr::{self, Bbox, StoreSelection, ZarrStore};
use dss_vortex::warp::{Align, Method};
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use zarr_support::*;

/// Seconds since 1970 of 2019-09-19 22:00 and 2019-09-20 01:00 UTC: steps 6286 to 6289.
const FIRST: i64 = 1_568_930_400;
const LAST: i64 = 1_568_941_200;
/// The Texas Panhandle (Fall et al. 2023, Fig. 13).
const PANHANDLE: Bbox = Bbox {
    west: -102.0,
    south: 34.6,
    east: -100.7,
    north: 35.6,
};

fn aorc() -> PathBuf {
    fixtures().join("aorc_2019_subset.zarr")
}

fn out(name: &str) -> PathBuf {
    scratch(&format!("zarr_import_out_{name}")).join("out.dss")
}

fn request(input: &Path, output: &Path, variable: &str, version: u8) -> ImportRequest {
    let mut r = ImportRequest::new(
        vec![input.to_path_buf()],
        output.to_path_buf(),
        Selection::Variable(variable.to_string()),
        Method::Bilinear,
    );
    r.parts.a = "SHG".into();
    r.parts.b = "PANHANDLE".into();
    r.parts.f = "AORC".into();
    r.version = Some(version);
    r.sync = false;
    // A latitude and longitude grid's corner is not on whole cells of its own size, which the
    // importer refuses unless a warp states an extent (`to_shg`) or Vortex's rounding is asked for.
    r.align = Align::Vortex;
    r
}

/// The warp a real import states: SHG at 2000 m on an extent of whole cells inside the window,
/// 53 x 50 cells, with nothing left to round.
fn to_shg(mut r: ImportRequest) -> ImportRequest {
    r.target = Some(Crs::Named(NamedCrs::Shg));
    r.cell_size = Some(2000.0);
    r.bounds = Bounds::Envelope {
        envelope: Envelope {
            min_x: -536_000.0,
            max_x: -430_000.0,
            min_y: 1_302_000.0,
            max_y: 1_402_000.0,
        },
        crs: None,
    };
    r.align = Align::Refuse;
    r
}

fn selection() -> StoreSelection {
    StoreSelection {
        bbox: Some(PANHANDLE),
        times: Some((FIRST, LAST)),
    }
}

fn run(r: &ImportRequest, s: &StoreSelection) -> Result<Vec<ImportedRecord>, ImportError> {
    zarr::import_stores(r, s, &mut |_| ControlFlow::Continue(()))
}

fn read_back(path: &Path, version: u8) -> BTreeMap<String, Grid> {
    let mut out = BTreeMap::new();
    if version == 6 {
        let file = Dss6File::open(path).unwrap();
        for e in file.catalog().unwrap().entries {
            let g = file.read_grid(&e.pathname).unwrap();
            out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
        }
    } else {
        let mut file = Dss7File::open(path).unwrap();
        let catalog = file.catalog().unwrap();
        for e in catalog.catalog.entries {
            let g = file.read_grid(&e.pathname).unwrap();
            out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
        }
    }
    out
}

fn bits(g: &Grid) -> Vec<u32> {
    g.data.iter().map(|c| c.to_bits()).collect()
}

/// The D and E parts of a pathname.
fn stamps(pathname: &str) -> (String, String) {
    let parts: Vec<&str> = pathname.split('/').collect();
    (parts[4].to_string(), parts[5].to_string())
}

/// What the four hours must be stamped: each the hour ending at its time, and midnight as 2400
/// of the day it ends. A start-labelled stamp would be one hour later throughout.
const STAMPS: [(&str, &str); 4] = [
    ("19SEP2019:2100", "19SEP2019:2200"),
    ("19SEP2019:2200", "19SEP2019:2300"),
    ("19SEP2019:2300", "19SEP2019:2400"),
    ("20SEP2019:0000", "20SEP2019:0100"),
];

#[test]
fn aorc_hours_are_stamped_as_the_hour_ending_at_their_time() {
    let store = ZarrStore::open(&aorc()).unwrap();
    let v = store.variable("APCP_surface").unwrap();
    let window = v.window_for_bbox(&PANHANDLE).unwrap();
    let source = v.read(6286..6290, &window).unwrap();
    let (nx, ny, _) = v.grid(&window).unwrap();
    assert_eq!((nx, ny), (157, 121));
    for version in [7u8, 6] {
        let path = out(&format!("stamps{version}"));
        let records = run(
            &request(&aorc(), &path, "APCP_surface", version),
            &selection(),
        )
        .unwrap();
        assert_eq!(records.len(), 4, "DSS{version}");
        // The same through the warp to SHG: the stamps, type and units do not depend on the grid.
        let warped_path = out(&format!("stamps_shg{version}"));
        let warped = run(
            &to_shg(request(&aorc(), &warped_path, "APCP_surface", version)),
            &selection(),
        )
        .unwrap();
        let warped_grids = read_back(&warped_path, version);
        assert_eq!((warped.len(), warped_grids.len()), (4, 4));
        for (step, record) in warped.iter().enumerate() {
            let w = &record.record;
            assert_eq!(
                stamps(&w.pathname),
                (STAMPS[step].0.to_string(), STAMPS[step].1.to_string())
            );
            assert_eq!((w.data_type, w.units.as_str()), (DataType::PerCum, "MM"));
            assert_eq!((w.nx, w.ny, w.cell_size), (53, 50, 2000.0));
            let g = &warped_grids[&w.pathname];
            assert_eq!(g.data_type, 1, "PER-CUM");
            assert_eq!(g.end_time.unwrap() - g.start_time.unwrap(), 3600);
        }
        let grids = read_back(&path, version);
        assert_eq!(grids.len(), 4, "DSS{version}");
        for (step, record) in records.iter().enumerate() {
            let w = &record.record;
            assert_eq!(
                stamps(&w.pathname),
                (STAMPS[step].0.to_string(), STAMPS[step].1.to_string()),
                "DSS{version} step {step}"
            );
            let parts: Vec<&str> = w.pathname.split('/').collect();
            assert_eq!(parts[3], "PRECIPITATION");
            assert_eq!(w.data_type, DataType::PerCum);
            assert_eq!(w.units, "MM");
            assert_eq!((w.nx, w.ny), (nx, ny));
            let g = &grids[&w.pathname];
            assert_eq!(g.data_type, 1, "PER-CUM");
            assert_eq!(g.data_units.trim().to_uppercase(), "MM");
            assert_eq!(g.end_time.unwrap() - g.start_time.unwrap(), 3600);
            // Whole seconds since 1899-12-31: the period ends at the store's instant.
            let instant = FIRST + 3600 * step as i64;
            assert_eq!(g.end_time.unwrap(), instant + 25_568 * 86_400);
            // No warp was asked for: the record holds the store's cells, bit for bit, in its
            // south-to-north row order. kg/m^2 is mm of water, so no value changes.
            let want: Vec<u32> = source[step * nx * ny..(step + 1) * nx * ny]
                .iter()
                .map(|c| c.to_bits())
                .collect();
            assert_eq!(bits(g), want, "DSS{version} step {step}");
            assert_eq!(g.data.len(), 18_997);
        }
        // The published 6.6 in. "for the hour ending at 00 UTC" is in the record ending at 2400.
        let peak = |i: usize| {
            let g = &grids[&records[i].record.pathname];
            g.data.iter().copied().fold(0.0f32, f32::max) / 25.4
        };
        assert!((peak(2) - 6.59).abs() < 0.005, "{}", peak(2));
        assert!(peak(1) < 2.0 && peak(3) < 2.4);
    }
}

/// Negative control for the stamps: the same store with its precipitation not stated as AORC's
/// has no period, and is refused, not stamped as an instant. A zero-length record is what gave
/// HEC-HMS no rain.
#[test]
fn precipitation_with_no_stated_period_is_refused_and_a_stated_one_is_stamped() {
    let root = scratch("zarr_import_plain").join("plain.zarr");
    let hours = [0.0, 1.0, 2.0];
    let attrs = r#""units":"mm","crs":"EPSG:4326","_FillValue":-1"#;
    write_store(
        &root,
        &grid_arrays("precipitation", &hours, 4, 6, attrs, &ramp(72)),
    );
    let path = out("plain");
    let r = request(&root, &path, "precipitation", 7);
    let e = run(&r, &StoreSelection::default()).unwrap_err();
    assert!(e.to_string().contains("precipitation at an instant"), "{e}");
    assert!(!path.exists(), "a refused run leaves no file");

    // The caller states one hour: periods ending at each time, and the note says who stated it.
    let mut stated = r.clone();
    stated.window_rule = WindowRule::Accumulation(Accumulation::H01);
    let records = run(&stated, &StoreSelection::default()).unwrap();
    assert_eq!(records.len(), 3);
    let got: Vec<(String, String)> = records.iter().map(|r| stamps(&r.record.pathname)).collect();
    assert_eq!(
        got,
        [
            ("31DEC1999:2300".to_string(), "31DEC1999:2400".to_string()),
            ("01JAN2000:0000".to_string(), "01JAN2000:0100".to_string()),
            ("01JAN2000:0100".to_string(), "01JAN2000:0200".to_string()),
        ]
    );
    for record in &records {
        assert_eq!(record.window.as_ref().unwrap().stated_by, "caller");
        assert_eq!(record.record.data_type, DataType::PerCum);
    }
    // Through `import_gridded`, which opens a folder as a store.
    let direct = out("plain_direct");
    let mut by_path = stated.clone();
    by_path.output = direct.clone();
    let records = import::import_gridded(&by_path, &mut |_| ControlFlow::Continue(())).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(read_back(&direct, 7).len(), 3);
}

#[test]
fn a_stated_accumulation_must_agree_with_aorc() {
    let path = out("agree");
    let mut r = request(&aorc(), &path, "APCP_surface", 7);
    r.window_rule = WindowRule::Accumulation(Accumulation::H01);
    let records = run(&r, &selection()).unwrap();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].window.as_ref().unwrap().stated_by, "store");
    assert_eq!(stamps(&records[2].record.pathname).1, "19SEP2019:2400");

    let other = out("disagree");
    let mut r = request(&aorc(), &other, "APCP_surface", 7);
    r.window_rule = WindowRule::Accumulation(Accumulation::H03);
    let e = run(&r, &selection()).unwrap_err();
    assert!(e.to_string().contains("the store states 3600 s"), "{e}");
    assert!(!other.exists());
}

/// A variable that is not a depth is not written as precipitation, with or without a statement.
#[test]
fn temperature_is_not_written_as_precipitation() {
    let store = ZarrStore::open(&aorc()).unwrap();
    let v = store.variable("TMP_2maboveground").unwrap();
    assert_eq!(v.stated_accumulation(), None);
    assert!(matches!(v.time(6288).unwrap(), FrameTime::Instant(_)));
    assert_eq!(v.units(), "K");

    let path = out("tmp_stated");
    let mut r = request(&aorc(), &path, "TMP_2maboveground", 7);
    r.window_rule = WindowRule::Accumulation(Accumulation::H01);
    let corner = StoreSelection {
        bbox: Some(Bbox {
            west: -129.9,
            south: 20.1,
            east: -129.0,
            north: 20.9,
        }),
        times: Some((FIRST, FIRST)),
    };
    let e = run(&r, &corner).unwrap_err();
    assert!(e.to_string().contains("not precipitation"), "{e}");
    assert!(!path.exists());

    // As itself: an instant, INST-VAL, TEMPERATURE. Its cells in this corner are all the fill.
    let path = out("tmp");
    let r = request(&aorc(), &path, "TMP_2maboveground", 7);
    let records = run(&r, &corner).unwrap();
    assert_eq!(records.len(), 1);
    let w = &records[0].record;
    let parts: Vec<&str> = w.pathname.split('/').collect();
    assert_eq!(parts[3], "TEMPERATURE");
    assert_eq!((parts[4], parts[5]), ("19SEP2019:2200", ""));
    assert_eq!(w.data_type, DataType::InstVal);
    assert_ne!(w.units, "MM");
}

/// The same cells through the warp to SHG, from the store and from arrays. `import_arrays` is
/// what `oracle/vortex/run_import.py` holds to Vortex, so this ties a store's records to it.
fn shg(version: u8) {
    let store = ZarrStore::open(&aorc()).unwrap();
    let v = store.variable("APCP_surface").unwrap();
    let window = v.window_for_bbox(&PANHANDLE).unwrap();
    let (nx, ny, transform) = v.grid(&window).unwrap();
    let cells = v.read(6286..6290, &window).unwrap();
    let times: Vec<FrameTime> = (6286..6290).map(|s| v.time(s).unwrap()).collect();

    let from_store = out(&format!("shg_store{version}"));
    let r = to_shg(request(&aorc(), &from_store, "APCP_surface", version));
    let stored = run(&r, &selection()).unwrap();

    let arrays = |cells: Vec<f32>| ArrayInput {
        cells,
        nx,
        ny,
        transform,
        crs: v.crs().clone(),
        nodata: v.nodata().unwrap(),
        times: times.clone(),
        variable: "APCP_surface".into(),
        description: v.description().map(str::to_string),
        units: v.units().to_string(),
        file_name: "2019.zarr".into(),
    };
    let from_arrays = out(&format!("shg_arrays{version}"));
    let mut ra = r.clone();
    ra.output = from_arrays.clone();
    let given = import::import_arrays(&arrays(cells.clone()), &ra, &mut |_| {
        ControlFlow::Continue(())
    })
    .unwrap();

    let (a, b) = (
        read_back(&from_store, version),
        read_back(&from_arrays, version),
    );
    assert_eq!((stored.len(), given.len(), a.len(), b.len()), (4, 4, 4, 4));
    let names: Vec<&String> = a.keys().collect();
    assert_eq!(names, b.keys().collect::<Vec<_>>());
    let mut compared = 0;
    for (name, g) in &a {
        let other = &b[name];
        assert_eq!(bits(g), bits(other), "{name}");
        assert_eq!((g.nx, g.ny), (other.nx, other.ny));
        assert_eq!(
            (
                g.lower_left_cell_x,
                g.lower_left_cell_y,
                g.cell_size.to_bits()
            ),
            (
                other.lower_left_cell_x,
                other.lower_left_cell_y,
                other.cell_size.to_bits()
            )
        );
        assert_eq!(
            (g.data_type, &g.data_units),
            (other.data_type, &other.data_units)
        );
        assert_eq!(
            (g.start_time, g.end_time),
            (other.start_time, other.end_time)
        );
        assert_eq!(g.srs_definition, other.srs_definition);
        assert_eq!(g.data.len(), 2650);
        assert!(g.data.iter().any(|c| *c > 0.0), "{name} holds rain");
        compared += g.data.len();
    }
    assert_eq!(compared, 10_600, "cells compared");
    let ends: Vec<String> = stored
        .iter()
        .map(|r| stamps(&r.record.pathname).1)
        .collect();
    assert_eq!(
        ends,
        [
            "19SEP2019:2200",
            "19SEP2019:2300",
            "19SEP2019:2400",
            "20SEP2019:0100"
        ]
    );

    // Negative control: one source cell changed by 1 mm changes a record. (One bit is lost in
    // the bilinear weights, so it would not show.)
    let mut changed = cells;
    // A wet cell in the middle of the hour ending at midnight, well inside the SHG extent.
    let frame = 2 * nx * ny;
    let at = (ny / 3..2 * ny / 3)
        .flat_map(|row| (nx / 3..2 * nx / 3).map(move |col| frame + row * nx + col))
        .find(|i| changed[*i] > 1.0)
        .unwrap();
    changed[at] += 1.0;
    let control = out(&format!("shg_control{version}"));
    let mut rc = r.clone();
    rc.output = control.clone();
    import::import_arrays(&arrays(changed), &rc, &mut |_| ControlFlow::Continue(())).unwrap();
    let c = read_back(&control, version);
    assert!(a.iter().any(|(name, g)| bits(g) != bits(&c[name])));
}

#[test]
fn the_warp_to_shg_equals_the_same_cells_given_as_arrays_dss7() {
    shg(7);
}

#[test]
fn the_warp_to_shg_equals_the_same_cells_given_as_arrays_dss6() {
    shg(6);
}

/// Vortex's own AORC test file, through the NetCDF source, gives the hour ending at its stamp
/// (`GridDatasetReader`, the `AORC_APCP` rule; `NetcdfDataReaderTest` expects 1983-12-31 23:00 to
/// 1984-01-01 00:00). A store that states the same instant gives the same period.
#[test]
fn the_aorc_period_is_vortexs_for_the_same_instant() {
    let nc = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/netcdf/AORC_APCP_MARFC_1984010100.nc4");
    let file = NetcdfFile::open(&nc).unwrap();
    let name = file
        .variable_names()
        .into_iter()
        .find(|n| n.contains("APCP"))
        .unwrap();
    let step = file.variable(&name).unwrap().step(0).unwrap();
    let midnight = 441_763_200_000i64; // 1984-01-01 00:00 UTC, in milliseconds.
    assert_eq!(
        (step.start, step.end),
        (Some(midnight - 3_600_000), Some(midnight))
    );
    assert_eq!(step.data_type, DataType::PerCum);

    let root = scratch("zarr_import_1984").join("1984.zarr");
    // Hours since 2000-01-01 of 1983-12-31 23:00 and 1984-01-01 00:00.
    let hours = [-140_257.0, -140_256.0];
    let attrs = r#""units":"kg/m^2","crs":"EPSG:4326","_FillValue":-32767,"aorc_version":"v1.1","scale_factor":0.1"#;
    write_store(
        &root,
        &grid_arrays("APCP_surface", &hours, 4, 6, attrs, &ramp(48)),
    );
    let store = ZarrStore::open(&root).unwrap();
    let v = store.variable("APCP_surface").unwrap();
    assert_eq!(v.instant(1).unwrap(), Some(midnight / 1000));
    let julian = |y, m, d| dss_core::time::ymd_to_julian(y, m, d);
    assert_eq!(
        v.time(1).unwrap(),
        FrameTime::Period {
            start: (julian(1983, 12, 31), 82_800),
            end: (julian(1984, 1, 1), 0),
        }
    );
    let path = out("1984");
    let records = run(
        &request(&root, &path, "APCP_surface", 7),
        &StoreSelection::default(),
    )
    .unwrap();
    assert_eq!(
        stamps(&records[1].record.pathname),
        ("31DEC1983:2300".to_string(), "31DEC1983:2400".to_string())
    );

    // A subset with a gap is still hourly totals: `tests/zarr_aorc.rs` holds the rule.
    let gapped = scratch("zarr_import_odd").join("odd.zarr");
    write_store(
        &gapped,
        &grid_arrays("APCP_surface", &[0.0, 3.0], 4, 6, attrs, &ramp(48)),
    );
    let store = ZarrStore::open(&gapped).unwrap();
    assert_eq!(
        store
            .variable("APCP_surface")
            .unwrap()
            .stated_accumulation(),
        Some(3600)
    );
}

/// A valid cell a DSS reader would take as missing is refused, not written: -901 and -902 are
/// HEC's markers (`heclib`'s `UNDEFINED`), and the sink's rule covers every source.
#[test]
fn a_cell_a_reader_would_take_as_missing_is_refused() {
    for marker in [-901i16, -902] {
        let root = scratch(&format!("zarr_import_marker{}", -marker)).join("m.zarr");
        let mut data = ramp(24);
        data[5] = marker;
        let attrs = r#""units":"degC","crs":"EPSG:4326","_FillValue":-1"#;
        write_store(
            &root,
            &grid_arrays("TMP_2maboveground", &[0.0], 4, 6, attrs, &data),
        );
        let path = out(&format!("marker{}", -marker));
        let e = run(
            &request(&root, &path, "TMP_2maboveground", 7),
            &StoreSelection::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("cannot hold as a value"), "{e}");
        assert!(e.completed.is_empty());
    }
}

#[test]
fn selections_that_take_nothing_are_refused() {
    let path = out("nothing");
    let r = request(&aorc(), &path, "APCP_surface", 7);
    // No step in the range.
    let none = StoreSelection {
        bbox: Some(PANHANDLE),
        times: Some((0, 10)),
    };
    let e = run(&r, &none).unwrap_err();
    assert!(e.to_string().contains("holds no frames"), "{e}");
    // A box off the grid.
    let off = StoreSelection {
        bbox: Some(Bbox {
            west: 10.0,
            south: 0.0,
            east: 11.0,
            north: 1.0,
        }),
        times: Some((FIRST, LAST)),
    };
    let e = run(&r, &off).unwrap_err();
    assert!(e.to_string().contains("does not touch its grid"), "{e}");
    // A variable the store does not hold.
    let e = run(&request(&aorc(), &path, "NO_SUCH", 7), &selection()).unwrap_err();
    assert!(e.to_string().contains("not in this input"), "{e}");
    assert!(!path.exists());
    // A folder that is no store.
    let empty = scratch("zarr_import_empty");
    let e = run(&request(&empty, &path, "x", 7), &selection()).unwrap_err();
    assert!(e.to_string().contains("not a Zarr v2 store"), "{e}");
    let _ = fs::remove_dir_all(empty);
}

/// A store with no stated CRS is read, and is not given one: a warp of it is refused.
#[test]
fn a_store_that_states_no_crs_is_not_given_one() {
    let root = scratch("zarr_import_nocrs").join("nocrs.zarr");
    let attrs = r#""units":"K","_FillValue":-1"#;
    write_store(
        &root,
        &grid_arrays("TMP_2maboveground", &[0.0], 4, 6, attrs, &ramp(24)),
    );
    let store = ZarrStore::open(&root).unwrap();
    assert_eq!(
        *store.variable("TMP_2maboveground").unwrap().crs(),
        Crs::Undefined
    );
    let path = out("nocrs");
    let mut r = request(&root, &path, "TMP_2maboveground", 7);
    r.target = Some(Crs::Named(NamedCrs::Shg));
    r.cell_size = Some(2000.0);
    let e = run(&r, &StoreSelection::default()).unwrap_err();
    assert!(e.to_string().contains("states no CRS"), "{e}");
    assert!(!path.exists());
}

/// A time axis that states one time twice would write one pathname twice. That is refused before
/// anything is written, as it is for any source.
#[test]
fn a_time_stated_twice_is_refused() {
    let root = scratch("zarr_import_twice").join("twice.zarr");
    let attrs = r#""units":"degC","crs":"EPSG:4326","_FillValue":-1"#;
    write_store(
        &root,
        &grid_arrays("TMP_2maboveground", &[5.0, 5.0], 4, 6, attrs, &ramp(48)),
    );
    let path = out("twice");
    let r = request(&root, &path, "TMP_2maboveground", 7);
    let e = run(&r, &StoreSelection::default()).unwrap_err();
    assert!(e.to_string().contains("would write it twice"), "{e}");
    assert!(!path.exists());
    // Negative control: with distinct times the same store is written.
    let distinct = scratch("zarr_import_distinct").join("distinct.zarr");
    write_store(
        &distinct,
        &grid_arrays("TMP_2maboveground", &[5.0, 6.0], 4, 6, attrs, &ramp(48)),
    );
    let records = run(
        &request(&distinct, &path, "TMP_2maboveground", 7),
        &StoreSelection::default(),
    )
    .unwrap();
    assert_eq!(records.len(), 2);
}

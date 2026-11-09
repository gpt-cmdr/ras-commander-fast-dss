//! Reading only what an import needs from a store.
//!
//! `import_gridded` read each whole source frame before the warp, so a small envelope on a year
//! of AORC read 35 million cells a step, for 8,760 steps. Now the envelope, padded for the warp,
//! and a time range go down into the store's read, and a request that would still read too much
//! is refused while planning, with the selection to add.

#![allow(clippy::result_large_err)] // `ImportError`: see `import.rs`.
mod zarr_support;

use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::frame::{Crs, FrameTime, GeoTransform, NamedCrs};
use dss_vortex::import::{
    self, ArrayInput, Bounds, GridSource, ImportError, ImportRequest, ImportedRecord, Selection,
    SourceSelection,
};
use dss_vortex::products::WindowRule;
use dss_vortex::source::zarr::{Limits, ZarrSource, ZarrStore};
use dss_vortex::warp::{Method, WarpEnvelope};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use zarr_support::*;

const FIRST: i64 = 1_568_930_400; // 2019-09-19 22:00 UTC
const LAST: i64 = 1_568_941_200; // 2019-09-20 01:00 UTC

fn aorc() -> PathBuf {
    fixtures().join("aorc_2019_subset.zarr")
}

fn shg_box() -> Envelope {
    Envelope {
        min_x: -536_000.0,
        max_x: -430_000.0,
        min_y: 1_302_000.0,
        max_y: 1_402_000.0,
    }
}

fn request(name: &str, method: Method) -> ImportRequest {
    let out = scratch(&format!("zarr_pushdown_{name}")).join("out.dss");
    let mut r = ImportRequest::new(
        vec![aorc()],
        out,
        Selection::Variable("APCP_surface".into()),
        method,
    );
    r.parts.a = "SHG".into();
    r.parts.f = "AORC".into();
    r.sync = false;
    r.target = Some(Crs::Named(NamedCrs::Shg));
    r.cell_size = Some(2000.0);
    r.bounds = Bounds::Envelope {
        envelope: shg_box(),
        crs: None,
    };
    r
}

fn run(r: &ImportRequest) -> Result<Vec<ImportedRecord>, ImportError> {
    import::import_gridded(r, &mut |_| ControlFlow::Continue(()))
}

fn read_back(path: &Path) -> BTreeMap<String, Grid> {
    let mut file = Dss7File::open(path).unwrap();
    let catalog = file.catalog().unwrap();
    let mut out = BTreeMap::new();
    for e in catalog.catalog.entries {
        let g = file.read_grid(&e.pathname).unwrap();
        out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
    }
    out
}

/// The whole grid, stated as a box: what `import_gridded` read before, for every request.
const WHOLE: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];

/// The envelope's own cells give the records the whole frame gives, bit for bit: the padding is
/// more than the warp reaches. Run for each resampling method, four hours of real rain each.
#[test]
fn the_envelopes_cells_give_the_whole_frames_records() {
    for (name, method) in [
        ("bilinear", Method::Bilinear),
        ("near", Method::Near),
        ("average", Method::Average),
    ] {
        let mut pushed = request(&format!("pushed_{name}"), method);
        pushed.source_times = Some((FIRST, LAST));
        let mut whole = request(&format!("whole_{name}"), method);
        whole.source_times = Some((FIRST, LAST));
        whole.source_bbox = Some(WHOLE);
        let (a, b) = (run(&pushed).unwrap(), run(&whole).unwrap());
        assert_eq!((a.len(), b.len()), (4, 4), "{name}");
        let (ga, gb) = (read_back(&pushed.output), read_back(&whole.output));
        assert_eq!(ga.keys().collect::<Vec<_>>(), gb.keys().collect::<Vec<_>>());
        let mut cells = 0;
        for (path, g) in &ga {
            let other = &gb[path];
            let bits = |g: &Grid| g.data.iter().map(|c| c.to_bits()).collect::<Vec<u32>>();
            assert_eq!(bits(g), bits(other), "{name} {path}");
            assert_eq!(
                (g.nx, g.ny, g.lower_left_cell_x, g.lower_left_cell_y),
                (
                    other.nx,
                    other.ny,
                    other.lower_left_cell_x,
                    other.lower_left_cell_y
                )
            );
            assert_eq!(
                (g.start_time, g.end_time),
                (other.start_time, other.end_time)
            );
            assert!(g.data.iter().any(|c| *c > 0.0), "{name} {path} holds rain");
            cells += g.data.len();
        }
        assert_eq!(cells, 10_600, "{name}");
    }
}

/// What the store is asked for: the envelope's cells and a margin, not the grid's 4201 x 8401.
#[test]
fn an_envelope_selects_a_window_of_the_store() {
    let source = ZarrSource::new(ZarrStore::open(&aorc()).unwrap(), WindowRule::Refuse);
    let whole = source.grid("APCP_surface", 0).unwrap();
    assert_eq!((whole.nx, whole.ny), (8401, 4201));
    let refusal = source.step_count("APCP_surface").unwrap_err().to_string();
    assert!(refusal.contains("max_cells_per_import"), "{refusal}");
    let selection = SourceSelection {
        envelope: Some(WarpEnvelope {
            envelope: shg_box(),
            crs: Some(dss_vortex::crs::CrsDef::shg()),
        }),
        cell_size: Some(2000.0),
        ..SourceSelection::default()
    };
    source.select("APCP_surface", &selection).unwrap();
    let window = source.grid("APCP_surface", 0).unwrap();
    // The envelope is 106 x 100 km: about 1.2 x 0.9 degrees, 140 x 108 cells, and the margin of
    // four 2 km cells and two of the store's on each side.
    assert!((150..190).contains(&window.nx), "{}", window.nx);
    assert!((115..150).contains(&window.ny), "{}", window.ny);
    // A whole year of hours on that window is within the bound, and is counted, not read.
    assert_eq!(source.step_count("APCP_surface").unwrap(), 8760);
    // A time range narrows it, and the caller's own box replaces the envelope's.
    source
        .select(
            "APCP_surface",
            &SourceSelection {
                times: Some((FIRST, LAST)),
                bbox: Some([-102.0, 34.6, -100.7, 35.6]),
                ..selection.clone()
            },
        )
        .unwrap();
    assert_eq!(source.step_count("APCP_surface").unwrap(), 4);
    let stated = source.grid("APCP_surface", 0).unwrap();
    assert_eq!((stated.nx, stated.ny), (157, 121));
}

/// A year's store with no selection at all. It is refused while planning,
/// before a cell is read or a file made, and the refusal says what to state.
#[test]
fn a_year_of_the_whole_grid_is_refused_while_planning() {
    let mut r = request("year", Method::Bilinear);
    r.bounds = Bounds::None;
    r.target = None;
    r.cell_size = None;
    let started = std::time::Instant::now();
    let e = run(&r).unwrap_err();
    assert_eq!(e.phase, "planning");
    let text = e.to_string();
    assert!(text.contains("8760 steps of 8401 x 4201 cells"), "{text}");
    assert!(text.contains("max_cells_per_import"), "{text}");
    assert!(
        text.contains("State times") && text.contains("bbox"),
        "{text}"
    );
    assert!(!r.output.exists());
    assert!(started.elapsed().as_secs() < 30, "planning read the store");

    // With the bound raised the same request plans, so it is the bound that refused: it then
    // stops at the next limit, the destination grid's size.
    r.zarr_limits = Some(Limits {
        max_cells_per_import: u64::MAX,
        ..Limits::default()
    });
    let text = run(&r).unwrap_err().to_string();
    assert!(!text.contains("max_cells_per_import"), "{text}");
    assert!(!r.output.exists());

    // An envelope alone is enough for a short range, and a range alone is not for the whole grid
    // when it is long.
    let mut ranged = request("year_ranged", Method::Bilinear);
    ranged.source_times = Some((FIRST, LAST));
    assert_eq!(run(&ranged).unwrap().len(), 4);
    let mut long = request("year_long", Method::Bilinear);
    long.bounds = Bounds::None;
    long.target = None;
    long.cell_size = None;
    long.source_times = Some((FIRST - 200 * 3600, LAST));
    let text = run(&long).unwrap_err().to_string();
    assert!(text.contains("204 steps of 8401 x 4201"), "{text}");
}

/// A box or a time range on a source that cannot select is refused, not ignored.
#[test]
fn a_selection_on_another_source_is_refused() {
    let out = scratch("zarr_pushdown_arrays").join("out.dss");
    let mut r = ImportRequest::new(
        Vec::new(),
        out.clone(),
        Selection::Variable("t".into()),
        Method::Bilinear,
    );
    r.align = dss_vortex::warp::Align::Vortex;
    let input = ArrayInput {
        cells: vec![1.0; 12],
        nx: 4,
        ny: 3,
        transform: GeoTransform::axis_aligned(2000.0, 2000.0, 8000.0, -2000.0),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: -9999.0,
        times: vec![FrameTime::Instant((45_457, 0))],
        variable: "t".into(),
        description: None,
        units: "m".into(),
        file_name: "arrays".into(),
    };
    let ok = import::import_arrays(&input, &r, &mut |_| ControlFlow::Continue(()));
    assert_eq!(
        ok.unwrap().len(),
        1,
        "the control: with no selection it is written"
    );
    for change in [0, 1] {
        let mut selected = r.clone();
        selected.output = out.with_file_name("refused.dss");
        if change == 0 {
            selected.source_times = Some((0, 10));
        } else {
            selected.source_bbox = Some(WHOLE);
        }
        let e = import::import_arrays(&input, &selected, &mut |_| ControlFlow::Continue(()))
            .unwrap_err();
        assert!(e.to_string().contains("takes bbox or times"), "{e}");
        assert!(!selected.output.exists());
    }
}

/// A complete store can be asked for: a chunk with no file is then a refusal, not missing cells.
#[test]
fn an_incomplete_store_is_refused_when_a_complete_one_is_asked_for() {
    let strict = Limits {
        refuse_absent_chunks: true,
        ..Limits::default()
    };
    let window = dss_vortex::source::zarr::Window {
        rows: 1700..1710,
        cols: 3000..3010,
    };
    // Columns 3000 to 3009 are in chunk 43.13.11, which the fixture does not hold.
    let store = ZarrStore::open(&aorc()).unwrap();
    let cells = store
        .variable("APCP_surface")
        .unwrap()
        .read(6288..6289, &window)
        .unwrap();
    assert!(cells.iter().all(|c| c.is_nan()) && cells.len() == 100);
    let store = ZarrStore::open_with_limits(&aorc(), strict).unwrap();
    let v = store.variable("APCP_surface").unwrap();
    let e = v.read(6288..6289, &window).unwrap_err().to_string();
    assert!(
        e.contains("refuse_absent_chunks") && e.contains("43.13.11"),
        "{e}"
    );
    // Where every chunk is present the strict read is the ordinary one.
    let present = dss_vortex::source::zarr::Window {
        rows: 1700..1710,
        cols: 3400..3410,
    };
    assert_eq!(v.read(6288..6289, &present).unwrap().len(), 100);
    // Through the importer.
    let mut r = request("strict", Method::Bilinear);
    r.source_times = Some((FIRST, LAST));
    r.source_bbox = Some([-104.5, 34.6, -100.7, 35.6]);
    r.zarr_limits = Some(strict);
    let e = run(&r).unwrap_err().to_string();
    assert!(e.contains("refuse_absent_chunks"), "{e}");
}

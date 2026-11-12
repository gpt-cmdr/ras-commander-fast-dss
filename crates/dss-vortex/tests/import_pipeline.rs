//! The importer's guarantees, with no oracle: planning before writing, failure partway, crash,
//! identity, limits and refusals. Sources are in-memory, so each failure lands exactly where a test
//! wants it; `tests/python/test_gridded.py` repeats the file route through the public API.

#![allow(clippy::result_large_err)] // `ImportError`: see `import.rs`.
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{FaultPoint, WriteStep};
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::frame::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
use dss_vortex::import::{
    self, ArrayInput, Bounds, Duplicates, Failure, GridSource, ImportLimits, ImportParts,
    ImportRequest, Selection, SourceGridDef, SourceProvider, StepMeta,
};
use dss_vortex::warp::{Align, Method};
use dss_vortex::VortexError;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

const JUNE15: i64 = 45_457; // Checked in `the_julian_day_constant_is_june_15`.
const NX: usize = 4;
const NY: usize = 3;

fn fresh(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("import_pipeline")
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d.join("out.dss")
}

fn hour(h: i64) -> FrameTime {
    let split = |t: i64| ((t).div_euclid(86_400) + JUNE15, t.rem_euclid(86_400));
    FrameTime::Period {
        start: split(h * 3600),
        end: split((h + 1) * 3600),
    }
}

fn transform() -> GeoTransform {
    GeoTransform::axis_aligned(2000.0, 2000.0, 8000.0, -2000.0)
}

// What goes wrong, and where.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    DecodeAt(usize),
    DifferentTime(usize),
    DifferentVariable(usize),
    DifferentUnits(usize),
    DifferentGrid(usize),
    DifferentCrs(usize),
    DifferentNodata(usize),
}

struct Memory {
    label: &'static str,
    variable: &'static str,
    units: &'static str,
    frames: Vec<FrameTime>,
    nx: usize,
    ny: usize,
    fault: Fault,
    decoded: Cell<usize>,
    nodata: f32,
    // Replaces what `grid` states; with a count, only after that many calls.
    stated_grid: Option<SourceGridDef>,
    grid_changes_after: Option<usize>,
    grid_calls: Cell<usize>,
}

impl Memory {
    fn new(label: &'static str, frames: Vec<FrameTime>) -> Self {
        Memory {
            label,
            variable: "Precip",
            units: "mm",
            frames,
            nx: NX,
            ny: NY,
            fault: Fault::None,
            decoded: Cell::new(0),
            nodata: -9999.0,
            stated_grid: None,
            grid_changes_after: None,
            grid_calls: Cell::new(0),
        }
    }

    fn cells(&self, step: usize) -> Vec<f32> {
        (0..self.nx * self.ny)
            .map(|i| i as f32 + step as f32 * 100.0)
            .collect()
    }
}

impl GridSource for &Memory {
    fn file_name(&self) -> &str {
        self.label
    }

    fn variables(&self) -> Vec<String> {
        vec![self.variable.to_string()]
    }

    fn step_count(&self, _: &str) -> Result<usize, VortexError> {
        Ok(self.frames.len())
    }

    fn step_meta(&self, _: &str, step: usize) -> Result<StepMeta, VortexError> {
        Ok(StepMeta {
            time: self.frames[step],
            units: self.units.to_string(),
            description: None,
            source_type: DataType::PerCum,
        })
    }

    fn grid(&self, _: &str, _: usize) -> Result<SourceGridDef, VortexError> {
        let calls = self.grid_calls.get();
        self.grid_calls.set(calls + 1);
        if let Some(stated) = &self.stated_grid {
            if self.grid_changes_after.is_none_or(|after| calls >= after) {
                return Ok(stated.clone());
            }
        }
        Ok(SourceGridDef {
            nx: self.nx,
            ny: self.ny,
            transform: transform(),
            crs: Crs::Named(NamedCrs::Shg),
            nodata: Some(self.nodata),
            stored_wkt: None,
        })
    }

    fn frame(&self, _: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.decoded.set(self.decoded.get() + 1);
        if self.fault == Fault::DecodeAt(step) {
            return Err(VortexError::Decode {
                format: "test",
                message: "stopped on purpose".into(),
            });
        }
        let mut time = self.frames[step];
        let mut variable = self.variable.to_string();
        let mut units = self.units.to_string();
        let mut t = transform();
        if self.fault == Fault::DifferentTime(step) {
            time = hour(500);
        }
        if self.fault == Fault::DifferentVariable(step) {
            variable = "Temperature".into();
        }
        if self.fault == Fault::DifferentUnits(step) {
            units = "K".into();
        }
        if self.fault == Fault::DifferentGrid(step) {
            t.origin_x += 2000.0;
        }
        let mut crs = Crs::Named(NamedCrs::Shg);
        if self.fault == Fault::DifferentCrs(step) {
            crs = Crs::Named(NamedCrs::Hrap);
        }
        let mut nodata = Some(self.nodata);
        if self.fault == Fault::DifferentNodata(step) {
            nodata = Some(-1.0);
        }
        Ok(GridFrame::new(GridFrameParts {
            cells: self.cells(step),
            nx: self.nx,
            ny: self.ny,
            row_order: RowOrder::NorthToSouth,
            transform: t,
            crs,
            nodata,
            time,
            data_type: DataType::PerCum,
            units,
            variable,
            description: None,
        })?)
    }
}

struct Sources<'a>(Vec<&'a Memory>);

impl SourceProvider for Sources<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn path(&self, index: usize) -> PathBuf {
        PathBuf::from(self.0[index].label)
    }

    fn open(&self, index: usize) -> Result<Box<dyn GridSource + '_>, VortexError> {
        Ok(Box::new(self.0[index]))
    }
}

fn request(output: &Path) -> ImportRequest {
    let mut r = ImportRequest::new(
        Vec::new(),
        output.to_path_buf(),
        Selection::Variable("Precip".into()),
        Method::Near,
    );
    r.parts = ImportParts {
        a: "A".into(),
        b: "B".into(),
        c: None,
        f: "F".into(),
    };
    r
}

fn run(
    sources: &[&Memory],
    r: &ImportRequest,
) -> Result<Vec<import::ImportedRecord>, import::ImportError> {
    import::run(&Sources(sources.to_vec()), r, &mut |_| {
        ControlFlow::Continue(())
    })
}

fn read_back(path: &Path, version: u8) -> BTreeMap<String, Grid> {
    let mut out = BTreeMap::new();
    if version == 6 {
        let file = Dss6File::open(path).unwrap();
        for e in &file.catalog().unwrap().entries {
            let Some(g) = readable(file.read_grid(&e.pathname)) else {
                continue;
            };
            out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
        }
    } else {
        let mut file = Dss7File::open(path).unwrap();
        let catalog = file.catalog().unwrap();
        for e in &catalog.catalog.entries {
            let Some(g) = readable(file.read_grid(&e.pathname)) else {
                continue;
            };
            out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
        }
    }
    out
}

fn bits(g: &Grid) -> Vec<u32> {
    g.data.iter().map(|c| c.to_bits()).collect()
}

/// A request uses every core unless told otherwise. That the written file does not depend on
/// the count is `tests/python/test_p72_threads.py`.
#[test]
fn threads_default_to_the_host_parallelism() {
    let r = request(Path::new("unused.dss"));
    assert_eq!(r.threads, dss_vortex::warp::host_threads());
    assert_eq!(
        r.threads,
        std::thread::available_parallelism().map_or(1, usize::from)
    );
}

#[test]
fn the_julian_day_constant_is_june_15() {
    assert_eq!(dss_core::time::ymd_to_julian(2024, 6, 15), JUNE15);
}

#[test]
fn a_failure_partway_keeps_finished_records_and_says_where() {
    for version in [7u8, 6] {
        let out = fresh(&format!("partway{version}"));
        let good = Memory::new("first.grib2", vec![hour(0), hour(1)]);
        let mut bad = Memory::new("second.grib2", vec![hour(2), hour(3), hour(4)]);
        bad.fault = Fault::DecodeAt(1);
        let mut r = request(&out);
        r.version = Some(version);
        let e = run(&[&good, &bad], &r).unwrap_err();
        assert_eq!(e.phase, "writing");
        assert_eq!(e.input.as_deref(), Some(Path::new("second.grib2")));
        assert_eq!(e.step, Some(1));
        assert_eq!(e.variable.as_deref(), Some("Precip"));
        assert_eq!(e.completed.len(), 3);
        assert!(matches!(
            e.failure,
            Failure::Vortex(VortexError::Decode { .. })
        ));
        let text = e.to_string();
        assert!(
            text.contains("second.grib2") && text.contains("frame 1"),
            "{text}"
        );
        // The three finished records hold their own cells, and nothing else is there.
        let grids = read_back(&out, version);
        assert_eq!(grids.len(), 3);
        let expected = [(0, good.cells(0)), (1, good.cells(1)), (2, bad.cells(0))];
        for (h, cells) in expected {
            let name = e.completed[h].record.pathname.clone();
            let g = &grids[&name];
            // Stored south-up.
            let mut want = Vec::new();
            for row in (0..NY).rev() {
                want.extend_from_slice(&cells[row * NX..(row + 1) * NX]);
            }
            assert_eq!(g.data, want, "{name}");
        }
        // A later run over the same file finishes the job, keeping what was there.
        bad.fault = Fault::None;
        let done = run(&[&good, &bad], &r).unwrap();
        assert_eq!(done.len(), 5);
        assert_eq!(read_back(&out, version).len(), 5);
    }
}

#[test]
fn a_first_frame_that_cannot_decode_creates_no_file() {
    let out = fresh("first_decode");
    let mut bad = Memory::new("a.grib2", vec![hour(0)]);
    bad.fault = Fault::DecodeAt(0);
    let e = run(&[&bad], &request(&out)).unwrap_err();
    assert!(e.completed.is_empty());
    assert!(!out.exists());
}

#[test]
fn planning_refusals_decode_nothing_and_create_nothing() {
    let out = fresh("planning");
    // The second input would write the first's pathname again.
    let a = Memory::new("a.grib2", vec![hour(0)]);
    let b = Memory::new("b.grib2", vec![hour(0)]);
    let e = run(&[&a, &b], &request(&out)).unwrap_err();
    assert_eq!(e.phase, "planning");
    assert_eq!(e.input.as_deref(), Some(Path::new("b.grib2")));
    assert!(e.to_string().contains("twice"), "{e}");
    assert_eq!(a.decoded.get() + b.decoded.get(), 0);
    assert!(!out.exists());
    // A variable missing from the second input refuses the run, naming it.
    let mut other = Memory::new("c.grib2", vec![hour(1)]);
    other.variable = "Other";
    let e = run(&[&a, &other], &request(&out)).unwrap_err();
    assert_eq!(e.input.as_deref(), Some(Path::new("c.grib2")));
    assert!(
        e.to_string()
            .contains("Observed behaviour we did not implement"),
        "{e}"
    );
    assert_eq!(a.decoded.get(), 0);
    assert!(!out.exists());
    // No frames, no records.
    let empty = Memory::new("d.grib2", vec![]);
    let e = run(&[&empty], &request(&out)).unwrap_err();
    assert!(e.to_string().contains("no frames"), "{e}");
    assert!(!out.exists());
}

#[test]
fn stated_duplicates_keep_the_last_and_write_it_once() {
    let out = fresh("duplicates");
    let a = Memory::new("a.grib2", vec![hour(0)]);
    let b = Memory::new("b.grib2", vec![hour(0), hour(1)]);
    let mut r = request(&out);
    r.duplicates = Duplicates::Last;
    let done = run(&[&a, &b], &r).unwrap();
    assert_eq!(done.len(), 2);
    assert_eq!(a.decoded.get(), 0, "a superseded frame is not decoded");
    assert_eq!(done[0].input, PathBuf::from("b.grib2"));
    assert_eq!(read_back(&out, 7).len(), 2);
}

#[test]
fn a_frame_that_differs_from_its_plan_is_never_written_under_the_planned_name() {
    let faults = [
        (Fault::DifferentTime(1), "time"),
        (Fault::DifferentVariable(1), "variable"),
        (Fault::DifferentUnits(1), "units"),
        (Fault::DifferentGrid(1), "grid"),
        (Fault::DifferentCrs(1), "CRS"),
        (Fault::DifferentNodata(1), "nodata"),
    ];
    for (i, (fault, word)) in faults.into_iter().enumerate() {
        let out = fresh(&format!("identity{i}"));
        let mut m = Memory::new("a.grib2", vec![hour(0), hour(1)]);
        m.fault = fault;
        let e = run(&[&m], &request(&out)).unwrap_err();
        assert_eq!(e.step, Some(1), "{word}");
        assert!(e.to_string().contains(word), "{word}: {e}");
        // Only the first frame's record is there.
        let grids = read_back(&out, 7);
        assert_eq!(grids.len(), 1, "{word}");
        assert!(grids.contains_key("/A/B/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/F/"));
    }
}

#[test]
fn an_input_whose_grid_changes_after_planning_writes_nothing_under_the_plan() {
    let shifted = GeoTransform::axis_aligned(4000.0, 2000.0, 8000.0, -2000.0);
    let def = |transform, crs, nodata| SourceGridDef {
        nx: NX,
        ny: NY,
        transform,
        crs,
        nodata: Some(nodata),
        stored_wkt: None,
    };
    let shg = || Crs::Named(NamedCrs::Shg);
    let changes = [
        ("transform", def(shifted, shg(), -9999.0)),
        ("crs", def(transform(), Crs::Named(NamedCrs::Hrap), -9999.0)),
        ("nodata", def(transform(), shg(), -1.0)),
    ];
    for version in [7u8, 6] {
        for (name, changed) in &changes {
            let out = fresh(&format!("changes_{name}{version}"));
            let mut m = Memory::new("a.grib2", vec![hour(0), hour(1)]);
            // Planning asks twice, and the first frame's write once.
            m.stated_grid = Some(changed.clone());
            m.grid_changes_after = Some(3);
            let mut r = request(&out);
            r.version = Some(version);
            let e = run(&[&m], &r).unwrap_err();
            assert_eq!(e.phase, "writing", "{name}");
            assert_eq!(e.step, Some(1), "{name}");
            assert!(e.to_string().contains("since planning"), "{name}: {e}");
            assert_eq!(m.decoded.get(), 1, "{name}: the changed frame was decoded");
            assert_eq!(read_back(&out, version).len(), 1, "{name}");
        }
    }
    // Control: the same source with no change writes both.
    let out = fresh("changes_none");
    let m = Memory::new("a.grib2", vec![hour(0), hour(1)]);
    assert_eq!(run(&[&m], &request(&out)).unwrap().len(), 2);
}

#[test]
fn a_warp_with_no_marker_is_refused_while_planning() {
    let out = fresh("no_marker");
    let mut m = Memory::new("a.grib2", vec![hour(0)]);
    m.stated_grid = Some(SourceGridDef {
        nx: NX,
        ny: NY,
        transform: transform(),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: None,
        stored_wkt: None,
    });
    let mut r = request(&out);
    r.cell_size = Some(2000.0);
    let e = run(&[&m], &r).unwrap_err();
    assert_eq!(e.phase, "planning");
    assert!(e.to_string().contains("no nodata marker"), "{e}");
    assert_eq!(m.decoded.get(), 0);
    assert!(!out.exists());
}

#[test]
fn a_nan_marker_is_the_same_grid_from_plan_to_write() {
    // NaN != NaN, so comparing markers as numbers would refuse every NaN source.
    let out = fresh("nan_marker");
    let mut m = Memory::new("a.grib2", vec![hour(0), hour(1)]);
    m.nodata = f32::NAN;
    assert_eq!(run(&[&m], &request(&out)).unwrap().len(), 2);
}

#[test]
fn transforms_with_no_true_dss_form_are_refused_before_any_cell_is_decoded() {
    let t = |origin_x, dx, row_rotation, origin_y, column_rotation, dy| GeoTransform {
        origin_x,
        dx,
        row_rotation,
        origin_y,
        column_rotation,
        dy,
    };
    let cases = [
        (
            "negative dx",
            t(10000.0, -2000.0, 0.0, 8000.0, 0.0, -2000.0),
        ),
        (
            "negative dx, south-up",
            t(10000.0, -2000.0, 0.0, 2000.0, 0.0, 2000.0),
        ),
        ("zero dx", t(2000.0, 0.0, 0.0, 8000.0, 0.0, -2000.0)),
        ("zero dy", t(2000.0, 2000.0, 0.0, 8000.0, 0.0, 0.0)),
        (
            "row rotation",
            t(2000.0, 2000.0, 100.0, 8000.0, 0.0, -2000.0),
        ),
        (
            "column rotation",
            t(2000.0, 2000.0, 0.0, 8000.0, 100.0, -2000.0),
        ),
        ("rotated 90", t(2000.0, 0.0, 2000.0, 8000.0, 2000.0, 0.0)),
    ];
    for version in [7u8, 6] {
        for align in [Align::Refuse, Align::Vortex] {
            for warped in [false, true] {
                for (name, transform) in &cases {
                    // A warp handles negative dx, and is tested through the public API.
                    if warped && name.starts_with("negative") {
                        continue;
                    }
                    let out = fresh("no_form");
                    let mut m = Memory::new("a.grib2", vec![hour(0)]);
                    m.stated_grid = Some(SourceGridDef {
                        nx: NX,
                        ny: NY,
                        transform: *transform,
                        crs: Crs::Named(NamedCrs::Shg),
                        nodata: Some(-9999.0),
                        stored_wkt: None,
                    });
                    let mut r = request(&out);
                    r.version = Some(version);
                    r.align = align;
                    if warped {
                        r.cell_size = Some(2000.0);
                    }
                    let e = run(&[&m], &r).unwrap_err();
                    assert_eq!(e.phase, "planning", "{name}, warped {warped}");
                    assert_eq!(m.decoded.get(), 0, "{name}, warped {warped}");
                    assert!(!out.exists(), "{name}, warped {warped}");
                }
            }
        }
    }
    // Control: planning accepts south-up. The public API tests hold where it is stored.
    let out = fresh("south_up_control");
    let mut m = Memory::new("a.grib2", vec![hour(0)]);
    m.stated_grid = Some(SourceGridDef {
        nx: NX,
        ny: NY,
        transform: GeoTransform::axis_aligned(2000.0, 2000.0, 2000.0, 2000.0),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: Some(-9999.0),
        stored_wkt: None,
    });
    // Only the write fails here: this source decodes north-up, and the plan said south-up.
    let e = run(&[&m], &request(&out)).unwrap_err();
    assert_eq!(e.phase, "writing");
    assert!(e.to_string().contains("grid"), "{e}");
}

#[test]
fn a_crash_at_every_write_step_leaves_other_records_and_a_rerun_heals() {
    for version in [7u8, 6] {
        let clean_out = fresh(&format!("clean{version}"));
        let m = Memory::new("a.grib2", vec![hour(0), hour(1), hour(2)]);
        let mut clean = request(&clean_out);
        clean.version = Some(version);
        run(&[&m], &clean).unwrap();
        let reference = read_back(&clean_out, version);
        assert_eq!(reference.len(), 3);
        let names: Vec<String> = reference.keys().cloned().collect();
        let mut stopped = 0;
        for (i, step) in WriteStep::ALL.into_iter().enumerate() {
            let out = fresh(&format!("crash{version}_{i}"));
            let mut r = request(&out);
            r.version = Some(version);
            r.fault = Some((1, FaultPoint { block: 0, step }));
            let result = run(&[&m], &r);
            let Err(e) = result else {
                continue; // This step is not on this write's path.
            };
            stopped += 1;
            assert_eq!(e.step, Some(1));
            assert_eq!(e.completed.len(), 1);
            // The finished record is whole. The stopped one is absent or whole, never partial.
            let after = read_back(&out, version);
            assert_eq!(
                bits(&after[&names[0]]),
                bits(&reference[&names[0]]),
                "{step:?}"
            );
            assert!(!after.contains_key(&names[2]), "{step:?}");
            if let Some(g) = after.get(&names[1]) {
                assert_eq!(bits(g), bits(&reference[&names[1]]), "{step:?}");
            }
            // The same run again finishes the file, as a clean run would.
            r.fault = None;
            run(&[&m], &r).unwrap();
            let healed = read_back(&out, version);
            assert_eq!(healed.len(), 3, "{step:?}");
            for name in &names {
                assert_eq!(
                    bits(&healed[name]),
                    bits(&reference[name]),
                    "{step:?} {name}"
                );
            }
        }
        assert!(
            stopped >= 5,
            "only {stopped} steps stopped a DSS{version} write"
        );
    }
}

#[test]
fn an_existing_file_keeps_its_version_and_other_records() {
    let out = fresh("existing");
    let m = Memory::new("a.grib2", vec![hour(0)]);
    run(&[&m], &request(&out)).unwrap();
    let before = fs::read(&out).unwrap();
    let n = Memory::new("b.grib2", vec![hour(1)]);
    let mut wrong = request(&out);
    wrong.version = Some(6);
    let e = run(&[&n], &wrong).unwrap_err();
    assert!(e.to_string().contains("DSS7"), "{e}");
    assert_eq!(
        fs::read(&out).unwrap(),
        before,
        "a refusal must not touch the file"
    );
    // Not a DSS file: refused, and not overwritten.
    let junk = fresh("junk");
    fs::write(&junk, b"not a DSS file at all").unwrap();
    let e = run(&[&m], &request(&junk)).unwrap_err();
    assert_eq!(e.phase, "planning");
    assert_eq!(fs::read(&junk).unwrap(), b"not a DSS file at all");
    // A record already there stays when another is added.
    let first = read_back(&out, 7);
    run(&[&n], &request(&out)).unwrap();
    let both = read_back(&out, 7);
    assert_eq!(both.len(), 2);
    for (name, g) in &first {
        assert_eq!(bits(&both[name]), bits(g));
    }
}

#[test]
fn limits_refuse_before_any_cell_is_decoded() {
    let out = fresh("limits");
    let m = Memory::new("a.grib2", vec![hour(0), hour(1)]);
    // Records.
    let mut r = request(&out);
    r.limits.max_records = 1;
    assert!(run(&[&m], &r)
        .unwrap_err()
        .to_string()
        .contains("limit of 1 records"));
    // Source cells, on any source.
    let mut r = request(&out);
    r.limits.max_source_cells = NX * NY - 1;
    assert!(run(&[&m], &r).unwrap_err().to_string().contains("source"));
    // Destination cells, unwarped.
    let mut r = request(&out);
    r.limits.max_destination_cells = (NX * NY - 1) as u64;
    assert!(run(&[&m], &r)
        .unwrap_err()
        .to_string()
        .contains("writer's limit"));
    // Destination cells, warped: a whole-US request at 1 m.
    let mut r = request(&out);
    r.target = Some(Crs::Named(NamedCrs::Shg));
    r.cell_size = Some(1.0);
    r.bounds = Bounds::Envelope {
        envelope: Envelope {
            min_x: 0.0,
            max_x: 100_000.0,
            min_y: 0.0,
            max_y: 100_000.0,
        },
        crs: None,
    };
    let e = run(&[&m], &r).unwrap_err();
    assert_eq!(e.phase, "planning");
    assert_eq!(m.decoded.get(), 0);
    assert!(!out.exists());
    assert_eq!(
        ImportLimits::default().max_destination_cells,
        dss_core::grid_write::MAX_WRITE_CELLS as u64
    );
    assert!(
        ImportLimits::default().max_source_cells
            < dss_vortex::warp::WarpLimits::default().max_cells as usize
    );
}

fn arrays(times: Vec<FrameTime>) -> ArrayInput {
    ArrayInput {
        cells: vec![1.5; NX * NY * times.len()],
        nx: NX,
        ny: NY,
        transform: transform(),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: -9999.0,
        times,
        variable: "Precip".into(),
        description: None,
        units: "mm".into(),
        file_name: String::new(),
    }
}

fn import_arrays(input: &ArrayInput, r: &ImportRequest) -> Result<usize, import::ImportError> {
    import::import_arrays(input, r, &mut |_| ControlFlow::Continue(())).map(|v| v.len())
}

#[test]
fn an_instant_with_a_period_type_needs_an_explicit_window() {
    let out = fresh("instant_policy");
    let mut r = request(&out);
    r.data_type = Some(DataType::PerCum);
    let input = arrays(vec![FrameTime::Instant((JUNE15, 3600))]);
    let e = import_arrays(&input, &r).unwrap_err();
    assert!(e.to_string().contains("needs a period"), "{e}");
    assert!(!out.exists());
    // With the window stated, the same cells are stored as a period.
    assert_eq!(import_arrays(&arrays(vec![hour(1)]), &r).unwrap(), 1);
    assert!(read_back(&out, 7).contains_key("/A/B/PRECIPITATION/15JUN2024:0100/15JUN2024:0200/F/"));
}

#[test]
fn a_time_with_seconds_and_an_unsnapped_origin_are_refused() {
    let out = fresh("refusals");
    let r = request(&out);
    let seconds = arrays(vec![FrameTime::Period {
        start: (JUNE15, 3630),
        end: (JUNE15, 7230),
    }]);
    assert!(import_arrays(&seconds, &r)
        .unwrap_err()
        .to_string()
        .contains("whole minute"));
    let mut unsnapped = arrays(vec![hour(0)]);
    unsnapped.transform = GeoTransform::axis_aligned(2500.0, 2000.0, 8000.0, -2000.0);
    let e = import_arrays(&unsnapped, &r).unwrap_err().to_string();
    assert!(e.contains("Observed behaviour we did not implement"), "{e}");
    assert!(!out.exists());
    // The opt-in gives what Vortex stores: the origin rounded to whole cells.
    let mut opt_in = request(&out);
    opt_in.align = Align::Vortex;
    assert_eq!(import_arrays(&unsnapped, &opt_in).unwrap(), 1);
    let grids = read_back(&out, 7);
    let g = grids.values().next().unwrap();
    assert_eq!((g.lower_left_cell_x, g.lower_left_cell_y), (1, 1));
}

#[test]
fn bad_arrays_are_refused() {
    let out = fresh("bad_arrays");
    let r = request(&out);
    let mut short = arrays(vec![hour(0)]);
    short.cells.pop();
    assert!(import_arrays(&short, &r).is_err());
    assert!(import_arrays(&arrays(vec![]), &r).is_err());
    let mut unnamed = arrays(vec![hour(0)]);
    unnamed.variable.clear();
    assert!(import_arrays(&unnamed, &r).is_err());
    let mut zero = arrays(vec![hour(0)]);
    zero.nx = 0;
    assert!(import_arrays(&zero, &r).is_err());
    let mut undefined = arrays(vec![hour(0)]);
    undefined.crs = Crs::Undefined;
    assert!(import_arrays(&undefined, &r).is_err());
    // A name that would shift pathname parts.
    let mut slash = arrays(vec![hour(0)]);
    slash.variable = "PRE/CIP".into();
    assert!(import_arrays(&slash, &r).is_err());
    let mut parts = request(&out);
    parts.parts.b = "x/y".into();
    assert!(import_arrays(&arrays(vec![hour(0)]), &parts).is_err());
    assert!(!out.exists());
}

// --- Shapefile input.

fn polygon_shp(points: &[(f64, f64)]) -> Vec<u8> {
    let mut content = Vec::new();
    content.extend_from_slice(&5i32.to_le_bytes());
    for v in [0.0f64, 0.0, 10.0, 10.0] {
        content.extend_from_slice(&v.to_le_bytes());
    }
    content.extend_from_slice(&1i32.to_le_bytes());
    content.extend_from_slice(&(points.len() as i32).to_le_bytes());
    content.extend_from_slice(&0i32.to_le_bytes());
    for (x, y) in points {
        content.extend_from_slice(&x.to_le_bytes());
        content.extend_from_slice(&y.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&9994i32.to_be_bytes());
    out.extend_from_slice(&[0u8; 20]);
    let total = (100 + 8 + content.len()) as i32 / 2;
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&1000i32.to_le_bytes());
    out.extend_from_slice(&5i32.to_le_bytes());
    for v in [0.0f64, 0.0, 10.0, 10.0, 0.0, 0.0, 0.0, 0.0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&(content.len() as i32 / 2).to_be_bytes());
    out.extend_from_slice(&content);
    out
}

const PRJ_FEET: &str = "PROJCS[\"NAD83 Albers feet\",GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]],PROJECTION[\"Albers_Conic_Equal_Area\"],PARAMETER[\"latitude_of_center\",23],PARAMETER[\"longitude_of_center\",-96],PARAMETER[\"standard_parallel_1\",29.5],PARAMETER[\"standard_parallel_2\",45.5],PARAMETER[\"false_easting\",1000],PARAMETER[\"false_northing\",2000],UNIT[\"US survey foot\",0.304800609601219]]";

#[test]
fn a_shapefile_gives_its_points_union_and_its_prj() {
    let out = fresh("shp");
    let shp = out.with_file_name("a.shp");
    let square = [(1.0, 2.0), (1.0, 8.0), (7.0, 8.0), (7.0, 2.0), (1.0, 2.0)];
    fs::write(&shp, polygon_shp(&square)).unwrap();
    // No .prj: the coordinates' CRS is not known, and it is not guessed.
    let e = import::shapefile_envelope(&shp).unwrap_err().to_string();
    assert!(e.contains("no .prj"), "{e}");
    fs::write(out.with_file_name("a.prj"), PRJ_FEET).unwrap();
    let (envelope, crs) = import::shapefile_envelope(&shp).unwrap();
    // US survey feet become metres, as `pj_inv` takes them, with the false origin scaled too.
    let foot = 0.304800609601219;
    assert_eq!(envelope.min_x, 1.0 * foot);
    assert_eq!(envelope.max_y, 8.0 * foot);
    match crs.projection {
        dss_vortex::crs::Projection::Albers { x_0, y_0, .. } => {
            assert_eq!(x_0, 1000.0 * foot);
            assert_eq!(y_0, 2000.0 * foot);
        }
        other => panic!("{other:?}"),
    }
    // A missing file is refused rather than meaning no envelope.
    let gone = out.with_file_name("gone.shp");
    let e = import::shapefile_envelope(&gone).unwrap_err().to_string();
    assert!(e.contains("Observed behaviour we did not implement"), "{e}");
}

// A unit the feet path does not read must never be taken as metres: the envelope would move.
#[test]
fn a_prj_whose_unit_cannot_be_read_is_refused_and_not_taken_as_metres() {
    let out = fresh("shp_unit");
    let shp = out.with_file_name("a.shp");
    fs::write(
        &shp,
        polygon_shp(&[(1.0, 2.0), (1.0, 8.0), (7.0, 8.0), (1.0, 2.0)]),
    )
    .unwrap();
    let prj = out.with_file_name("a.prj");
    let authority = PRJ_FEET.replace(
        "0.304800609601219]",
        "0.304800609601219,AUTHORITY[\"EPSG\",\"9003\"]]",
    );
    assert_ne!(authority, PRJ_FEET, "the control must change the text");
    fs::write(&prj, &authority).unwrap();
    match import::shapefile_envelope(&shp) {
        Err(_) => {}
        Ok((envelope, _)) => assert!(
            (envelope.min_x - 0.304800609601219).abs() < 1e-9,
            "feet taken as metres: {envelope:?}"
        ),
    }
    // A .prj too long to be a CRS is refused before it is read.
    fs::write(&prj, vec![b' '; (1 << 20) + 1]).unwrap();
    let e = import::shapefile_envelope(&shp).unwrap_err().to_string();
    assert!(e.contains(".prj is over"), "{e}");
    // The readable form still converts, so the checks above can tell the routes apart.
    fs::write(&prj, PRJ_FEET).unwrap();
    let (envelope, _) = import::shapefile_envelope(&shp).unwrap();
    assert!(
        (envelope.min_x - 0.304800609601219).abs() < 1e-9,
        "{envelope:?}"
    );
}

#[test]
fn malformed_shapefiles_are_typed_errors() {
    let out = fresh("shp_fuzz");
    let shp = out.with_file_name("a.shp");
    fs::write(out.with_file_name("a.prj"), PRJ_FEET).unwrap();
    let good = polygon_shp(&[(1.0, 2.0), (1.0, 8.0), (7.0, 8.0), (1.0, 2.0)]);
    // Fixed: a declared point count of i32::MAX, a negative count, huge parts, empty files.
    let mut huge = good.clone();
    huge[100 + 8 + 4 + 32 + 4..100 + 8 + 4 + 32 + 8].copy_from_slice(&i32::MAX.to_le_bytes());
    let mut negative = good.clone();
    negative[100 + 8 + 4 + 32 + 4..100 + 8 + 4 + 32 + 8].copy_from_slice(&(-5i32).to_le_bytes());
    let mut parts = good.clone();
    parts[100 + 8 + 4 + 32..100 + 8 + 4 + 32 + 4].copy_from_slice(&i32::MAX.to_le_bytes());
    let mut nan = good.clone();
    let p = 100 + 8 + 4 + 32 + 8 + 4;
    nan[p..p + 8].copy_from_slice(&f64::INFINITY.to_le_bytes());
    for (name, bytes) in [
        ("huge", &huge[..]),
        ("negative", &negative[..]),
        ("parts", &parts[..]),
        ("infinite", &nan[..]),
        ("empty", &good[..0]),
        ("header", &good[..100]),
        ("truncated", &good[..good.len() - 3]),
    ] {
        fs::write(&shp, bytes).unwrap();
        assert!(import::shapefile_envelope(&shp).is_err(), "{name}");
    }
    // Mutations: never a panic out of this function, whatever comes back.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..2000 {
        let mut bytes = good.clone();
        for _ in 0..1 + next() % 4 {
            let at = (next() % bytes.len() as u64) as usize;
            bytes[at] = (next() & 0xFF) as u8;
        }
        if next() % 5 == 0 {
            bytes.truncate((next() % bytes.len() as u64) as usize);
        }
        fs::write(&shp, &bytes).unwrap();
        let _ = import::shapefile_envelope(&shp);
    }
}

// After a crash the walk can list an entry the file's lookup does not reach yet: that record was not
// published, so it is absent. Any other read failure fails the test.
fn readable(read: Result<Grid, dss_core::GridError>) -> Option<Grid> {
    match read {
        Ok(g) => Some(g),
        Err(dss_core::GridError::NotFound) => None,
        Err(other) => panic!("{other}"),
    }
}

// Hostile GRIB2 through the whole route, on the file path: a typed error or a record, never a panic,
// and an error leaves no partial record. The reader's own fuzz is `grib_fuzz.rs`.
#[test]
fn mutated_grib_inputs_never_panic_and_never_leave_a_partial_record() {
    let good = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public/vortex_p46/apcp_joined.grib2"),
    )
    .unwrap();
    let out = fresh("grib_fuzz");
    let input = out.with_file_name("x.grib2");
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut wrote, mut refused) = (0, 0);
    for _ in 0..400 {
        let mut bytes = good.clone();
        for _ in 0..1 + next() % 6 {
            let at = (next() % bytes.len() as u64) as usize;
            bytes[at] = (next() & 0xFF) as u8;
        }
        if next() % 4 == 0 {
            bytes.truncate((next() % bytes.len() as u64) as usize);
        }
        fs::write(&input, &bytes).unwrap();
        let _ = fs::remove_file(&out);
        let mut r = ImportRequest::new(
            vec![input.clone()],
            out.clone(),
            Selection::Variable("Total_precipitation_surface_1_Hour_Accumulation".into()),
            Method::Near,
        );
        r.sync = dss_core::dss7::write::SyncMode::None;
        r.align = Align::Vortex;
        r.duplicates = Duplicates::Last;
        match import::import_gridded(&r, &mut |_| ControlFlow::Continue(())) {
            Ok(done) => {
                wrote += 1;
                assert_eq!(read_back(&out, 7).len(), done.len());
            }
            Err(e) => {
                refused += 1;
                if out.exists() {
                    assert_eq!(read_back(&out, 7).len(), e.completed.len(), "{e}");
                } else {
                    assert!(e.completed.is_empty(), "{e}");
                }
            }
        }
    }
    assert!(wrote > 0 && refused > 0, "wrote {wrote}, refused {refused}");
}

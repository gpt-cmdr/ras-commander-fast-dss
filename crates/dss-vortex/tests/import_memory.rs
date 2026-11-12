//! Streaming, measured: what an import holds must not grow with the number of frames or inputs.
//! One test in its own binary, because the recording allocator sees the whole process.

use dss_vortex::frame::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
use dss_vortex::import::{
    self, GridSource, ImportRequest, Selection, SourceGridDef, SourceProvider, StepMeta,
};
use dss_vortex::warp::Method;
use dss_vortex::VortexError;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

struct Recording;
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System`, and only counts.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            PEAK.fetch_max(NOW.fetch_add(l.size(), SeqCst) + l.size(), SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), SeqCst);
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

const NX: usize = 400;
const NY: usize = 300;
const FRAME_BYTES: usize = NX * NY * 4;

struct Synthetic {
    index: usize,
}

fn time(index: usize) -> FrameTime {
    let t = index as i64 * 3600;
    let split = |s: i64| (45_000 + s.div_euclid(86_400), s.rem_euclid(86_400));
    FrameTime::Period {
        start: split(t),
        end: split(t + 3600),
    }
}

fn transform() -> GeoTransform {
    GeoTransform::axis_aligned(0.0, 2000.0, (NY * 2000) as f64, -2000.0)
}

impl GridSource for Synthetic {
    fn file_name(&self) -> &str {
        "memory"
    }

    fn variables(&self) -> Vec<String> {
        vec!["Precip".into()]
    }

    fn step_count(&self, _: &str) -> Result<usize, VortexError> {
        Ok(1)
    }

    fn step_meta(&self, _: &str, _: usize) -> Result<StepMeta, VortexError> {
        Ok(StepMeta {
            time: time(self.index),
            units: "mm".into(),
            description: None,
            source_type: DataType::PerCum,
        })
    }

    fn grid(&self, _: &str, _: usize) -> Result<SourceGridDef, VortexError> {
        Ok(SourceGridDef {
            nx: NX,
            ny: NY,
            transform: transform(),
            crs: Crs::Named(NamedCrs::Shg),
            nodata: Some(-9999.0),
            stored_wkt: None,
        })
    }

    fn frame(&self, _: &str, _: usize) -> Result<GridFrame, VortexError> {
        Ok(GridFrame::new(GridFrameParts {
            cells: (0..NX * NY).map(|i| (i + self.index) as f32).collect(),
            nx: NX,
            ny: NY,
            row_order: RowOrder::NorthToSouth,
            transform: transform(),
            crs: Crs::Named(NamedCrs::Shg),
            nodata: Some(-9999.0),
            time: time(self.index),
            data_type: DataType::PerCum,
            units: "mm".into(),
            variable: "Precip".into(),
            description: None,
        })?)
    }
}

struct Many(usize);

impl SourceProvider for Many {
    fn len(&self) -> usize {
        self.0
    }

    fn path(&self, index: usize) -> PathBuf {
        PathBuf::from(format!("input{index}"))
    }

    fn open(&self, index: usize) -> Result<Box<dyn GridSource + '_>, VortexError> {
        Ok(Box::new(Synthetic { index }))
    }
}

fn peak_of(inputs: usize, name: &str) -> usize {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("import_memory")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let mut r = ImportRequest::new(
        Vec::new(),
        dir.join("out.dss"),
        Selection::Variable("Precip".into()),
        Method::Near,
    );
    r.sync = dss_core::dss7::write::SyncMode::None;
    let start = NOW.load(SeqCst);
    PEAK.store(start, SeqCst);
    let written = import::run(&Many(inputs), &r, &mut |_| ControlFlow::Continue(())).unwrap();
    let peak = PEAK.load(SeqCst) - start;
    assert_eq!(written.len(), inputs);
    peak
}

#[test]
fn memory_does_not_grow_with_inputs_or_frames() {
    let few = peak_of(5, "few");
    let many = peak_of(50, "many");
    eprintln!("peak with 5 inputs {few}, with 50 inputs {many}, one frame {FRAME_BYTES}");
    // 45 more frames would add over 21 MB if cells were kept. A plan row and a returned record are
    // a few hundred bytes each.
    assert!(
        many < few + FRAME_BYTES / 2,
        "peak {few} with 5 inputs and {many} with 50, a frame being {FRAME_BYTES}"
    );
    // A frame and its compressed copy, with working space; well under the 50 frames' 24 MB.
    assert!(
        many < 6 * FRAME_BYTES,
        "peak {many}, a frame being {FRAME_BYTES}"
    );
}

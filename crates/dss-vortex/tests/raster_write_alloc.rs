//! A failed allocation during encoding must be an error, not an abort. The allocator denies the
//! Nth request made after the encoder's first write, on this thread, for every N the encoder
//! makes, and then all of them. An abort kills this test binary, so the test fails.
//! The second test denies each grid-sized request through the public writers, from the first.

use dss_vortex::sink_raster::{
    encode_asc_to, encode_geotiff_to, write_asc, write_geotiff, AscGeoref, AscOptions,
    NodataPolicy, TiffOptions,
};
use dss_vortex::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Seek, SeekFrom, Write};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static SEEN: Cell<usize> = const { Cell::new(0) };
    /// Deny the request with this index; `DENY_NONE` denies none, and `DENY_ALL` every one.
    static DENY_AT: Cell<usize> = const { Cell::new(DENY_NONE) };
    /// Only requests at least this large count. One byte counts them all.
    static AT_LEAST: Cell<usize> = const { Cell::new(1) };
}
const DENY_NONE: usize = usize::MAX;
const DENY_ALL: usize = usize::MAX - 1;

struct Denying;

fn denied(size: usize) -> bool {
    if !ARMED.with(Cell::get) || size < AT_LEAST.with(Cell::get) {
        return false;
    }
    let seen = SEEN.with(|s| {
        let v = s.get();
        s.set(v + 1);
        v
    });
    let at = DENY_AT.with(Cell::get);
    at == DENY_ALL || at == seen
}

unsafe impl GlobalAlloc for Denying {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if denied(layout.size()) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if denied(layout.size()) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if denied(new_size) {
            return std::ptr::null_mut();
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Denying = Denying;

/// A fixed buffer: the writer itself asks for nothing, and arms the allocator on its first write.
struct Sink {
    bytes: Vec<u8>,
    at: usize,
    end: usize,
}

impl Sink {
    fn new() -> Self {
        Sink {
            bytes: vec![0; 1 << 22],
            at: 0,
            end: 0,
        }
    }
}

impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        ARMED.with(|a| a.set(true));
        self.bytes[self.at..self.at + data.len()].copy_from_slice(data);
        self.at += data.len();
        self.end = self.end.max(self.at);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Sink {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.at = match to {
            SeekFrom::Start(n) => n as usize,
            SeekFrom::Current(n) => (self.at as i64 + n) as usize,
            SeekFrom::End(n) => (self.end as i64 + n) as usize,
        };
        Ok(self.at as u64)
    }
}

fn frame() -> GridFrame {
    let (nx, ny) = (64usize, 40usize);
    let cells: Vec<f32> = (0..nx * ny).map(|i| i as f32 * 0.37 + 0.125).collect();
    GridFrame::new(GridFrameParts {
        cells,
        nx,
        ny,
        row_order: RowOrder::NorthToSouth,
        transform: GeoTransform::axis_aligned(-646000.0, 2000.0, 2124000.0, -2000.0),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: Some(-9999.0),
        time: FrameTime::Invariant,
        data_type: DataType::InstVal,
        units: String::new(),
        variable: String::new(),
        description: None,
    })
    .unwrap()
}

type Encode<'a> = &'a dyn Fn(&mut Sink) -> Result<(), dss_vortex::VortexError>;

/// One encode under a denial; the result is only inspected once the allocator is disarmed.
fn run(deny: usize, encode: Encode, sink: &mut Sink) -> (usize, bool) {
    sink.at = 0;
    sink.end = 0;
    SEEN.with(|s| s.set(0));
    DENY_AT.with(|d| d.set(deny));
    let result = encode(sink);
    ARMED.with(|a| a.set(false));
    DENY_AT.with(|d| d.set(DENY_NONE));
    (SEEN.with(Cell::get), result.is_ok())
}

#[test]
fn a_denied_allocation_during_encoding_is_an_error_not_an_abort() {
    let good = frame();
    let mut sink = Sink::new();
    let tiff_cases = [
        ("deflate bigtiff", TiffOptions::default()),
        (
            "plain classic",
            TiffOptions {
                nodata: NodataPolicy::Keep,
                bigtiff: false,
                deflate: false,
            },
        ),
        ("vortex", TiffOptions::vortex()),
    ];
    for (label, options) in &tiff_cases {
        let encode = |s: &mut Sink| encode_geotiff_to(&mut *s, &good, options);
        let (requests, ok) = run(DENY_NONE, &encode, &mut sink);
        assert!(ok, "{label}: the baseline writes");
        assert!(sink.end > 1000, "{label}: wrote {}", sink.end);
        for n in 0..requests {
            let (_, ok) = run(n, &encode, &mut sink);
            assert!(
                !ok,
                "{label}: request {n} of {requests} was denied and it still claimed success"
            );
        }
        // Getting past this line is what it asserts: nothing aborted with every request denied.
        run(DENY_ALL, &encode, &mut sink);
    }
    for georef in [AscGeoref::RoundTrip, AscGeoref::Vortex] {
        let encode = |s: &mut Sink| encode_asc_to(&mut *s, &good, georef);
        let (requests, ok) = run(DENY_NONE, &encode, &mut sink);
        assert!(ok);
        assert_eq!(
            requests, 0,
            "a grid's text allocates nothing per cell, {georef:?}"
        );
        let (_, ok) = run(DENY_ALL, &encode, &mut sink);
        assert!(ok, "{georef:?}");
    }
}

/// Requests sized by a grid of a few MiB. The fixed buffers are below this, and not sized by a frame.
const GRID_SIZED: usize = 128 * 1024;

fn large_frame() -> GridFrame {
    let (nx, ny) = (1024usize, 512usize);
    let cells: Vec<f32> = (0..nx * ny).map(|i| (i % 8191) as f32 * 0.37).collect();
    GridFrame::new(GridFrameParts {
        cells,
        nx,
        ny,
        row_order: RowOrder::NorthToSouth,
        transform: GeoTransform::axis_aligned(0.0, 1.0, 512.0, -1.0),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: Some(-9999.0),
        time: FrameTime::Invariant,
        data_type: DataType::InstVal,
        units: String::new(),
        variable: String::new(),
        description: None,
    })
    .unwrap()
}

/// One public write with the allocator armed from its first request.
fn armed_write(
    deny: usize,
    write: &dyn Fn() -> Result<(), dss_vortex::VortexError>,
) -> (usize, bool) {
    SEEN.with(|s| s.set(0));
    AT_LEAST.with(|a| a.set(GRID_SIZED));
    DENY_AT.with(|d| d.set(deny));
    ARMED.with(|a| a.set(true));
    let result = write();
    ARMED.with(|a| a.set(false));
    DENY_AT.with(|d| d.set(DENY_NONE));
    AT_LEAST.with(|a| a.set(1));
    (SEEN.with(Cell::get), result.is_ok())
}

#[test]
fn a_denied_grid_sized_allocation_through_the_writers_leaves_the_target_whole() {
    let dir = std::env::temp_dir().join(format!("p50_write_alloc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let frame = large_frame();
    const ORIGINAL: &[u8] = b"ORIGINAL";
    for name in ["a.tif", "a.asc"] {
        let path = dir.join(name);
        std::fs::write(&path, ORIGINAL).unwrap();
        let write_to = |to: &std::path::Path| {
            if name == "a.tif" {
                write_geotiff(&frame, to, &TiffOptions::default())
            } else {
                write_asc(&frame, to, &AscOptions::default())
            }
        };
        let probe = dir.join(format!("probe_{name}"));
        let (requests, ok) = armed_write(DENY_NONE, &|| write_to(&probe));
        assert!(ok, "{name}: the baseline writes");
        assert!(requests >= 2, "{name}: {requests} grid-sized requests");
        for leftover in std::fs::read_dir(&dir).unwrap() {
            let leftover = leftover.unwrap().path();
            if leftover != path {
                std::fs::remove_file(leftover).unwrap();
            }
        }
        for n in 0..requests {
            let (_, ok) = armed_write(n, &|| write_to(&path));
            assert!(!ok, "{name}: request {n} of {requests} was denied");
            assert_eq!(std::fs::read(&path).unwrap(), ORIGINAL, "{name} {n}");
            let files = std::fs::read_dir(&dir).unwrap().count();
            assert_eq!(files, 1, "{name} {n}: a file was left behind");
        }
        std::fs::remove_file(&path).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

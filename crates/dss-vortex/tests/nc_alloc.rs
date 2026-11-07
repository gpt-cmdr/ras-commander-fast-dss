//! Size claims in a file must be checked against limits before anything sized by them is allocated.
//! An allocator records the largest request and can refuse requests over a limit. Fixtures here are
//! real HDF5 written by `oracle/vortex/make_nc_hostile.py`: each declares a large grid with little
//! or highly compressed data. One `#[test]` only, so no other test thread adds to the counts.

use dss_vortex::source::netcdf::{Limits, NetcdfFile};
use dss_vortex::VortexError;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Recording;

static LARGEST: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > LIMIT.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > LIMIT.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > LIMIT.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        LARGEST.fetch_max(new_size, Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

fn hostile(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/netcdf_hostile")
        .join(name)
}

const MIB: usize = 1 << 20;

fn is_refusal(error: &VortexError) -> bool {
    matches!(
        error,
        VortexError::Refused { .. } | VortexError::Decode { .. }
    )
}

fn read_first(name: &str, limits: Limits) -> Result<usize, VortexError> {
    let file = NetcdfFile::open_with_limits(&hostile(name), limits)?;
    let names = file.variable_names();
    let v = file.variable(&names[0])?;
    Ok(v.step(0)?.cells.len())
}

#[test]
fn claims_are_bounded_before_allocation() {
    // 40 MiB: the metadata probe reserves its 32 MiB and the estimate, whatever the grid claims.
    // A grid over max_cells is refused with no allocation near its size. 2^15 x 2^15 is 2^30 cells.
    LARGEST.store(0, Ordering::SeqCst);
    let error = read_first("fill_over_limit.nc4", Limits::default()).unwrap_err();
    assert!(is_refusal(&error), "{error}");
    assert!(error.to_string().contains("over the limit"), "{error}");
    let largest = LARGEST.load(Ordering::SeqCst);
    assert!(
        largest < 40 * MIB,
        "requested {largest} bytes for a refused grid"
    );

    // Within max_cells, with no chunks written: the read is only as large as the caller's limit
    // allows. A lower max_cells refuses it, again with no large request.
    LARGEST.store(0, Ordering::SeqCst);
    let limits = Limits {
        max_cells: 1 << 20,
        ..Limits::default()
    };
    let error = read_first("fill_16384.nc4", limits).unwrap_err();
    assert!(is_refusal(&error), "{error}");
    let largest = LARGEST.load(Ordering::SeqCst);
    assert!(
        largest < 40 * MIB,
        "requested {largest} bytes for a refused grid"
    );

    // Under default limits and a small memory ceiling, reading it is a typed error, not an abort.
    LIMIT.store(64 * MIB, Ordering::SeqCst);
    let error = read_first("fill_16384.nc4", Limits::default()).unwrap_err();
    assert!(error.to_string().contains("could not allocate"), "{error}");

    // One chunk of 256 MiB behind a step of 4096 cells. The reservation has to count the chunk, not
    // the cells. Without that, this read goes ahead, and it aborted under a 200 MiB process limit.
    let error = read_first("chunk_over_step.nc4", Limits::default()).unwrap_err();
    assert!(error.to_string().contains("could not allocate"), "{error}");

    // A deflate bomb: 256 MiB per chunk from about 256 KiB. The cell probe fails first here, and
    // with the cells fitting, the chunk's own allocation must be fallible too.
    let error = read_first("deflate_bomb.nc4", Limits::default()).unwrap_err();
    assert!(is_refusal(&error), "{error}");
    LIMIT.store(usize::MAX, Ordering::SeqCst);

    // max_chunk_bytes refuses the chunk before it is inflated, whatever the output buffer held.
    let limits = Limits {
        max_chunk_bytes: MIB,
        ..Limits::default()
    };
    let error = read_first("deflate_bomb.nc4", limits).unwrap_err();
    assert!(is_refusal(&error), "{error}");
    assert!(
        error.to_string().to_lowercase().contains("chunk"),
        "{error}"
    );

    // A file over the input limit is not read.
    let limits = Limits {
        max_input_bytes: 16,
        ..Limits::default()
    };
    let error = NetcdfFile::open_with_limits(&hostile("fill_16384.nc4"), limits).unwrap_err();
    assert!(error.to_string().contains("over the limit"), "{error}");
    let bytes = fs::read(hostile("fill_16384.nc4")).unwrap();
    assert!(NetcdfFile::from_bytes("x.nc4", bytes, limits).is_err());

    // Too many variables.
    let limits = Limits {
        max_variables: 1,
        ..Limits::default()
    };
    assert!(NetcdfFile::open_with_limits(&hostile("fill_16384.nc4"), limits).is_err());

    // An axis longer than max_axis_points is refused before it is read.
    let limits = Limits {
        max_axis_points: 8,
        ..Limits::default()
    };
    let error = read_first("fill_16384.nc4", limits).unwrap_err();
    assert!(error.to_string().contains("over the limit"), "{error}");
}

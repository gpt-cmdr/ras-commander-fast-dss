//! The estimate in `source/netcdf/budget.rs` must hold what the reader actually allocates. An
//! allocator here records live bytes and their peak, and refuses requests over a ceiling as a process
//! limit does, over every fixture and deterministic mutations of them. One `#[test]` only, so no other
//! thread adds to the counts.

mod nc_raw;

use dss_vortex::source::netcdf::{Limits, NetcdfFile};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Recording;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CEILING: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Requests at or over this are not counted: the metadata probe, which is released untouched.
static PROBE: AtomicUsize = AtomicUsize::new(usize::MAX);

fn added(bytes: usize) {
    if bytes < PROBE.load(Ordering::SeqCst) {
        let live = LIVE.fetch_add(bytes, Ordering::SeqCst) + bytes;
        PEAK.fetch_max(live, Ordering::SeqCst);
    }
}

fn removed(bytes: usize) {
    if bytes < PROBE.load(Ordering::SeqCst) {
        LIVE.fetch_sub(bytes, Ordering::SeqCst);
    }
}

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > CEILING.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        added(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > CEILING.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        added(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        removed(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > CEILING.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        removed(layout.size());
        added(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

const MIB: usize = 1 << 20;

/// What does not scale with the file: one `with_capacity` of a 16-bit count, thread state, the
/// walk's tables on a small file.
const FIXED: usize = 4 * MIB;

#[derive(Default)]
struct Measure {
    largest_ratio: f64,
    largest_estimate: u64,
    opened: usize,
    refused: usize,
}

fn measure(name: &str, bytes: Vec<u8>, seen: &mut Measure) {
    let input = bytes.len();
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    // The probe asks for at least this. It is released without being touched, and it is what stands
    // in for the reader's allocations when memory is short, so it is not part of what they use.
    PROBE.store(32 * MIB, Ordering::SeqCst);
    let opened = NetcdfFile::from_bytes(name, bytes, Limits::default());
    PROBE.store(usize::MAX, Ordering::SeqCst);
    let peak = PEAK.load(Ordering::SeqCst).saturating_sub(before);
    match opened {
        Ok(file) => {
            seen.opened += 1;
            let estimate = file.metadata_estimate() as usize;
            assert!(
                peak <= input + estimate + FIXED,
                "{name}: peak {peak} over input {input} + estimate {estimate}"
            );
            seen.largest_estimate = seen.largest_estimate.max(estimate as u64);
            if estimate > 0 {
                let used = peak.saturating_sub(input) as f64 / estimate as f64;
                seen.largest_ratio = seen.largest_ratio.max(used);
            }
        }
        Err(_) => {
            seen.refused += 1;
            // A refusal costs at most the limit it refers to, whatever the file claims.
            assert!(
                peak <= input + 64 * MIB + FIXED,
                "{name}: refused after a peak of {peak}"
            );
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn what_the_reader_uses_stays_within_the_estimate() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/netcdf");
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_none_or(|e| e != "json"))
        .collect();
    paths.sort();
    let mut seen = Measure::default();
    let mut rng = Rng(0x2545F4914F6CDD1D);
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let original = fs::read(path).unwrap();
        measure(&name, original.clone(), &mut seen);
        if original.len() > 400_000 {
            continue;
        }
        for _ in 0..40 {
            let mut bytes = original.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(bytes.len().min(8192));
                bytes[at] = rng.next() as u8;
            }
            measure(&name, bytes, &mut seen);
        }
    }
    assert!(seen.opened > 50, "{}", seen.opened);
    println!(
        "opened {}, refused {}, largest peak over estimate {:.3}, largest estimate {}",
        seen.opened, seen.refused, seen.largest_ratio, seen.largest_estimate
    );

    // Every hostile shape, measured the same way, and each refused.
    let built: Vec<(&str, Vec<u8>)> = vec![
        ("cycle", nc_raw::group_cycle()),
        ("doubled", nc_raw::group_doubled(30)),
        ("deep", nc_raw::group_chain(33)),
        ("continuation", nc_raw::shared_continuation(4000, 1 << 20)),
        ("heap", nc_raw::repeated_heap_object(4000, 1 << 20)),
        ("name", nc_raw::repeated_name(60_000, 1 << 16)),
        ("shared", nc_raw::repeated_shared_message(3000, 1 << 20)),
        ("classic", nc_raw::classic_dimension_count(3_000_000)),
        (
            "clones",
            nc_raw::classic_repeated_dimension(1 << 20, 200_000),
        ),
    ];
    for (name, bytes) in built {
        let before = seen.refused;
        measure(name, bytes, &mut seen);
        assert_eq!(seen.refused, before + 1, "{name} was not refused");
    }

    // A user-defined attribute is counted field by element: 30,000 values from 40 KB, within the
    // estimate, and the overlapping shape never reaches the reader.
    let before = seen.opened;
    measure("plain", nc_raw::plain_compounds(1, 500, 60), &mut seen);
    measure("plain_small", nc_raw::plain_compound(20, 30), &mut seen);
    assert_eq!(seen.opened, before + 2);
    for (name, bytes) in [
        ("overlap", nc_raw::overlapping_compound(2142, 30_000)),
        ("sum", nc_raw::plain_compounds(12, 500, 60)),
    ] {
        let before = seen.refused;
        measure(name, bytes, &mut seen);
        assert_eq!(seen.refused, before + 1, "{name} was not refused");
    }

    // A read at max_chunks_per_read stays within what `check_read` reserves for its chunk tables.
    // The cells are 8 bytes each while stored and returned, at most.
    let at_limit = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public/netcdf_hostile/chunks_at_limit.nc4"),
    )
    .unwrap();
    let cells = 512 * 512 * 8;
    let file = NetcdfFile::from_bytes("limit.nc4", at_limit, Limits::default()).unwrap();
    let variable = file.variable("data").unwrap();
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    PROBE.store(32 * MIB, Ordering::SeqCst);
    variable.step(0).unwrap();
    PROBE.store(usize::MAX, Ordering::SeqCst);
    let peak = PEAK.load(Ordering::SeqCst).saturating_sub(before);
    let table = (1usize << 18) * 512;
    println!("read at the chunk limit: peak {peak}, tables {table}, cells {cells}");
    assert!(
        peak <= table + cells + FIXED,
        "peak {peak} over tables {table} and cells {cells}"
    );

    // Under a ceiling like a process limit's, nothing asks for more than the reader can soften: this
    // completes with a typed error, and an abort would fail the test binary. The review's file, with
    // its count low enough to pass the metadata limit, is what asked for 120 MB.
    CEILING.store(16 * MIB, Ordering::SeqCst);
    let error = NetcdfFile::from_bytes(
        "padded.nc",
        nc_raw::classic_dimension_count(1_000),
        Limits::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("could not allocate"), "{error}");
    CEILING.store(usize::MAX, Ordering::SeqCst);
}

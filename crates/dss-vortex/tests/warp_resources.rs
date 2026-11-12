//! What a warp may hold, measured by the allocator rather than by asking 8 GiB of the host.
//!
//! This file has one test, because the allocator's counters are process-wide.

use dss_vortex::crs::{CrsDef, Envelope};
use dss_vortex::frame::GeoTransform;
use dss_vortex::warp::{
    Method, SourceGrid, WarpEnvelope, WarpLimits, WarpPlan, WarpSpec, DEFAULT_MAX_CELLS,
    FORMAT_MAX_CELLS, MAX_WARP_THREADS,
};
use dss_vortex::VortexError;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

/// Records live and peak bytes, and can refuse the nth allocation at or above [`LARGE`].
struct Recording;

/// Above std's fixed bookkeeping for a thread, below anything the grids here scale up to.
const LARGE: usize = 4096;
const NEVER: usize = usize::MAX;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGE_SEEN: AtomicUsize = AtomicUsize::new(0);
static REFUSE_AT: AtomicUsize = AtomicUsize::new(NEVER);
static REFUSED: AtomicBool = AtomicBool::new(false);

// SAFETY: every call forwards to `System`; returning null is how an allocator reports failure.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= LARGE {
            let n = LARGE_SEEN.fetch_add(1, SeqCst);
            if n == REFUSE_AT.load(SeqCst) {
                REFUSED.store(true, SeqCst);
                return std::ptr::null_mut();
            }
        }
        let p = System.alloc(layout);
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), SeqCst) + layout.size();
            PEAK.fetch_max(live, SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        System.dealloc(p, layout);
        LIVE.fetch_sub(layout.size(), SeqCst);
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

const METHODS: [Method; 3] = [Method::Near, Method::Bilinear, Method::Average];
const OUT_NX: usize = 600;
const OUT_NY: usize = 2000;
const OUT_BYTES: u64 = (OUT_NX * OUT_NY * 4) as u64;

// A 600 x 2000 destination from a 300 x 300 source: the row references, the destination, the mask
// and each worker's scratch all reach LARGE.
fn source() -> SourceGrid {
    SourceGrid {
        nx: 300,
        ny: 300,
        transform: GeoTransform::axis_aligned(0.0, 10.0, 3000.0, -10.0),
        crs: CrsDef::shg(),
    }
}

fn spec(method: Method, threads: usize) -> WarpSpec {
    let mut s = WarpSpec::new(method);
    s.cell_size = Some(0.5);
    s.threads = threads;
    s.envelope = Some(WarpEnvelope {
        envelope: Envelope {
            min_x: 0.0,
            max_x: 300.0,
            min_y: 0.0,
            max_y: 1000.0,
        },
        crs: Some(CrsDef::shg()),
    });
    s
}

fn cells() -> Vec<f32> {
    (0..300 * 300)
        .map(|i| {
            if i % 97 == 0 {
                -1.0
            } else {
                (i % 91) as f32 * 0.5
            }
        })
        .collect()
}

fn warp(s: &WarpSpec, cells: &[f32]) -> Result<Vec<f32>, VortexError> {
    WarpPlan::new(&source(), s)?.warp_cells(cells, -1.0)
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|c| c.to_bits()).collect()
}

// Every large allocation in turn fails. A refusal is fine; aborting the process is the failure
// this test exists for, and it would take the test binary with it.
fn every_large_allocation_can_fail(data: &[f32]) {
    for method in METHODS {
        for threads in [1, 4] {
            let s = spec(method, threads);
            let want = bits(&warp(&s, data).unwrap());
            let mut refusals = 0;
            for n in 0.. {
                REFUSED.store(false, SeqCst);
                LARGE_SEEN.store(0, SeqCst);
                REFUSE_AT.store(n, SeqCst);
                let result = warp(&s, data);
                REFUSE_AT.store(NEVER, SeqCst);
                if !REFUSED.load(SeqCst) {
                    assert_eq!(bits(&result.unwrap()), want, "{method:?} {threads}");
                    break;
                }
                match result {
                    Err(VortexError::Refused { .. }) => refusals += 1,
                    Err(other) => panic!("{method:?} {threads} allocation {n}: {other:?}"),
                    Ok(out) => panic!(
                        "{method:?} {threads} allocation {n} failed, yet {} cells came back",
                        out.len()
                    ),
                }
            }
            // The destination, the row references, the source window, the mask and the scratch at least.
            assert!(refusals >= 5, "{method:?} {threads}: only {refusals}");
        }
    }
}

// The plan's own retained allocations count too: a budget set to the estimate must hold them and
// the warp together, measured from before the plan exists.
fn the_plan_and_the_warp_fit_a_budget_of_exactly_the_estimate(data: &[f32]) {
    for method in METHODS {
        let mut s = spec(method, 1);
        let before = LIVE.load(SeqCst);
        let estimate = WarpPlan::new(&source(), &s).unwrap().memory_estimate(-1.0);
        s.limits.max_memory_bytes = estimate;
        let plan = WarpPlan::new(&source(), &s).unwrap();
        PEAK.store(LIVE.load(SeqCst), SeqCst);
        let out = plan.warp_cells(data, -1.0).unwrap();
        let held = PEAK.load(SeqCst) - before;
        drop(out);
        assert!(held as u64 <= estimate, "{method:?}: {held} > {estimate}");
    }
}

// The budget's figure must hold what the allocator saw. Stacks are not in the allocator's count,
// so the estimate is above it by them when workers start.
fn the_estimate_covers_what_was_really_held(data: &[f32]) {
    for method in METHODS {
        for threads in [1, 4] {
            let plan = WarpPlan::new(&source(), &spec(method, threads)).unwrap();
            let estimate = plan.memory_estimate(-1.0) as usize;
            let before = LIVE.load(SeqCst);
            PEAK.store(before, SeqCst);
            let out = plan.warp_cells(data, -1.0).unwrap();
            let held = PEAK.load(SeqCst) - before;
            drop(out);
            assert!(
                held <= estimate,
                "{method:?} {threads}: {held} > {estimate}"
            );
            assert!(held as u64 > OUT_BYTES, "{method:?} {threads}: {held}");
        }
    }
}

fn the_budget_lowers_workers_and_then_refuses(data: &[f32]) {
    let one = WarpPlan::new(&source(), &spec(Method::Bilinear, 1)).unwrap();
    let alone = one.memory_estimate(-1.0);
    let want = bits(&one.warp_cells(data, -1.0).unwrap());

    // Enough for one worker and no more: asking for many gets one, with the same bits.
    let mut s = spec(Method::Bilinear, usize::MAX);
    s.limits.max_memory_bytes = alone;
    let plan = WarpPlan::new(&source(), &s).unwrap();
    assert_eq!(plan.memory_estimate(-1.0), alone);
    assert_eq!(bits(&plan.warp_cells(data, -1.0).unwrap()), want);

    // Less than one worker needs: refused, and nothing large is asked of the allocator.
    s.limits.max_memory_bytes = alone - 1;
    let plan = WarpPlan::new(&source(), &s).unwrap();
    LARGE_SEEN.store(0, SeqCst);
    let result = plan.warp_cells(data, -1.0);
    assert_eq!(LARGE_SEEN.load(SeqCst), 0);
    assert!(matches!(result, Err(VortexError::Refused { .. })));

    // Less than the destination: refused by the plan.
    s.limits.max_memory_bytes = OUT_BYTES - 1;
    assert!(matches!(
        WarpPlan::new(&source(), &s),
        Err(VortexError::Refused { .. })
    ));
}

// usize::MAX is the extreme request. Without the cap this starts a thread per row
// and panics when the OS refuses one.
fn a_huge_thread_count_is_capped(data: &[f32]) {
    let want = bits(&warp(&spec(Method::Near, 1), data).unwrap());
    let per_thread = 3u64 << 20;
    for threads in [0, usize::MAX] {
        let plan = WarpPlan::new(&source(), &spec(Method::Near, threads)).unwrap();
        let cap = (MAX_WARP_THREADS as u64) * per_thread + 2 * OUT_BYTES;
        let estimate = plan.memory_estimate(-1.0);
        assert!(estimate < cap, "{threads}: {estimate}");
        assert_eq!(bits(&plan.warp_cells(data, -1.0).unwrap()), want);
    }
}

fn the_default_clears_the_products_and_larger_grids_opt_in(data: &[f32]) {
    // The largest destination among the real-product warp cases compared with Vortex,
    // `noenv.mrms.nocell`.
    const LARGEST_REAL_PRODUCT_CELLS: u64 = 39_902_027;
    const _: () = assert!(LARGEST_REAL_PRODUCT_CELLS < DEFAULT_MAX_CELLS);
    assert_eq!(DEFAULT_MAX_CELLS, 268_435_456);
    assert_eq!(WarpLimits::default().max_cells, DEFAULT_MAX_CELLS);
    assert!(warp(&spec(Method::Near, 1), data).is_ok());

    // 40,000 x 40,000 is over 2^28. Only the plan is built; nothing that size is allocated.
    let small = SourceGrid {
        nx: 1,
        ny: 1,
        transform: GeoTransform::axis_aligned(0.0, 40000.0, 40000.0, -40000.0),
        crs: CrsDef::shg(),
    };
    let mut s = WarpSpec::new(Method::Near);
    s.cell_size = Some(1.0);
    s.envelope = Some(WarpEnvelope {
        envelope: Envelope {
            min_x: 0.0,
            max_x: 40000.0,
            min_y: 0.0,
            max_y: 40000.0,
        },
        crs: Some(CrsDef::shg()),
    });
    match WarpPlan::new(&small, &s) {
        Err(VortexError::Refused { reason, .. }) => {
            assert!(reason.contains("format_maximum"), "{reason}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(WarpLimits::format_maximum().max_cells, FORMAT_MAX_CELLS);
    s.limits = WarpLimits::format_maximum();
    let plan = WarpPlan::new(&small, &s).unwrap();
    assert_eq!(plan.nx() * plan.ny(), 1_600_000_000);
    assert!(plan.memory_estimate(-1.0) >= 6_400_000_000);

    // Past the format's maximum, whatever the caller sets.
    s.cell_size = Some(0.5);
    s.limits.max_cells = u64::MAX;
    s.limits.max_memory_bytes = u64::MAX;
    assert!(matches!(
        WarpPlan::new(&small, &s),
        Err(VortexError::Refused { .. })
    ));
}

#[test]
fn resources() {
    let data = cells();
    every_large_allocation_can_fail(&data);
    the_estimate_covers_what_was_really_held(&data);
    the_plan_and_the_warp_fit_a_budget_of_exactly_the_estimate(&data);
    the_budget_lowers_workers_and_then_refuses(&data);
    a_huge_thread_count_is_capped(&data);
    the_default_clears_the_products_and_larger_grids_opt_in(&data);
}

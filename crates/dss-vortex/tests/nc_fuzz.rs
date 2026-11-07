//! Malformed files return a typed error. Deterministic mutations of every fixture go through the
//! real entry points, under an allocator that refuses large requests as a process memory limit does:
//! `netcdf-reader`'s infallible allocations abort on that, so an abort here fails the whole test.
//! One `#[test]` only, so the limit stays in place while it runs.

use dss_vortex::source::netcdf::{Limits, NetcdfFile};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Limited;

/// Largest request allowed.
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

unsafe impl GlobalAlloc for Limited {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > LIMIT.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > LIMIT.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > LIMIT.load(Ordering::SeqCst) {
            return std::ptr::null_mut();
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Limited = Limited;

/// Everything the source does with a file. Errors are expected; only completing matters.
fn exercise(name: &str, bytes: Vec<u8>, limits: Limits) -> usize {
    let mut steps = 0;
    let Ok(file) = NetcdfFile::from_bytes(name, bytes, limits) else {
        return 0;
    };
    for variable in file.variable_names().iter().take(8) {
        if let Ok(v) = file.variable(variable) {
            let count = v.step_count();
            for step in [0, count / 2, count.saturating_sub(1)] {
                if v.step(step).is_ok() {
                    steps += 1;
                }
                let _ = v.frame(step);
                let _ = file.stored_step(variable, step);
            }
        }
    }
    steps
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*.
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

const EXTREMES: [[u8; 8]; 6] = [
    [0xff; 8],
    [0; 8],
    [0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    [0x80, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0x7f, 0xff, 0xff, 0xff],
    [0, 0, 0x40, 0, 0, 0, 0x40, 0],
];

fn mutate(original: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut bytes = original.to_vec();
    // Most of what a parser trusts sits at the front of a small file.
    let span = bytes.len().min(4096);
    match rng.below(5) {
        0 => {
            let at = rng.below(span);
            bytes[at] = (rng.next() & 0xff) as u8;
        }
        1 => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        2 => {
            bytes.truncate(rng.below(bytes.len()));
        }
        3 => {
            let value = EXTREMES[rng.below(EXTREMES.len())];
            let at = rng.below(span.saturating_sub(8).max(1));
            let end = (at + 8).min(bytes.len());
            bytes[at..end].copy_from_slice(&value[..end - at]);
        }
        _ => {
            for _ in 0..8 {
                let at = rng.below(bytes.len());
                bytes[at] = (rng.next() & 0xff) as u8;
            }
        }
    }
    bytes
}

/// A classic file declaring `len` x `len` x `len` values with a few bytes of data.
fn classic_declaring(len: u32) -> Vec<u8> {
    fn name(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(&(text.len() as u32).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
        out.resize(out.len() + (4 - text.len() % 4) % 4, 0);
    }
    let mut d = Vec::new();
    d.extend_from_slice(b"CDF\x01");
    d.extend_from_slice(&0u32.to_be_bytes());
    d.extend_from_slice(&10u32.to_be_bytes());
    d.extend_from_slice(&3u32.to_be_bytes());
    for dim in ["time", "lat", "lon"] {
        name(&mut d, dim);
        d.extend_from_slice(&len.to_be_bytes());
    }
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&11u32.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    name(&mut d, "units");
    d.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 1]);
    d.extend_from_slice(b"x\0\0\0");
    d.extend_from_slice(&[0; 0]);
    d.extend_from_slice(&11u32.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    name(&mut d, "data");
    d.extend_from_slice(&3u32.to_be_bytes());
    for id in 0u32..3 {
        d.extend_from_slice(&id.to_be_bytes());
    }
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&5u32.to_be_bytes());
    d.extend_from_slice(&len.wrapping_mul(len).wrapping_mul(4).to_be_bytes());
    d.extend_from_slice(&200u32.to_be_bytes());
    d.resize(d.len() + 64, 0);
    d
}

#[test]
fn no_mutation_panics_or_aborts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/netcdf");
    let limits = Limits::default();
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_none_or(|e| e != "json"))
        .collect();
    names.sort();
    // Every allocation in this test is under this. Larger requests fail, as under a process limit.
    LIMIT.store(256 << 20, Ordering::SeqCst);
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let (mut tried, mut read_something) = (0, 0);
    for path in &names {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let original = fs::read(path).unwrap();
        if original.len() > 400_000 {
            continue;
        }
        for _ in 0..120 {
            let bytes = mutate(&original, &mut rng);
            tried += 1;
            if exercise(&name, bytes, limits) > 0 {
                read_something += 1;
            }
        }
    }
    // Random bytes behind each magic.
    for magic in [
        &b"CDF\x01"[..],
        b"CDF\x02",
        b"CDF\x05",
        b"\x89HDF\r\n\x1a\n",
    ] {
        for _ in 0..500 {
            let mut bytes = magic.to_vec();
            for _ in 0..rng.below(2048) {
                bytes.push((rng.next() & 0xff) as u8);
            }
            exercise("random.nc", bytes, limits);
            tried += 1;
        }
    }
    // Declared sizes far past the payload, including where the product wraps 32 bits.
    for len in [1, 1 << 8, 1 << 14, 1 << 16, 1 << 24, u32::MAX] {
        exercise("declared.nc", classic_declaring(len), limits);
    }
    println!("{tried} mutations, {read_something} still read a step");
    assert!(tried > 3000, "{tried}");
    assert!(
        read_something > 100,
        "{read_something}: too few reach decoding"
    );
}

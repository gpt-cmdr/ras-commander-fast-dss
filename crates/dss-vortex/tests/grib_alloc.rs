//! Declared counts must be checked against the payload before anything sized by them is allocated.
//! An allocator that records requests observes the order; an eventual error would not show it.
//! One `#[test]` only, so no other test thread adds to the counts.

use dss_vortex::source::grib::{Grib2File, Limits};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Recording;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

const POINTS: u32 = 1 << 20;

fn section(bytes: &[u8], number: u8) -> usize {
    let mut at = 16;
    loop {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if bytes[at + 4] == number {
            return at;
        }
        at += length;
    }
}

fn set(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// A real fixture declaring a 1024 x 1024 grid and `POINTS` groups, with its short payload kept.
fn declaring_huge_counts(name: &str) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib");
    let mut bytes = fs::read(dir.join(name)).unwrap();
    let grid = section(&bytes, 3);
    set(&mut bytes, grid + 6, POINTS);
    set(&mut bytes, grid + 30, 1024);
    set(&mut bytes, grid + 34, 1024);
    let representation = section(&bytes, 5);
    set(&mut bytes, representation + 5, POINTS);
    set(&mut bytes, representation + 31, POINTS);
    bytes
}

/// Fixture `name` on a 1024 x 1024 grid with every bitmap bit set and an empty section 7.
fn bitmap_with_nothing_to_place(name: &str, declared: u32) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib");
    let original = fs::read(dir.join(name)).unwrap();
    let mut bytes = original.clone();
    let grid = section(&bytes, 3);
    set(&mut bytes, grid + 6, POINTS);
    set(&mut bytes, grid + 30, 1024);
    set(&mut bytes, grid + 34, 1024);
    let representation = section(&bytes, 5);
    set(&mut bytes, representation + 5, declared);
    bytes.truncate(section(&bytes, 6));
    let bitmap_bytes = (POINTS / 8) as usize;
    bytes.extend_from_slice(&(6 + bitmap_bytes as u32).to_be_bytes());
    bytes.extend_from_slice(&[6, 0]);
    bytes.resize(bytes.len() + bitmap_bytes, 0xFF);
    bytes.extend_from_slice(&5u32.to_be_bytes());
    bytes.push(7);
    bytes.extend_from_slice(b"7777");
    let total = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&total.to_be_bytes());
    bytes
}

fn largest_request_while(run: impl FnOnce()) -> usize {
    LARGEST.store(0, Ordering::SeqCst);
    run();
    LARGEST.load(Ordering::SeqCst)
}

#[test]
fn nothing_sized_by_a_declared_count_is_allocated_before_validation() {
    for name in ["complex.grib2", "spatial1.grib2", "spatial2.grib2"] {
        let bytes = declaring_huge_counts(name);
        let file = Grib2File::from_bytes("x.grib2", bytes, Limits::default()).unwrap();
        let mut result = None;
        let largest = largest_request_while(|| result = Some(file.decode_record(0)));
        assert!(result.unwrap().is_err(), "{name}");
        assert!(largest < 64 * 1024, "{name}: requested {largest} bytes");
    }

    // A bitmap's set bits are reconciled with section 5 and section 7 before the grid. Declaring
    // no values makes section 5's size check pass on an empty payload.
    for name in [
        "simple_bitmap.grib2",
        "complex_bitmap.grib2",
        "spatial2_bitmap.grib2",
    ] {
        for declared in [0, 1, POINTS - 1, POINTS] {
            let bytes = bitmap_with_nothing_to_place(name, declared);
            let bitmap_size = bytes.len();
            let file = Grib2File::from_bytes("x.grib2", bytes, Limits::default()).unwrap();
            let mut result = None;
            let largest = largest_request_while(|| result = Some(file.decode_record(0)));
            assert!(result.unwrap().is_err(), "{name} declaring {declared}");
            assert!(
                largest < 64 * 1024 + bitmap_size,
                "{name} declaring {declared}: requested {largest} bytes"
            );
        }
    }

    // Set bits and section 5 agree, but the groups supply none of the values.
    let mut bytes = bitmap_with_nothing_to_place("complex_bitmap.grib2", POINTS);
    let representation = section(&bytes, 5);
    set(&mut bytes, representation + 31, 1); // one group
    bytes[representation + 19] = 0; // no reference bits
    bytes[representation + 36] = 0; // no width bits
    bytes[representation + 46] = 0; // no length bits
    set(&mut bytes, representation + 37, 0); // reference length
    set(&mut bytes, representation + 42, 0); // last length
    let file = Grib2File::from_bytes("x.grib2", bytes, Limits::default()).unwrap();
    let mut result = None;
    let largest = largest_request_while(|| result = Some(file.decode_record(0)));
    let error = result.unwrap().unwrap_err().to_string();
    assert!(error.contains("group lengths sum to 0"), "{error}");
    assert!(largest < 64 * 1024, "requested {largest} bytes");

    // Group tables with no bits at all hold no storage: a constant field costs only its grid.
    let mut bytes = declaring_huge_counts("complex.grib2");
    let representation = section(&bytes, 5);
    bytes[representation + 19] = 0; // bits per group reference
    bytes[representation + 36] = 0; // bits per group width
    bytes[representation + 35] = 0; // reference width
    bytes[representation + 46] = 0; // bits per group length
    bytes[representation + 41] = 0; // length increment
    set(&mut bytes, representation + 37, 1); // reference length
    set(&mut bytes, representation + 42, 1); // last length
    let file = Grib2File::from_bytes("x.grib2", bytes, Limits::default()).unwrap();
    let mut result = None;
    let largest = largest_request_while(|| result = Some(file.decode_record(0)));
    let cells = result.unwrap().unwrap();
    assert_eq!(cells.len(), POINTS as usize);
    assert!(largest <= POINTS as usize * 4, "requested {largest} bytes");

    // Over the input limit, refused before the bytes are scanned.
    let limits = Limits {
        max_input_bytes: 8,
        ..Limits::default()
    };
    assert!(Grib2File::from_bytes("x.grib2", vec![0; 9], limits).is_err());
}

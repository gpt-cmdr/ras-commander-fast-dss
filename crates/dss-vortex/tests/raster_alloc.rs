//! Declared sizes must be checked against the file before anything sized by them is allocated. An
//! allocator that records requests observes the order; an eventual error would not show it.
//! One `#[test]` only, so no other test thread adds to the counts.

use dss_vortex::source::raster::{RasterFile, ReadOptions};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Recording;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc_zeroed(layout) }
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

/// The largest request during `f`.
fn largest<T>(f: impl FnOnce() -> T) -> (usize, T) {
    LARGEST.store(0, Ordering::SeqCst);
    let out = f();
    (LARGEST.load(Ordering::SeqCst), out)
}

/// A little-endian classic TIFF from inline entries plus trailing blobs.
fn tiff(entries: &[(u16, u16, u32, u32)], tail: &[u8]) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| e.0);
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(sorted.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in &sorted {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(tail);
    out
}

fn header_size(entries: usize) -> u32 {
    10 + entries as u32 * 12 + 4
}

/// Georeference blobs for a valid transform, with their offsets from `at`.
type Entry = (u16, u16, u32, u32);

fn geo(at: u32) -> (Vec<Entry>, Vec<u8>) {
    let mut tail = Vec::new();
    for v in [1.0f64, 1.0, 0.0] {
        tail.extend_from_slice(&v.to_le_bytes());
    }
    for v in [0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0] {
        tail.extend_from_slice(&v.to_le_bytes());
    }
    (vec![(33550, 12, 3, at), (33922, 12, 6, at + 24)], tail)
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// `width` x `height` float32 in one tile or strip of `chunk` stored bytes, compressed as `code`.
fn declared(
    width: u32,
    height: u32,
    tile: Option<(u32, u32)>,
    code: u32,
    stored: &[u8],
) -> Vec<u8> {
    let mut base: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, width),
        (257, 4, 1, height),
        (258, 3, 1, 32),
        (259, 3, 1, code),
        (262, 3, 1, 1),
        (277, 3, 1, 1),
        (339, 3, 1, 3),
    ];
    let geo_count = 2;
    let extra = if tile.is_some() { 4 } else { 3 };
    let at = header_size(base.len() + geo_count + extra);
    let (entries, mut tail) = geo(at);
    base.extend(entries);
    let data_at = at + tail.len() as u32;
    match tile {
        Some((w, h)) => base.extend([
            (322, 4, 1, w),
            (323, 4, 1, h),
            (324, 4, 1, data_at),
            (325, 4, 1, stored.len() as u32),
        ]),
        None => base.extend([
            (278, 4, 1, height),
            (273, 4, 1, data_at),
            (279, 4, 1, stored.len() as u32),
        ]),
    }
    tail.extend_from_slice(stored);
    tiff(&base, &tail)
}

const MEBIBYTE: usize = 1 << 20;

fn read(bytes: &[u8], name: &str) -> Result<RasterFile, dss_vortex::VortexError> {
    RasterFile::from_bytes(name, bytes, None, ReadOptions::default())
}

#[test]
fn declared_sizes_do_not_reach_the_allocator() {
    let small = zlib(&[0u8; 64]);

    // Under the cell limit, and refused because a chunk this short cannot hold them.
    for (name, bytes) in [
        ("deflate strip", declared(8192, 8192, None, 8, &small)),
        ("LZW strip", declared(8192, 8192, None, 5, &small)),
        ("uncompressed strip", declared(8192, 8192, None, 1, &small)),
        (
            "deflate tile",
            declared(8192, 8192, Some((8192, 8192)), 8, &small),
        ),
        // A tile far larger than a one-cell image.
        ("huge tile", declared(1, 1, Some((16384, 16384)), 8, &small)),
        // Over the limit.
        ("over limit", declared(65536, 65536, None, 8, &small)),
        (
            "tile over limit",
            declared(1, 1, Some((1 << 20, 1 << 20)), 8, &small),
        ),
    ] {
        let (peak, result) = largest(|| read(&bytes, "a.tif"));
        assert!(result.is_err(), "{name}");
        assert!(peak < MEBIBYTE, "{name}: requested {peak} bytes");
    }

    // A declared offset table bigger than the file, which the tiff crate would read into a Vec.
    let mut entries = vec![
        (256u16, 4u16, 1u32, 8192u32),
        (257, 4, 1, 8192),
        (258, 3, 1, 32),
        (259, 3, 1, 1),
        (262, 3, 1, 1),
        (277, 3, 1, 1),
        (278, 4, 1, 1),
        (339, 3, 1, 3),
    ];
    let at = header_size(entries.len() + 2 + 2);
    let (georef, tail) = geo(at);
    entries.extend(georef);
    entries.push((273, 4, 1 << 30, at));
    entries.push((279, 4, 1 << 30, at));
    let (peak, result) = largest(|| read(&tiff(&entries, &tail), "a.tif"));
    assert!(result.is_err());
    assert!(peak < MEBIBYTE, "offset table: requested {peak} bytes");

    // ASCII: a header declaring more cells than the text could hold.
    let text = b"ncols 8192\nnrows 8192\nxllcorner 0\nyllcorner 0\ncellsize 1\n 1 2 3\n";
    let (peak, result) = largest(|| read(text, "a.asc"));
    assert!(result.is_err());
    assert!(peak < MEBIBYTE, "asc: requested {peak} bytes");

    // A valid large grid does allocate; the recorder sees it, so the bounds above can fail.
    let cells = 2048 * 2048;
    let plain = vec![0u8; cells * 4];
    let bytes = declared(2048, 2048, None, 1, &plain);
    let (peak, result) = largest(|| read(&bytes, "a.tif"));
    assert!(result.is_ok());
    assert!(peak >= cells * 4, "the recorder saw only {peak} bytes");

    // Why the crate's decoder is not used to read: from a few hundred bytes it asks for the grid.
    let probe = declared(4096, 4096, None, 8, &small);
    let (peak, _) = largest(|| {
        let mut decoder = tiff_crate::decoder::Decoder::new(Cursor::new(&probe))
            .unwrap()
            .with_limits(tiff_crate::decoder::Limits::unlimited());
        decoder.read_image()
    });
    assert!(
        peak >= 4096 * 4096 * 4,
        "the tiff crate requested {peak} bytes from {} bytes",
        probe.len()
    );
}

use tiff as tiff_crate;

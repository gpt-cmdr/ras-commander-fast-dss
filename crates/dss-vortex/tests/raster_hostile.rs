//! Malformed and oversized rasters give typed errors, not panics or aborts. Deterministic.

use dss_vortex::source::raster::{Limits, RasterFile, ReadOptions, UnknownCrs};
use dss_vortex::VortexError;
use std::fs;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/raster")
}

/// A small generator, so the corpus is the same on every run.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn options() -> ReadOptions {
    ReadOptions {
        // The mutations can state 2^28 cells; this keeps the run's real allocation small.
        limits: Limits {
            max_cells: 1 << 22,
            ..Limits::default()
        },
        unknown_crs: UnknownCrs::Undefined,
    }
}

const EXTREMES: [u8; 6] = [0, 1, 0x7F, 0x80, 0xFE, 0xFF];

fn mutate(rng: &mut Xorshift, original: &[u8]) -> Vec<u8> {
    let mut bytes = original.to_vec();
    match rng.below(5) {
        0 => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        1 => {
            let at = rng.below(bytes.len());
            bytes[at] = EXTREMES[rng.below(EXTREMES.len())];
        }
        2 => {
            let keep = rng.below(bytes.len());
            bytes.truncate(keep);
        }
        3 => {
            // A run of extremes, as a large count or offset would be.
            let at = rng.below(bytes.len());
            let value = EXTREMES[rng.below(EXTREMES.len())];
            for byte in bytes.iter_mut().skip(at).take(8) {
                *byte = value;
            }
        }
        _ => {
            // Bias toward the header and first directory, where sizes live.
            let at = rng.below(bytes.len().min(512));
            bytes[at] = EXTREMES[rng.below(EXTREMES.len())];
        }
    }
    bytes
}

#[test]
fn mutated_fixtures_never_panic() {
    let mut rng = Xorshift(0x9E3779B97F4A7C15);
    let mut names: Vec<_> = fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tif") || n.ends_with(".tiff") || n.ends_with(".asc"))
        .collect();
    names.sort();
    assert!(names.len() > 50);
    let (mut read, mut failed) = (0, 0);
    for name in &names {
        let original = fs::read(dir().join(name)).unwrap();
        if original.len() > 100_000 {
            continue;
        }
        for _ in 0..150 {
            let bytes = mutate(&mut rng, &original);
            match RasterFile::from_bytes(name.as_str(), &bytes, None, options()) {
                Ok(file) => {
                    let parts = file.frame().parts();
                    assert_eq!(parts.cells.len(), parts.nx * parts.ny);
                    read += 1;
                }
                Err(_) => failed += 1,
            }
        }
    }
    assert!(read > 0 && failed > 0, "{read} read, {failed} failed");
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Xorshift(0x2545F4914F6CDD1D);
    for len in [0usize, 1, 2, 3, 4, 7, 8, 16, 64, 512, 4096] {
        for magic in [&b"II*\0"[..], b"MM\0*", b"II+\0", b"MM\0+", b"ncols"] {
            for _ in 0..50 {
                let mut bytes = magic.to_vec();
                bytes.extend((0..len).map(|_| rng.next() as u8));
                let name = if magic == b"ncols" { "a.asc" } else { "a.tif" };
                let _ = RasterFile::from_bytes(name, &bytes, None, options());
            }
        }
    }
}

#[test]
fn empty_inputs_are_typed() {
    assert!(RasterFile::from_bytes("a.tif", b"", None, options()).is_err());
    assert!(RasterFile::from_bytes("a.asc", b"", None, options()).is_err());
    assert!(RasterFile::from_bytes("a.asc", b"ncols 0\nnrows 0\n", None, options()).is_err());
}

type Entry = (u16, u16, u32, u32, Option<Vec<u8>>);

/// A little-endian classic TIFF from tag values, each inline or out of line.
struct Build {
    entries: Vec<Entry>,
}

impl Build {
    fn new() -> Self {
        Build {
            entries: Vec::new(),
        }
    }

    fn short(mut self, tag: u16, value: u16) -> Self {
        self.entries.push((tag, 3, 1, u32::from(value), None));
        self
    }

    fn long(mut self, tag: u16, value: u32) -> Self {
        self.entries.push((tag, 4, 1, value, None));
        self
    }

    fn doubles(mut self, tag: u16, values: &[f64]) -> Self {
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.entries
            .push((tag, 12, values.len() as u32, 0, Some(bytes)));
        self
    }

    fn finish(mut self) -> Vec<u8> {
        self.entries.sort_by_key(|e| e.0);
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        let table = out.len();
        out.resize(table + self.entries.len() * 12 + 4, 0);
        for (i, (tag, kind, count, value, blob)) in self.entries.iter().enumerate() {
            let value = match blob {
                Some(bytes) => {
                    let at = out.len() as u32;
                    out.extend_from_slice(bytes);
                    at
                }
                None => *value,
            };
            let at = table + i * 12;
            out[at..at + 2].copy_from_slice(&tag.to_le_bytes());
            out[at + 2..at + 4].copy_from_slice(&kind.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
            out[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

const TIE: [f64; 6] = [0.0, 0.0, 0.0, 10.0, 20.0, 0.0];

/// Everything a minimal valid float32 file needs, for `width` by `height` in one strip.
fn minimal(width: u32, height: u32) -> Build {
    Build::new()
        .long(256, width)
        .long(257, height)
        .short(258, 32)
        .short(259, 1)
        .short(262, 1)
        .short(277, 1)
        .long(278, height)
        .long(273, 0)
        .long(279, 0)
        .short(339, 3)
        .doubles(33550, &[1.0, 1.0, 0.0])
        .doubles(33922, &TIE)
}

/// Put an entry's value in place.
fn set_entry(bytes: &mut [u8], tag: u16, value: u32) {
    let count = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    for i in 0..count {
        let at = 10 + i * 12;
        if u16::from_le_bytes([bytes[at], bytes[at + 1]]) == tag {
            bytes[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("no tag {tag}");
}

/// The built directory, with `data` as its one strip.
fn with_data(build: Build, data: Vec<u8>) -> Vec<u8> {
    let mut bytes = build.finish();
    let at = bytes.len() as u32;
    let len = data.len() as u32;
    bytes.extend_from_slice(&data);
    set_entry(&mut bytes, 273, at);
    set_entry(&mut bytes, 279, len);
    bytes
}

fn file(width: u32, height: u32, extra: impl FnOnce(Build) -> Build, data: Vec<u8>) -> Vec<u8> {
    with_data(extra(minimal(width, height)), data)
}

fn floats(n: usize) -> Vec<u8> {
    (0..n).flat_map(|i| (i as f32).to_le_bytes()).collect()
}

#[test]
fn the_builder_makes_a_file_that_reads() {
    let bytes = file(3, 2, |b| b, floats(6));
    let read = RasterFile::from_bytes("a.tif", &bytes, None, ReadOptions::default()).unwrap();
    assert_eq!(read.frame().cells(), [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    assert_eq!(read.frame().parts().transform.origin_x, 10.0);
    assert_eq!(read.frame().parts().transform.origin_y, 20.0);
}

fn expect_error(bytes: &[u8], needle: &str) {
    let error = RasterFile::from_bytes("a.tif", bytes, None, ReadOptions::default())
        .err()
        .unwrap_or_else(|| panic!("read, expected {needle}"));
    let text = error.to_string();
    assert!(text.contains(needle), "{text}, expected {needle}");
    assert!(!matches!(error, VortexError::Io(_)), "{text}");
}

#[test]
fn short_and_over_long_data_are_refused() {
    expect_error(&file(3, 2, |b| b, floats(5)), "cannot hold");
    expect_error(&file(3, 2, |b| b, floats(7)), "cannot hold");
    expect_error(&file(3, 2, |b| b, Vec::new()), "sparse");
}

#[test]
fn a_chunk_past_the_end_of_the_file_is_refused() {
    let mut bytes = file(3, 2, |b| b, floats(6));
    let len = bytes.len() as u32;
    set_entry(&mut bytes, 273, len);
    expect_error(&bytes, "past the file's end");
}

#[test]
fn a_repeated_or_conflicting_tag_is_refused() {
    let bytes = file(3, 2, |b| b.long(322, 16), floats(6));
    expect_error(&bytes, "strip and tile");
    let bytes = file(3, 2, |b| b.short(258, 32), floats(6));
    expect_error(&bytes, "repeated");
}

#[test]
fn layouts_this_reader_does_not_take_are_refused_by_name() {
    for (tag, value, needle) in [
        (274u16, 3u16, "orientation"),
        (266, 2, "fill order"),
        (317, 9, "predictor"),
        (338, 1, "ExtraSamples"),
        (330, 8, "SubIFDs"),
    ] {
        expect_error(&file(3, 2, |b| b.short(tag, value), floats(6)), needle);
    }
    for photometric in [0, 3] {
        let mut bytes = file(3, 2, |b| b, floats(6));
        set_entry(&mut bytes, 262, photometric);
        expect_error(&bytes, "photometric");
    }
    for (tag, value, needle) in [
        (277, 2, "bands"),
        (258, 64, "sample type"),
        (259, 7, "compression"),
    ] {
        let mut bytes = file(3, 2, |b| b, floats(6));
        set_entry(&mut bytes, tag, value);
        expect_error(&bytes, needle);
    }
}

fn with_scale(scale: &[f64]) -> Vec<u8> {
    let build = Build::new()
        .long(256, 3)
        .long(257, 2)
        .short(258, 32)
        .short(259, 1)
        .short(262, 1)
        .long(273, 0)
        .long(279, 0)
        .short(339, 3)
        .doubles(33550, scale)
        .doubles(33922, &TIE);
    with_data(build, floats(6))
}

#[test]
fn georeference_this_reader_does_not_take_is_refused() {
    expect_error(&with_scale(&[1.0, -1.0, 0.0]), "not positive");
    expect_error(&with_scale(&[0.0, 1.0, 0.0]), "not positive");
    expect_error(&with_scale(&[1.0, 1.0]), "values");
    let identity = [
        1.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    expect_error(
        &file(3, 2, |b| b.doubles(34264, &identity), floats(6)),
        "conflicting",
    );
    let matrix = |m: &[f64]| {
        let build = Build::new()
            .long(256, 3)
            .long(257, 2)
            .short(258, 32)
            .short(259, 1)
            .short(262, 1)
            .long(273, 0)
            .long(279, 0)
            .short(339, 3)
            .doubles(34264, m);
        with_data(build, floats(6))
    };
    assert!(
        RasterFile::from_bytes("a.tif", &matrix(&identity), None, ReadOptions::default()).is_ok()
    );
    let mut perspective = identity;
    perspective[12] = 1.0;
    expect_error(&matrix(&perspective), "2D affine");
    let mut with_z = identity;
    with_z[2] = 1.0;
    expect_error(&matrix(&with_z), "2D affine");
    expect_error(&matrix(&identity[..15]), "values");
}

#[test]
fn non_finite_transforms_are_typed() {
    let bytes = with_scale(&[f64::INFINITY, 1.0, 0.0]);
    assert!(RasterFile::from_bytes("a.tif", &bytes, None, ReadOptions::default()).is_err());
    let bytes = with_scale(&[f64::NAN, 1.0, 0.0]);
    expect_error(&bytes, "not positive");
}

#[test]
fn a_directory_cycle_is_refused() {
    let mut bytes = file(3, 2, |b| b, floats(6));
    let count = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let next = 10 + count * 12;
    // The next directory is the first one again. Marked as an overview, it would loop for ever.
    bytes[next..next + 4].copy_from_slice(&8u32.to_le_bytes());
    let error = RasterFile::from_bytes("a.tif", &bytes, None, ReadOptions::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("full-size"), "{error}");
    let looped = {
        let mut b = file(3, 2, |b| b.long(254, 0), floats(6));
        let count = u16::from_le_bytes([b[8], b[9]]) as usize;
        let next = 10 + count * 12;
        // A second copy of the directory, marked as an overview, pointing at itself.
        let at = b.len();
        let dir: Vec<u8> = b[8..next + 4].to_vec();
        b.extend_from_slice(&dir);
        b[next..next + 4].copy_from_slice(&(at as u32).to_le_bytes());
        for i in 0..count {
            let entry = at + 2 + i * 12;
            if u16::from_le_bytes([b[entry], b[entry + 1]]) == 254 {
                b[entry + 8..entry + 12].copy_from_slice(&1u32.to_le_bytes());
            }
        }
        let tail = at + 2 + count * 12;
        b[tail..tail + 4].copy_from_slice(&(at as u32).to_le_bytes());
        b
    };
    expect_error(&looped, "or a cycle");
}

#[test]
fn over_limit_grids_are_refused_before_anything_scales() {
    expect_error(&file(1 << 20, 1 << 20, |b| b, floats(6)), "over the limit");
    let text = b"ncols 65536\nnrows 65536\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2";
    let error = RasterFile::from_bytes("a.asc", text, None, ReadOptions::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("over the limit"), "{error}");
}

#[test]
fn asc_headers_are_strict() {
    let cases: &[(&str, &str)] = &[
        (
            "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 0\n1\n",
            "not positive",
        ),
        (
            "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize -1\n1\n",
            "not positive",
        ),
        (
            "ncols 1\nnrows 1\nxllcorner x\nyllcorner 0\ncellsize 1\n1\n",
            "not a number",
        ),
        (
            "ncols 1\nnrows 1\nxllcorner inf\nyllcorner 0\ncellsize 1\n1\n",
            "not finite",
        ),
        (
            "ncols -1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1\n",
            "positive count",
        ),
        (
            "ncols 1\nncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1\n",
            "repeated",
        ),
        (
            "nrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1\n",
            "no ncols",
        ),
        (
            "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\n1\n",
            "cellsize",
        ),
        (
            "ncols 1\nnrows 1\nmystery 7\nxllcorner 0\nyllcorner 0\ncellsize 1\n1\n",
            "unknown key",
        ),
        (
            "ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n",
            "no data",
        ),
        (
            "ncols 1\nnrows 1\nxllcorner 0\nyllcenter 0\ncellsize 1\n1\n",
            "georeference",
        ),
        (
            "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1\n",
            "cannot hold",
        ),
    ];
    for (text, needle) in cases {
        let error = RasterFile::from_bytes("a.asc", text.as_bytes(), None, ReadOptions::default())
            .err()
            .unwrap_or_else(|| panic!("read {text:?}"))
            .to_string();
        assert!(error.contains(needle), "{text:?}: {error}");
    }
    let invalid = b"ncols 1\nnrows 1\xff\n";
    assert!(RasterFile::from_bytes("a.asc", invalid, None, ReadOptions::default()).is_err());
}

#[test]
fn the_prj_route_refuses_text_that_is_not_wkt() {
    let grid = b"ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1.5\n";
    for prj in [
        &b"not wkt"[..],
        b"",
        b"PROJCS[\"x\"",
        b"PROJCS[\"x\"]]",
        b"\xff\xfe",
        b"GEOGCS[\"x\",DATUM[\"y\",SPHEROID[\"z\",1,0]]]",
    ] {
        let error = RasterFile::from_bytes("a.asc", grid, Some(prj), ReadOptions::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains(".prj"), "{prj:?}: {error}");
    }
    let good = b"GEOGCS[\"x\",DATUM[\"y\",SPHEROID[\"z\",1,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";
    assert!(RasterFile::from_bytes("a.asc", grid, Some(good), ReadOptions::default()).is_ok());
    // Vortex's `orb100yr24ha.prj` has a bare authority code.
    let bare = b"GEOGCS[\"x\",DATUM[\"y\",SPHEROID[\"z\",1,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433],AUTHORITY[\"EPSG\",4322]]";
    assert!(RasterFile::from_bytes("a.asc", grid, Some(bare), ReadOptions::default()).is_ok());
}

//! Shared by the Zarr tests: fixture paths, a SHA-256, and a builder of small stores.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/zarr")
}

/// A new empty folder under the target directory, never beside the fixtures.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Every file under a folder, as `/`-separated paths relative to it, sorted.
pub fn files_under(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), out);
            } else {
                let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

pub fn cells_bytes(cells: &[f32]) -> Vec<u8> {
    cells.iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// One array of a store built for a test: its `.zarray`, `.zattrs` and uncompressed chunks.
pub struct TestArray {
    pub name: &'static str,
    pub dtype: &'static str,
    pub shape: Vec<usize>,
    pub chunks: Vec<usize>,
    /// The `fill_value` member, as JSON.
    pub fill: &'static str,
    /// Members of `.zattrs` besides `_ARRAY_DIMENSIONS`, as JSON without braces.
    pub attrs: String,
    pub dims: Vec<&'static str>,
    /// Stored bytes, C order, in the dtype's byte order.
    pub data: Vec<u8>,
}

pub fn le_f64(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn le_i16(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn list(values: &[usize]) -> String {
    let parts: Vec<String> = values.iter().map(usize::to_string).collect();
    format!("[{}]", parts.join(","))
}

pub fn zarray_text(a: &TestArray) -> String {
    format!(
        r#"{{"chunks":{},"compressor":null,"dtype":"{}","fill_value":{},"filters":null,"order":"C","shape":{},"zarr_format":2}}"#,
        list(&a.chunks),
        a.dtype,
        a.fill,
        list(&a.shape)
    )
}

pub fn zattrs_text(a: &TestArray) -> String {
    let dims: Vec<String> = a.dims.iter().map(|d| format!("\"{d}\"")).collect();
    let comma = if a.attrs.is_empty() { "" } else { "," };
    format!(
        r#"{{"_ARRAY_DIMENSIONS":[{}]{comma}{}}}"#,
        dims.join(","),
        a.attrs
    )
}

/// Write a store with per-array metadata and every chunk, uncompressed. Edge chunks are padded
/// with zeros to the chunk shape, as Zarr v2 stores them.
pub fn write_store(root: &Path, arrays: &[TestArray]) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zgroup"), r#"{"zarr_format":2}"#).unwrap();
    for a in arrays {
        let dir = root.join(a.name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".zarray"), zarray_text(a)).unwrap();
        fs::write(dir.join(".zattrs"), zattrs_text(a)).unwrap();
        let item = a.dtype[2..].parse::<usize>().unwrap();
        let rank = a.shape.len();
        let counts: Vec<usize> = (0..rank)
            .map(|d| a.shape[d].div_ceil(a.chunks[d]))
            .collect();
        let total: usize = counts.iter().product();
        let cells: usize = a.chunks.iter().product();
        for n in 0..total {
            // This chunk's index, the last dimension fastest.
            let mut index = vec![0usize; rank];
            let mut rest = n;
            for d in (0..rank).rev() {
                index[d] = rest % counts[d];
                rest /= counts[d];
            }
            let mut chunk = vec![0u8; cells * item];
            for c in 0..cells {
                let mut rest = c;
                let mut source = 0usize;
                let mut stride = 1usize;
                let mut inside = true;
                for d in (0..rank).rev() {
                    let at = index[d] * a.chunks[d] + rest % a.chunks[d];
                    rest /= a.chunks[d];
                    inside &= at < a.shape[d];
                    source += at.min(a.shape[d] - 1) * stride;
                    stride *= a.shape[d];
                }
                if inside {
                    chunk[c * item..(c + 1) * item]
                        .copy_from_slice(&a.data[source * item..(source + 1) * item]);
                }
            }
            let key: Vec<String> = index.iter().map(usize::to_string).collect();
            fs::write(dir.join(key.join(".")), chunk).unwrap();
        }
    }
}

/// A small latitude and longitude store: `time` (hours since 2000-01-01, `<f8`), `latitude`
/// (`ny` from 30 by 0.5), `longitude` (`nx` from -100 by 0.5), and one `<i2` variable in chunks
/// of 2 x 2 x 3.
pub fn grid_arrays(
    variable: &'static str,
    hours: &[f64],
    ny: usize,
    nx: usize,
    attrs: &str,
    data: &[i16],
) -> Vec<TestArray> {
    let lat: Vec<f64> = (0..ny).map(|i| 30.0 + 0.5 * i as f64).collect();
    let lon: Vec<f64> = (0..nx).map(|i| -100.0 + 0.5 * i as f64).collect();
    assert_eq!(data.len(), hours.len() * ny * nx);
    vec![
        TestArray {
            name: "time",
            dtype: "<f8",
            shape: vec![hours.len()],
            chunks: vec![hours.len().max(1)],
            fill: "null",
            attrs: r#""units":"hours since 2000-01-01 00:00:00""#.into(),
            dims: vec!["time"],
            data: le_f64(hours),
        },
        TestArray {
            name: "latitude",
            dtype: "<f8",
            shape: vec![ny],
            chunks: vec![ny],
            fill: "null",
            attrs: r#""units":"degrees_north""#.into(),
            dims: vec!["latitude"],
            data: le_f64(&lat),
        },
        TestArray {
            name: "longitude",
            dtype: "<f8",
            shape: vec![nx],
            chunks: vec![nx],
            fill: "null",
            attrs: r#""units":"degrees_east""#.into(),
            dims: vec!["longitude"],
            data: le_f64(&lon),
        },
        TestArray {
            name: variable,
            dtype: "<i2",
            shape: vec![hours.len(), ny, nx],
            chunks: vec![2, 2, 3],
            fill: "null",
            attrs: attrs.to_string(),
            dims: vec!["time", "latitude", "longitude"],
            data: le_i16(data),
        },
    ]
}

/// Cells `0, 1, 2, ...` for a grid of `steps * ny * nx`.
pub fn ramp(count: usize) -> Vec<i16> {
    (0..count).map(|i| i as i16).collect()
}

// SHA-256 (FIPS 180-4): the one copy in `tests/support/sha256.rs`.
#[path = "../support/sha256.rs"]
mod sha256_support;
// Not every Zarr test hashes, as `dead_code` above allows for the rest of this module.
#[allow(unused_imports)]
pub use sha256_support::sha256;

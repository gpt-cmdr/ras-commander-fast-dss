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

// SHA-256 (FIPS 180-4), as `crates/dss-core/tests/grid_real_public.rs` has it.
pub fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in m.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for t in 0..16 {
            w[t] = u32::from_be_bytes(chunk[4 * t..4 * t + 4].try_into().unwrap());
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for t in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[t])
                .wrapping_add(w[t]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

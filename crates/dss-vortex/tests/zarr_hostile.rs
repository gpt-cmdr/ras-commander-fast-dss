//! Malformed stores: each is a correct result or a typed error, never a panic or an abort.
//!
//! Every probe runs twice. In this process, where a panic would fail the test; and in a process of
//! its own through the built `zarr_dump`, under `oracle/vortex/zarr_job.py`'s Windows Job Object
//! at 128 and at 64 MiB of commit, where a failed infallible allocation or a stack overflow would
//! end the process and only the exit status could show it. Without Windows and Python the
//! subprocess runs with no limit, as `nc_hostile_job.rs` does.

mod zarr_support;

use dss_vortex::source::zarr::ZarrStore;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use zarr_support::*;

const VARIABLE: &str = "TMP_2maboveground";

/// A sound store of 4 steps on 4 x 6 cells, in chunks of 2 x 2 x 3 `<i2`: 24 bytes each.
fn base(root: &Path) {
    let attrs = r#""units":"K","crs":"EPSG:4326","_FillValue":-1"#;
    write_store(
        root,
        &grid_arrays(VARIABLE, &[0.0, 1.0, 2.0, 3.0], 4, 6, attrs, &ramp(96)),
    );
}

/// One step of 1024 x 1024 `<i2` in one chunk of 2 MiB, so a stream that decodes to more has room
/// to be large.
fn wide(root: &Path, compressor: &str, chunk: &[u8]) {
    let n = 1024;
    let lat: Vec<f64> = (0..n).map(|i| 10.0 + 0.01 * i as f64).collect();
    let lon: Vec<f64> = (0..n).map(|i| -100.0 + 0.01 * i as f64).collect();
    let axis = |name: &'static str, units: &str, values: &[f64]| TestArray {
        name,
        dtype: "<f8",
        shape: vec![values.len()],
        chunks: vec![values.len()],
        fill: "null",
        attrs: format!(r#""units":"{units}""#),
        dims: vec![name],
        data: le_f64(values),
    };
    write_store(
        root,
        &[
            axis("latitude", "degrees_north", &lat),
            axis("longitude", "degrees_east", &lon),
        ],
    );
    let dir = root.join("wide");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(".zarray"),
        format!(
            r#"{{"chunks":[1024,1024],"compressor":{compressor},"dtype":"<i2","fill_value":null,"filters":null,"order":"C","shape":[1024,1024],"zarr_format":2}}"#
        ),
    )
    .unwrap();
    fs::write(
        dir.join(".zattrs"),
        r#"{"_ARRAY_DIMENSIONS":["latitude","longitude"],"units":"K","crs":"EPSG:4326"}"#,
    )
    .unwrap();
    fs::write(dir.join("0.0"), chunk).unwrap();
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{}: no {from}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

// RFC 8878: magic, descriptor, [window], [content size], then blocks.
fn zstd_frame(descriptor: u8, window: Option<u8>, size: &[u8], blocks: &[u8]) -> Vec<u8> {
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, descriptor];
    f.extend(window);
    f.extend(size);
    f.extend(blocks);
    f
}

fn raw_block(data: &[u8]) -> Vec<u8> {
    let header = (data.len() as u32) << 3 | 1;
    let mut b = header.to_le_bytes()[..3].to_vec();
    b.extend(data);
    b
}

/// 30,000 run-length blocks of 128 KiB each: 120 kB that decode to 3.9 GB.
fn zstd_bomb() -> Vec<u8> {
    let mut blocks = Vec::new();
    let count = 30_000;
    for i in 0..count {
        let last = u32::from(i + 1 == count);
        let header = (131_072u32 << 3) | (1 << 1) | last;
        blocks.extend(&header.to_le_bytes()[..3]);
        blocks.push(7);
    }
    // Window descriptor 0x38: 128 KiB.
    zstd_frame(0x00, Some(0x38), &[], &blocks)
}

/// 64 MiB of zeros as a zlib stream: about 64 kB.
fn zlib_bomb() -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
    let block = vec![0u8; 1 << 20];
    for _ in 0..64 {
        z.write_all(&block).unwrap();
    }
    z.finish().unwrap()
}

struct Probe {
    name: &'static str,
    /// Text the reader's report must hold: an error's, or `cells_` for a store that is read.
    expected: &'static str,
    /// Only under a memory limit: with none, the read succeeds by taking the memory.
    limited_only: bool,
}

const ZSTD: &str = r#"{"id":"zstd","level":3}"#;

/// The sound store, then one change to it.
fn mutate<'a>(change: &'a dyn Fn(&Path)) -> impl Fn(&Path) + 'a {
    move |dir: &Path| {
        base(dir);
        change(dir);
    }
}

/// Build every probe under `root`, one folder each.
fn build(root: &Path) -> Vec<Probe> {
    let mut probes = Vec::new();
    let mut add = |name: &'static str, expected: &'static str, make: &dyn Fn(&Path)| {
        let dir = root.join(name);
        make(&dir);
        probes.push(Probe {
            name,
            expected,
            limited_only: false,
        });
    };
    let var = |dir: &Path, file: &str| dir.join(VARIABLE).join(file);

    // Controls: a sound store reads, and so does one with a chunk absent.
    add("ok_base", "cells_", &|d| base(d));
    add(
        "ok_missing_chunk",
        "cells_",
        &mutate(&|d| fs::remove_file(var(d, "1.1.1")).unwrap()),
    );

    // Chunks of the wrong size.
    add(
        "chunk_truncated",
        "10 stored bytes, and the chunk has 24",
        &mutate(&|d| fs::write(var(d, "0.0.0"), [0u8; 10]).unwrap()),
    );
    add(
        "chunk_empty",
        "0 stored bytes, and the chunk has 24",
        &mutate(&|d| fs::write(var(d, "0.0.0"), []).unwrap()),
    );
    add(
        "chunk_long",
        "25 stored bytes, and the chunk has 24",
        &mutate(&|d| fs::write(var(d, "0.0.0"), [0u8; 25]).unwrap()),
    );
    add(
        "chunk_far_too_long",
        "over the 4121 it may",
        &mutate(&|d| fs::write(var(d, "0.0.0"), vec![0u8; 1 << 20]).unwrap()),
    );
    add(
        "chunk_is_a_folder",
        "is not a regular file",
        &mutate(&|d| {
            fs::remove_file(var(d, "0.0.0")).unwrap();
            fs::create_dir(var(d, "0.0.0")).unwrap();
        }),
    );

    // Declared sizes.
    add(
        "shape_huge",
        "has no axis",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"shape\":[4,4,6]",
                "\"shape\":[4,4000000000,4000000000]",
            )
        }),
    );
    add(
        "shape_huge_with_axes",
        "over the limit of 16777216",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"shape\":[4,4,6]",
                "\"shape\":[4,1099511627776,6]",
            );
            edit(
                &d.join("latitude/.zarray"),
                "\"shape\":[4]",
                "\"shape\":[1099511627776]",
            );
        }),
    );
    add(
        "shape_not_whole",
        "not whole numbers",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"shape\":[4,4,6]",
                "\"shape\":[4,1e30,6]",
            )
        }),
    );
    add(
        "shape_negative",
        "not whole numbers",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"shape\":[4,4,6]",
                "\"shape\":[4,-4,6]",
            )
        }),
    );
    add(
        "shape_nine_dimensions",
        "over the limit of 8",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"shape\":[4,4,6]",
                "\"shape\":[1,1,1,1,1,1,4,4,6]",
            )
        }),
    );
    add(
        "chunks_huge",
        "max_chunk_bytes",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"chunks\":[2,2,3]",
                "\"chunks\":[1048576,1048576,1048576]",
            )
        }),
    );
    add(
        "chunks_overflow",
        "max_chunk_bytes",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"chunks\":[2,2,3]",
                "\"chunks\":[9223372036854775807,9223372036854775807,9223372036854775807]",
            )
        }),
    );
    add(
        "chunks_zero",
        "a chunk has no extent",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"chunks\":[2,2,3]",
                "\"chunks\":[0,2,3]",
            )
        }),
    );
    add(
        "axis_chunk_huge",
        "max_chunk_bytes",
        &mutate(&|d| {
            edit(
                &d.join("latitude/.zarray"),
                "\"chunks\":[4]",
                "\"chunks\":[1099511627776]",
            )
        }),
    );

    // JSON.
    add(
        "json_cut_short",
        "JSON is malformed",
        &mutate(&|d| fs::write(var(d, ".zarray"), "{\"chunks\":[2,2,3").unwrap()),
    );
    add(
        "json_nested_a_million_deep",
        "nesting over the limit",
        &mutate(&|d| fs::write(var(d, ".zattrs"), vec![b'['; 1 << 20]).unwrap()),
    );
    add(
        "json_over_the_size_limit",
        "over the 16777216 it may",
        &mutate(&|d| fs::write(var(d, ".zattrs"), vec![b' '; (16 << 20) + 1]).unwrap()),
    );
    add(
        "json_key_twice",
        "a key appears twice",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"order\":\"C\"",
                "\"order\":\"C\",\"order\":\"F\"",
            )
        }),
    );
    add(
        "json_not_text",
        "JSON is malformed",
        &mutate(&|d| fs::write(d.join(".zgroup"), [0xFFu8, 0xFE, 0x00, 0x7B]).unwrap()),
    );
    add(
        "group_version_3",
        "only 2 is read",
        &mutate(&|d| fs::write(d.join(".zgroup"), "{\"zarr_format\":3}").unwrap()),
    );
    add(
        "consolidated_not_an_object",
        "no metadata object",
        &mutate(&|d| {
            fs::write(
                d.join(".zmetadata"),
                "{\"zarr_consolidated_format\":1,\"metadata\":[]}",
            )
            .unwrap()
        }),
    );

    // Keys that would leave the store. The folder beside the store holds an array a reader that
    // followed them would list.
    add(
        "keys_leave_the_store",
        "not an array in the root group",
        &mutate(&|d| {
            let zarray = fs::read_to_string(var(d, ".zarray")).unwrap();
            let zattrs = fs::read_to_string(var(d, ".zattrs")).unwrap();
            let outside = d.join("../keys_outside");
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join(".zarray"), &zarray).unwrap();
            let mut entries = vec!["\".zgroup\":{\"zarr_format\":2}".to_string()];
            for key in [
                "../keys_outside",
                "..",
                "/absolute",
                "C:\\\\absolute",
                "a/b",
                "NUL",
                ".hidden",
            ] {
                entries.push(format!("\"{key}/.zarray\":{zarray}"));
                entries.push(format!("\"{key}/.zattrs\":{zattrs}"));
            }
            fs::write(
                d.join(".zmetadata"),
                format!(
                    "{{\"zarr_consolidated_format\":1,\"metadata\":{{{}}}}}",
                    entries.join(",")
                ),
            )
            .unwrap();
        }),
    );
    add(
        "consolidated_disagrees",
        "and the store's .zmetadata disagree",
        &mutate(&|d| {
            let zarray = fs::read_to_string(var(d, ".zarray")).unwrap();
            let other = zarray.replace("\"shape\":[4,4,6]", "\"shape\":[4,4,5]");
            fs::write(
                d.join(".zmetadata"),
                format!(
                    "{{\"zarr_consolidated_format\":1,\"metadata\":{{\".zgroup\":{{\"zarr_format\":2}},\"{VARIABLE}/.zarray\":{other}}}}}"
                ),
            )
            .unwrap();
        }),
    );
    add(
        "separator_backslash",
        "dimension_separator is not . or /",
        &mutate(&|d| {
            edit(
                &var(d, ".zarray"),
                "\"order\":\"C\"",
                "\"order\":\"C\",\"dimension_separator\":\"\\\\\"",
            )
        }),
    );

    // What is not read.
    let swap = |from: &'static str, to: &'static str| {
        move |d: &Path| {
            base(d);
            edit(&d.join(VARIABLE).join(".zarray"), from, to);
        }
    };
    add(
        "blosc",
        "blosc, which is C",
        &swap(
            "\"compressor\":null",
            "\"compressor\":{\"id\":\"blosc\",\"cname\":\"lz4\",\"clevel\":5,\"shuffle\":1}",
        ),
    );
    add(
        "compressor_unknown",
        "has no reader here",
        &swap("\"compressor\":null", "\"compressor\":{\"id\":\"lzma\"}"),
    );
    add(
        "compressor_no_id",
        "its compressor has no id",
        &swap("\"compressor\":null", "\"compressor\":7"),
    );
    add(
        "filter",
        "has no reader here; no filter does",
        &swap(
            "\"filters\":null",
            "\"filters\":[{\"id\":\"shuffle\",\"elementsize\":2}]",
        ),
    );
    add(
        "fortran_order",
        "Fortran order",
        &swap("\"order\":\"C\"", "\"order\":\"F\""),
    );
    add(
        "complex_cells",
        "is not a number this reader takes",
        &swap("\"dtype\":\"<i2\"", "\"dtype\":\"<c16\""),
    );
    add(
        "structured_cells",
        "a structured data type",
        &swap("\"dtype\":\"<i2\"", "\"dtype\":[[\"a\",\"<i2\"]]"),
    );
    add(
        "array_version_3",
        "only version 2 is read",
        &swap("\"zarr_format\":2", "\"zarr_format\":3"),
    );

    // Axes and time.
    add(
        "time_units_unknown",
        "has no rule here",
        &mutate(&|d| {
            edit(
                &d.join("time/.zattrs"),
                "hours since 2000-01-01 00:00:00",
                "fortnights since 2000-01-01",
            )
        }),
    );
    add(
        "time_not_a_number",
        "missing or inexact coordinate",
        &mutate(&|d| {
            let mut times = le_f64(&[0.0, 1.0, 2.0, 3.0]);
            times[8..16].copy_from_slice(&f64::NAN.to_le_bytes());
            fs::write(d.join("time/0"), times).unwrap();
        }),
    );
    add(
        "time_out_of_range",
        "not a whole second in the years",
        &mutate(&|d| fs::write(d.join("time/0"), le_f64(&[0.0, 1.0, 2.0, 1e300])).unwrap()),
    );
    add(
        "latitude_uneven",
        "evenly spaced",
        &mutate(&|d| fs::write(d.join("latitude/0"), le_f64(&[30.0, 30.5, 31.0, 40.0])).unwrap()),
    );
    add(
        "latitude_chunk_absent",
        "has no file in the store",
        &mutate(&|d| fs::remove_file(d.join("latitude/0")).unwrap()),
    );
    add(
        "fill_value_and_attribute_differ",
        "differ",
        &swap("\"fill_value\":null", "\"fill_value\":-2"),
    );

    // Numbers that are not numbers a cell can be scaled by (review of 8fc6610a).
    let scaled = |value: &'static str| {
        move |d: &Path| {
            base(d);
            edit(
                &d.join(VARIABLE).join(".zattrs"),
                "\"units\":\"K\"",
                &format!("\"units\":\"K\",\"scale_factor\":{value}"),
            );
        }
    };
    add(
        "scale_not_a_number",
        "its scale_factor is not finite",
        &scaled("NaN"),
    );
    add(
        "scale_infinite",
        "its scale_factor is not finite",
        &scaled("Infinity"),
    );
    add(
        "scale_overflows",
        "its scale_factor is not finite",
        &scaled("1e999"),
    );
    // The group's own attributes are not read, so nothing in them is parsed.
    add(
        "ok_root_attributes_nested_deep",
        "cells_",
        &mutate(&|d| fs::write(d.join(".zattrs"), vec![b'['; 100_000]).unwrap()),
    );

    // Zstandard frames against a chunk of 2 MiB.
    let chunk = 2 << 20;
    let frames: [(&'static str, &'static str, Vec<u8>); 7] = [
        (
            "zstd_states_a_huge_size",
            "states 9223372036854775808 bytes",
            zstd_frame(0xE0, None, &(1u64 << 63).to_le_bytes(), &raw_block(b"abcd")),
        ),
        (
            "zstd_states_a_huge_window",
            "window of",
            zstd_frame(0x00, Some(0xFF), &[], &raw_block(b"abcd")),
        ),
        (
            "zstd_decodes_to_gigabytes",
            "holds more than the chunk's",
            zstd_bomb(),
        ),
        (
            "zstd_cut_short",
            "damaged",
            zstd_frame(0x00, Some(0x38), &[], &[0xFF, 0xFF])
                .into_iter()
                .chain(vec![0x55; 100])
                .collect(),
        ),
        (
            "zstd_ends_early",
            "ends after 4 of the chunk's",
            zstd_frame(0x00, Some(0x38), &[], &raw_block(b"abcd")),
        ),
        ("zstd_not_a_frame", "not a Zstandard frame", vec![0u8; 64]),
        (
            "zstd_needs_a_dictionary",
            "needs a dictionary",
            zstd_frame(0x01, Some(0x38), &[9], &raw_block(b"abcd")),
        ),
    ];
    for (name, expected, frame) in frames {
        assert!(frame.len() < chunk / 8);
        add(name, expected, &|d| wide(d, ZSTD, &frame));
    }
    let deflate = zlib_bomb();
    add(
        "zlib_decodes_to_64_mib",
        "holds more than the chunk's",
        &|d| wide(d, r#"{"id":"zlib","level":9}"#, &deflate),
    );
    add("zlib_cut_short", "incomplete deflate stream", &|d| {
        wide(d, r#"{"id":"zlib","level":9}"#, &deflate[..40])
    });

    // The real AORC store's whole grid, two steps: 35 million cells a step, 282 MB. With no limit
    // it is read (every chunk of those steps is absent). Under one, it is a typed refusal.
    let aorc = root.join("aorc_whole_grid");
    copy_dir(&fixtures().join("aorc_2019_subset.zarr"), &aorc);
    probes.push(Probe {
        name: "aorc_whole_grid",
        expected: "could not allocate 70585202 cells",
        limited_only: true,
    });
    probes
}

/// What `zarr_dump --max-steps 2` reports for a store, without writing files.
fn report(store: &Path) -> String {
    let store = match ZarrStore::open(store) {
        Ok(store) => store,
        Err(error) => return format!("file_error: {error}"),
    };
    let mut out = format!("left_out: {:?}\n", store.left_out());
    for name in store.variable_names() {
        let result = store.variable(&name).and_then(|v| {
            let window = v.full_window();
            v.grid(&window)?;
            let cells = v.read(0..v.step_count().min(2), &window)?;
            Ok(cells)
        });
        match result {
            Ok(cells) => out.push_str(&format!(
                "cells_{name}: {} of which {} missing\n",
                cells.len(),
                cells.iter().filter(|c| c.is_nan()).count()
            )),
            Err(error) => out.push_str(&format!("{name}: {error}\n")),
        }
    }
    out
}

#[test]
fn every_malformed_store_is_a_typed_error_or_a_correct_result() {
    let root = scratch("zarr_hostile_in_process");
    let probes = build(&root);
    assert_eq!(probes.len(), 54);
    let mut errors = 0;
    for probe in probes.iter().filter(|p| !p.limited_only) {
        let said = report(&root.join(probe.name));
        assert!(said.contains(probe.expected), "{}: {said}", probe.name);
        errors += usize::from(!probe.expected.starts_with("cells_"));
    }
    assert_eq!(errors, 50);

    // The controls, in full. A sound store reads every cell.
    let ok = report(&root.join("ok_base"));
    assert!(
        ok.contains("cells_TMP_2maboveground: 48 of which 0 missing"),
        "{ok}"
    );
    // A chunk with no file is missing cells, exactly its own: chunk 1.1.1 is steps 2 and 3, rows
    // 2 and 3, columns 3 to 5, so steps 0 and 1 are whole and step 2 lacks six cells.
    let store = ZarrStore::open(&root.join("ok_missing_chunk")).unwrap();
    let v = store.variable(VARIABLE).unwrap();
    let window = v.full_window();
    let cells = v.read(0..4, &window).unwrap();
    let missing: Vec<usize> = (0..96).filter(|i| cells[*i].is_nan()).collect();
    let want: Vec<usize> = [2usize, 3]
        .iter()
        .flat_map(|t| [2usize, 3].map(|r| t * 24 + r * 6 + 3..t * 24 + r * 6 + 6))
        .flatten()
        .collect();
    assert_eq!(missing, want);
    assert_eq!(cells[0], 0.0);
    assert_eq!(cells[80], 80.0);
    // Its stored bytes do not exist, and are not invented.
    let e = v.read_stored(2..3, &window).unwrap_err();
    assert!(e.to_string().contains("has no file"), "{e}");
    // The folder a traversing key names was not read: nothing of it is listed.
    let outside = report(&root.join("keys_leave_the_store"));
    assert!(!outside.contains("cells_"), "{outside}");
    assert_eq!(outside.matches("not an array in the root group").count(), 7);
}

/// A stop requested from outside ends a read with a typed error, and a time limit of zero does.
#[test]
fn a_read_stops_when_asked_and_at_its_time_limit() {
    use dss_vortex::source::zarr::Limits;
    let root = scratch("zarr_hostile_stop");
    base(&root);
    let store = ZarrStore::open(&root).unwrap();
    let v = store.variable(VARIABLE).unwrap();
    let window = v.full_window();
    assert_eq!(v.read(0..4, &window).unwrap().len(), 96);
    store.request_stop();
    let e = v.read(0..4, &window).unwrap_err();
    assert!(e.to_string().contains("the read was stopped"), "{e}");
    store.clear_stop();
    assert_eq!(v.read(0..4, &window).unwrap().len(), 96);

    let timed = Limits {
        max_seconds_per_read: Some(0),
        ..Limits::default()
    };
    let e = ZarrStore::open_with_limits(&root, timed)
        .unwrap()
        .variable(VARIABLE)
        .unwrap_err();
    assert!(e.to_string().contains("max_seconds_per_read"), "{e}");

    // Each limit refuses by name, and a larger one reads.
    for (limits, name) in [
        (
            Limits {
                max_chunks_per_read: 3,
                chunk_cache_bytes: 0,
                ..Limits::default()
            },
            "max_chunks_per_read",
        ),
        (
            Limits {
                max_decoded_bytes_per_read: 100,
                chunk_cache_bytes: 0,
                ..Limits::default()
            },
            "max_decoded_bytes_per_read",
        ),
        (
            Limits {
                max_cells: 23,
                ..Limits::default()
            },
            "max_cells",
        ),
        (
            Limits {
                max_read_bytes: 383,
                ..Limits::default()
            },
            "max_read_bytes",
        ),
    ] {
        let store = ZarrStore::open_with_limits(&root, limits).unwrap();
        let v = store.variable(VARIABLE).unwrap();
        let e = v.read(0..4, &v.full_window()).unwrap_err();
        assert!(e.to_string().contains(name), "{name}: {e}");
    }
    let enough = Limits {
        max_chunks_per_read: 8,
        max_decoded_bytes_per_read: 192,
        max_cells: 24,
        max_read_bytes: 384,
        chunk_cache_bytes: 0,
        ..Limits::default()
    };
    let store = ZarrStore::open_with_limits(&root, enough).unwrap();
    let v = store.variable(VARIABLE).unwrap();
    assert_eq!(v.read(0..4, &v.full_window()).unwrap().len(), 96);
}

/// `target/<profile>/examples/zarr_dump`, built here if this test ran alone.
fn zarr_dump() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    let profile = test.parent().unwrap().parent().unwrap();
    let tool = profile
        .join("examples")
        .join(format!("zarr_dump{}", std::env::consts::EXE_SUFFIX));
    if !tool.exists() {
        let release = profile.file_name().is_some_and(|name| name == "release");
        let mut build = Command::new(env!("CARGO"));
        build.args([
            "build",
            "--locked",
            "-p",
            "dss-vortex",
            "--example",
            "zarr_dump",
        ]);
        if release {
            build.arg("--release");
        }
        assert!(build.status().unwrap().success());
    }
    tool
}

fn has_job_object() -> bool {
    cfg!(windows)
        && Command::new("python")
            .args(["-I", "-c", "pass"])
            .status()
            .is_ok_and(|status| status.success())
}

fn run_in_processes(limit_mib: u32) {
    let root = scratch(&format!("zarr_hostile_job_{limit_mib}"));
    let (input, out) = (root.join("in"), root.join("out"));
    fs::create_dir_all(&input).unwrap();
    let probes = build(&input);
    // A folder beside the probes that is not one.
    let _ = fs::remove_dir_all(input.join("keys_outside"));
    let tool = zarr_dump();
    let limited = has_job_object();
    if limited {
        let job = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/zarr_job.py");
        let output = Command::new("python")
            .arg("-I")
            .arg(&job)
            .arg(&tool)
            .arg(&input)
            .arg(&out)
            .arg(limit_mib.to_string())
            .args(["--max-steps", "2"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{text}");
        assert!(output.status.success(), "{limit_mib} MiB: {text}");
        assert!(
            text.contains(&format!("RESULT PASS 0 of {}", probes.len())),
            "{text}"
        );
    } else {
        assert!(
            !(cfg!(windows) && std::env::var_os("CI").is_some()),
            "Windows CI needs `python` on the path for the Job Object"
        );
        for probe in probes.iter().filter(|p| !p.limited_only) {
            let status = Command::new(&tool)
                .arg(input.join(probe.name))
                .arg(out.join(probe.name))
                .args(["--max-steps", "2"])
                .status()
                .unwrap();
            assert!(status.success(), "{}: {status}", probe.name);
        }
    }
    let mut checked = 0;
    for probe in probes.iter().filter(|p| limited || !p.limited_only) {
        let dump = fs::read_to_string(out.join(probe.name).join("dtos.json"))
            .unwrap_or_else(|e| panic!("{}: no dump: {e}", probe.name));
        assert!(
            dump.contains(probe.expected),
            "{}: wanted {}: {dump}",
            probe.name,
            probe.expected
        );
        checked += 1;
    }
    assert_eq!(checked, if limited { 54 } else { 53 }, "probes checked");
}

#[test]
fn malformed_stores_end_no_process_at_128_mib() {
    run_in_processes(128);
}

#[test]
fn malformed_stores_end_no_process_at_64_mib() {
    run_in_processes(64);
}

/// Two real AORC chunks, 144 hours of 256 x 256 cells: 18.9 MB decoded and 37.7 MB returned. At
/// 128 MiB the window is read; at 48 and 16 MiB the reader runs out of memory, and that is a typed
/// error from a fallible reservation, never the end of the process.
#[test]
fn real_chunks_under_a_memory_limit_read_or_refuse() {
    if !has_job_object() {
        eprintln!("skipped: it needs the Windows Job Object's memory limit");
        return;
    }
    let job = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/zarr_job.py");
    let tool = zarr_dump();
    for limit in [128u32, 48, 16] {
        let root = scratch(&format!("zarr_hostile_real_{limit}"));
        let input = root.join("in");
        copy_dir(
            &fixtures().join("aorc_2019_subset.zarr"),
            &input.join("aorc.zarr"),
        );
        let output = Command::new("python")
            .arg("-I")
            .arg(&job)
            .arg(&tool)
            .arg(&input)
            .arg(root.join("out"))
            .arg(limit.to_string())
            .args(["--variable", "APCP_surface", "--steps", "6192:6336"])
            .args(["--window", "1664:1920,3328:3584"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{limit} MiB: {text}");
        assert!(text.contains("RESULT PASS 0 of 1"), "{limit} MiB: {text}");
        let dump = fs::read_to_string(root.join("out/aorc.zarr/dtos.json")).unwrap();
        if limit == 128 {
            assert!(
                dump.contains("\"cells\":\"cells_APCP_surface.f32\""),
                "{dump}"
            );
            let cells = fs::metadata(root.join("out/aorc.zarr/cells_APCP_surface.f32")).unwrap();
            assert_eq!(cells.len(), 144 * 256 * 256 * 4);
        } else {
            assert!(dump.contains("could not allocate"), "{limit} MiB: {dump}");
        }
    }
}

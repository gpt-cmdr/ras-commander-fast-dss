//! Files that once aborted the process, and others built to the same pattern, through the real entry
//! points. Each returns a typed error under default limits. An abort here fails the whole test binary,
//! which is the point: `catch_unwind` could not have caught one.
//!
//! Set `NC_HOSTILE_OUT` to write the files to a directory, for `tests/support/nc_job.py`.

mod nc_raw;

use dss_vortex::source::netcdf::{Limits, NetcdfFile};
use dss_vortex::VortexError;
use netcdf_reader::{NcFile, NcMetadataMode, NcOpenOptions};
use std::fs;
use std::path::Path;

fn dump(name: &str, bytes: &[u8]) {
    if let Some(dir) = std::env::var_os("NC_HOSTILE_OUT") {
        fs::create_dir_all(&dir).unwrap();
        fs::write(Path::new(&dir).join(name), bytes).unwrap();
    }
}

fn refused(name: &str, bytes: Vec<u8>, reason: &str) {
    dump(name, &bytes);
    let size = bytes.len();
    let error = NetcdfFile::from_bytes(name, bytes, Limits::default()).expect_err(name);
    assert!(
        matches!(error, VortexError::Refused { .. }) && error.to_string().contains(reason),
        "{name}, {size} bytes: {error}"
    );
}

#[test]
fn a_group_cycle_is_refused_before_the_reader_recurses() {
    refused(
        "group_cycle_raw.nc4",
        nc_raw::group_cycle(),
        "more than one link",
    );
    // A real file: a committed fixture with one hard link added by h5py.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/netcdf_hostile/group_cycle.nc4");
    let error = NetcdfFile::open(&path).unwrap_err();
    assert!(error.to_string().contains("more than one link"), "{error}");
}

#[test]
fn doubled_links_are_refused_without_walking_every_path() {
    // 2^30 paths. Walked in full, this does not return in any useful time.
    refused(
        "group_doubled.nc4",
        nc_raw::group_doubled(30),
        "more than one link",
    );
}

#[test]
fn group_depth_is_limited_before_the_reader_recurses() {
    let accepted = NetcdfFile::from_bytes("deep.nc4", nc_raw::group_chain(32), Limits::default());
    assert!(accepted.is_ok(), "{accepted:?}");
    refused(
        "group_chain_33.nc4",
        nc_raw::group_chain(33),
        "nested over 32",
    );
    refused(
        "group_chain_4096.nc4",
        nc_raw::group_chain(4096),
        "nested over 32",
    );
}

#[test]
fn one_block_shared_by_many_headers_is_counted_each_time() {
    // 4 GiB of parsed attributes from about 1.2 MiB, with each header parsed once.
    let bytes = nc_raw::shared_continuation(4000, 1 << 20);
    assert!(bytes.len() < 2 << 20);
    refused("shared_continuation.nc4", bytes, "metadata");
}

#[test]
fn one_heap_object_referenced_many_times_is_counted_each_time() {
    // 4 GiB of strings from about 1 MiB, and the reader's heap cache makes the copies free to read.
    let bytes = nc_raw::repeated_heap_object(4000, 1 << 20);
    assert!(bytes.len() < 2 << 20);
    refused("repeated_heap_object.nc4", bytes, "metadata");
}

#[test]
fn one_name_shared_by_many_links_is_counted_each_time() {
    // 3.9 GiB of names from about 5 MiB.
    let bytes = nc_raw::repeated_name(60_000, 1 << 16);
    assert!(bytes.len() < 8 << 20);
    refused("repeated_name.nc4", bytes, "metadata");
    let accepted = NetcdfFile::from_bytes("x.nc4", nc_raw::repeated_name(2, 64), Limits::default());
    assert!(accepted.is_ok(), "{accepted:?}");
}

#[test]
fn a_header_parsed_for_every_shared_reference_is_counted_each_time() {
    let bytes = nc_raw::repeated_shared_message(3000, 1 << 20);
    refused("repeated_shared_message.nc4", bytes, "metadata");
}

#[test]
fn classic_header_counts_are_bounded_before_the_reader_reserves_them() {
    // A crafted file: the count fits the padding, so the reader's own check passes and it asks
    // for 120 MB at once.
    let bytes = nc_raw::classic_dimension_count(3_000_000);
    assert!(bytes.len() >= 48_000_016);
    let error = NetcdfFile::from_bytes("padded.nc", bytes, Limits::default()).unwrap_err();
    assert!(matches!(error, VortexError::Refused { .. }), "{error}");
    assert!(error.to_string().contains("metadata limit"), "{error}");
    // Within the limit, the same shape is a normal typed error for the file ending early.
    let error = NetcdfFile::from_bytes(
        "padded.nc",
        nc_raw::classic_dimension_count(100),
        Limits::default(),
    );
    assert!(error.is_err(), "{error:?}");
    // Each variable clones its dimensions' names: 200 GB from under 2 MiB.
    let bytes = nc_raw::classic_repeated_dimension(1 << 20, 200_000);
    assert!(bytes.len() < 2 << 20);
    refused("classic_repeated_dimension.nc", bytes, "metadata limit");
    // A count past the bytes present is an early end, not a reservation.
    let mut short = nc_raw::classic_dimension_count(4);
    short[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(NetcdfFile::from_bytes("short.nc", short, Limits::default()).is_err());
}

fn hostile_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/netcdf_hostile")
        .join(name);
    fs::read(path).unwrap()
}

#[test]
fn a_chunk_list_built_from_the_shape_is_limited_before_the_reader_builds_it() {
    // 23 KB. The reader's extensible array path lists 2^24 chunks from the shape, and asked for
    // 268 MB at once in a process limited to 400 MiB, which aborted (measured).
    // It opens: the limit is on what a read takes, and this read is refused before the reader lists.
    let file = NetcdfFile::from_bytes(
        "chunks_over_limit.nc4",
        hostile_fixture("chunks_over_limit.nc4"),
        Limits::default(),
    )
    .unwrap();
    let error = file.variable("data").unwrap().step(0).err().unwrap();
    assert!(matches!(error, VortexError::Refused { .. }), "{error}");
    assert!(
        error.to_string().contains("over the limit of 4194304"),
        "{error}"
    );
    // The limit is the caller's. The chunk count is what was refused, and at the limit a read works.
    let wider = Limits {
        max_chunks_per_read: 1 << 24,
        ..Limits::default()
    };
    assert!(
        NetcdfFile::from_bytes("x.nc4", hostile_fixture("chunks_over_limit.nc4"), wider).is_ok()
    );
    let file = NetcdfFile::from_bytes(
        "x.nc4",
        hostile_fixture("chunks_at_limit.nc4"),
        Limits::default(),
    )
    .unwrap();
    let step = file.variable("data").unwrap().step(0).unwrap();
    assert_eq!(step.cells.len(), 512 * 512);
}

/// `fixed_array.nc4` with its array's header claiming `entries` entries in pages of `2^page_bits`.
fn false_fixed_array(entries: u64, page_bits: u8, pad_to: usize) -> Vec<u8> {
    let mut bytes = hostile_fixture("fixed_array.nc4");
    let at = bytes.windows(4).position(|w| w == b"FAHD").unwrap();
    bytes[at + 7] = page_bits;
    bytes[at + 8..at + 16].copy_from_slice(&entries.to_le_bytes());
    let sum = hdf5_reader::checksum::jenkins_lookup3(&bytes[at..at + 24]);
    bytes[at + 24..at + 28].copy_from_slice(&sum.to_le_bytes());
    if bytes.len() < pad_to {
        bytes.resize(pad_to, 0);
    }
    bytes
}

#[test]
fn a_fixed_array_cannot_claim_more_entries_than_its_dataset_has_chunks() {
    // `page_offsets` makes a table of this many pages. The padding satisfies the only check on it,
    // that the page bitmap is in the file: 350 million pages is a request for 5.6 GB.
    refused(
        "fixed_array_pages.nc4",
        false_fixed_array(700_000_000, 1, 48_000_016),
        "declares",
    );
    // The real count still opens and reads.
    let file = NetcdfFile::from_bytes(
        "x.nc4",
        hostile_fixture("fixed_array.nc4"),
        Limits::default(),
    )
    .unwrap();
    let step = file.variable("data").unwrap().step(0).unwrap();
    assert_eq!(step.cells.len(), 64 * 64);
}

#[test]
fn a_filter_stage_with_no_known_size_is_refused_beside_others() {
    // `apply_pipeline_with_limit` has no size for a scale-offset stage's output, so deflate before it
    // runs up to 4 GiB, and the next stage copies that infallibly.
    refused(
        "scaleoffset_deflate.nc4",
        hostile_fixture("scaleoffset_deflate.nc4"),
        "other filters beside it",
    );
    // Alone it has the chunk's size, and opens.
    let file = NetcdfFile::from_bytes(
        "x.nc4",
        hostile_fixture("scaleoffset_alone.nc4"),
        Limits::default(),
    );
    assert!(file.is_ok(), "{:?}", file.err());
}

#[test]
fn the_decoded_limit_counts_each_chunk_of_the_selection_at_its_edge() {
    // 64 x 64 of f4 in 8 x 8 chunks with no filter: a step decodes 64 chunks of 256 bytes.
    let open = |limit: u64| {
        let limits = Limits {
            max_decoded_bytes_per_read: limit,
            ..Limits::default()
        };
        NetcdfFile::from_bytes("x.nc4", hostile_fixture("fixed_array.nc4"), limits).unwrap()
    };
    let file = open(64 * 256);
    assert_eq!(
        file.variable("data").unwrap().step(0).unwrap().cells.len(),
        64 * 64
    );
    let file = open(64 * 256 - 1);
    let error = file.variable("data").unwrap().step(0).err().unwrap();
    assert!(matches!(error, VortexError::Refused { .. }), "{error}");
    assert!(error.to_string().contains("would decode 16384"), "{error}");
    // Each filter decodes the whole chunk: this 128 MiB chunk has deflate and shuffle over it.
    let limits = Limits {
        max_decoded_bytes_per_read: (1 << 28) - 1,
        ..Limits::default()
    };
    let file =
        NetcdfFile::from_bytes("x.nc4", hostile_fixture("chunk_over_step.nc4"), limits).unwrap();
    let error = file.variable("data").unwrap().step(0).err().unwrap();
    assert!(
        error.to_string().contains("would decode 268435456"),
        "{error}"
    );
}

#[test]
fn a_year_of_aorc_chunks_is_limited_by_what_a_step_touches() {
    // 144 x 128 x 256 of i2 under shuffle and deflate. A step takes 33 x 33 chunks, 20.6 GB of filter
    // output; refused at 8 GiB, and Vortex 1.1.0 reads it. Everything but one chunk is unwritten.
    let bytes = hostile_fixture("aorc_year_shape.nc4");
    let file = NetcdfFile::from_bytes("x.nc4", bytes.clone(), Limits::default()).unwrap();
    let step = file.variable("APCP_surface").unwrap().step(144).unwrap();
    assert_eq!((step.nx, step.ny), (8401, 4201));
    let needed = 1089u64 * 144 * 128 * 256 * 2 * 2;
    let limited = |limit| {
        let limits = Limits {
            max_decoded_bytes_per_read: limit,
            ..Limits::default()
        };
        NetcdfFile::from_bytes("x.nc4", bytes.clone(), limits).unwrap()
    };
    let error = limited(needed - 1)
        .variable("APCP_surface")
        .unwrap()
        .step(0)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("would decode 20554186752"),
        "{error}"
    );
    assert!(limited(needed)
        .variable("APCP_surface")
        .unwrap()
        .step(0)
        .is_ok());
    assert_eq!(
        limited(needed)
            .variable("APCP_surface")
            .unwrap()
            .step_count(),
        8784
    );
}

#[test]
fn the_default_read_limit_keeps_aorcs_shape_and_a_refusal_names_the_setting() {
    // The largest real read is 0.279 GiB, and the AORC-shaped synthetic needs 20.6 GB. A small file
    // that asks for 64 GiB is refused, and the setting that admits it is in the message.
    let limits = Limits::default();
    assert_eq!(limits.max_decoded_bytes_per_read, 32 << 30);
    assert_eq!(limits.max_seconds_per_read, None);
    let tight = Limits {
        max_decoded_bytes_per_read: (1 << 28) - 1,
        ..Limits::default()
    };
    let file =
        NetcdfFile::from_bytes("x.nc4", hostile_fixture("chunk_over_step.nc4"), tight).unwrap();
    let error = file.variable("data").unwrap().step(0).err().unwrap();
    assert!(
        error.to_string().contains("max_decoded_bytes_per_read"),
        "{error}"
    );
}

#[test]
fn a_cache_that_holds_a_step_hits_on_the_next_step() {
    // 1,024 chunks a step and 521 slots gave no hits at all, whatever the bytes. ERA5 paid 0.41 s a
    // step where it could pay 0.016 s.
    let read_two = |limits: Limits| {
        let bytes = hostile_fixture("cache_step_1024.nc4");
        let file = NetcdfFile::from_bytes("x.nc4", bytes, limits).unwrap();
        let variable = file.variable("data").unwrap();
        let first = variable.step(0).unwrap();
        let after_first = file.chunk_cache_stats().unwrap().misses;
        let second = variable.step(1).unwrap();
        let stats = file.chunk_cache_stats().unwrap();
        // Cells, whatever the cache does: the second step is the first plus 1,024.
        assert_eq!(first.cells.len(), 1024);
        assert_eq!(second.cells[0], first.cells[0] + 1024.0);
        (after_first, stats)
    };
    // By default the cache is sized to a step.
    let (first, stats) = read_two(Limits::default());
    assert_eq!(stats.misses, first, "{stats:?}");
    assert!(stats.max_slots >= 1024, "{stats:?}");
    assert!(stats.max_bytes >= 1024 * 64, "{stats:?}");
    // Asked for, the caller's settings are what it gets.
    let fixed = Limits {
        chunk_cache_slots: 7,
        ..Limits::default()
    };
    let (_, stats) = read_two(fixed);
    assert_eq!(stats.max_slots, 7);
    // With sizing held off and room for only a few chunks, the next step misses.
    let small = Limits {
        chunk_cache_bytes: 1 << 10,
        max_sized_chunk_cache_bytes: 0,
        ..Limits::default()
    };
    let (first, stats) = read_two(small);
    assert_eq!(stats.max_bytes, 1 << 10);
    assert!(stats.misses > first, "{stats:?}");
    // A step over the sized bound keeps the floor: holding part of a step gets no hits.
    let (_, stats) = read_two(Limits {
        max_sized_chunk_cache_bytes: 1 << 10,
        ..Limits::default()
    });
    assert_eq!(stats.max_bytes, Limits::default().chunk_cache_bytes);
}

#[test]
fn a_read_can_be_stopped_while_it_decodes() {
    // 64 chunks of 256 MiB, deflated twice: about 9 s in one call, with nothing to stop it before.
    let bytes = hostile_fixture("double_deflate_8x8.nc4");
    let limits = Limits {
        max_seconds_per_read: Some(0),
        ..Limits::default()
    };
    let file = NetcdfFile::from_bytes("x.nc4", bytes.clone(), limits).unwrap();
    let started = std::time::Instant::now();
    // Resolving the variable reads its axes, which are reads too.
    let error = file
        .variable("data")
        .and_then(|variable| variable.step(0).map(|_| ()))
        .err()
        .unwrap();
    assert!(matches!(error, VortexError::Refused { .. }), "{error}");
    assert!(error.to_string().contains("time limit"), "{error}");
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());

    // From another thread.
    let file = NetcdfFile::from_bytes("x.nc4", bytes, Limits::default()).unwrap();
    let variable = file.variable("data").unwrap();
    file.request_stop();
    let error = variable.step(0).err().unwrap();
    assert!(error.to_string().contains("stopped"), "{error}");
    file.clear_stop();
}

#[test]
fn a_record_longer_than_the_chunk_limit_reads_one_chunk_a_step() {
    // 262,145 steps in netCDF-C's default (1, ny, nx) chunks, about 30 years of hours. Vortex reads
    // it. Refused whole when the limit was applied to the dataset rather than the read.
    let file = NetcdfFile::from_bytes(
        "long_record_262145.nc4",
        hostile_fixture("long_record_262145.nc4"),
        Limits::default(),
    )
    .unwrap();
    let variable = file.variable("data").unwrap();
    assert_eq!(variable.step(0).unwrap().cells, vec![1.0; 20]);
    assert_eq!(variable.step(262_144).unwrap().cells, vec![2.0; 20]);
}

#[test]
fn a_chunk_listed_many_times_is_an_error_in_process_too() {
    // The same shape, at 40 MB, with memory to list it all. It reaches the reader's own
    // check for repeated chunks. The negative control is the same index with one entry.
    let source = hostile_fixture("chunk_index_source.nc4");
    let file = NetcdfFile::from_bytes(
        "dup.nc4",
        nc_raw::duplicate_chunk_entries(&source, 1_000_000),
        Limits::default(),
    )
    .unwrap();
    let error = file.variable("data").unwrap().step(0).err().unwrap();
    assert!(error.to_string().contains("duplicate chunk"), "{error}");
    let file = NetcdfFile::from_bytes(
        "one.nc4",
        nc_raw::duplicate_chunk_entries(&source, 1),
        Limits::default(),
    )
    .unwrap();
    assert!(file.variable("data").unwrap().step(0).is_ok());
}

#[test]
fn the_metadata_limit_is_the_callers() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/netcdf_hostile/fill_16384.nc4");
    let limits = Limits {
        max_metadata_bytes: 1024,
        ..Limits::default()
    };
    let error = NetcdfFile::open_with_limits(&path, limits).unwrap_err();
    assert!(error.to_string().contains("metadata"), "{error}");
}

/// The reader alone, with no preflight, on a stack an eighth of what `on_decoder_stack` gives it.
fn read_on_small_stack(bytes: Vec<u8>) -> Result<(), String> {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let options = NcOpenOptions {
                metadata_mode: NcMetadataMode::Lossy,
                ..NcOpenOptions::default()
            };
            let file = NcFile::from_owner(bytes, options).map_err(|e| e.to_string())?;
            file.root_group().map(drop).map_err(|e| e.to_string())
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn recursion_at_each_limit_fits_an_eighth_of_the_decoder_stack() {
    // The group limit this crate sets.
    read_on_small_stack(nc_raw::group_chain(32)).unwrap();
    // hdf5-reader's own: MAX_DATATYPE_NESTING_DEPTH is 32, and past it a normal error.
    let past = read_on_small_stack(nc_raw::nested_attribute(33)).unwrap_err();
    assert!(past.contains("nesting"), "{past}");
    let _ = read_on_small_stack(nc_raw::nested_attribute(32));
    // MAX_BTREE_V1_RECURSION_DEPTH: past it too, so the deepest walk it allows has been taken.
    let past = read_on_small_stack(nc_raw::tree_chain(4096)).unwrap_err();
    assert!(past.contains("recursion limit"), "{past}");
    // Through this crate, whatever stack the caller has.
    let (opened, tree) = std::thread::Builder::new()
        .stack_size(64 << 10)
        .spawn(|| {
            let opened =
                NetcdfFile::from_bytes("deep.nc4", nc_raw::group_chain(32), Limits::default());
            let tree =
                NetcdfFile::from_bytes("tree.nc4", nc_raw::tree_chain(4096), Limits::default());
            (opened.is_ok(), tree.is_err())
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(opened && tree);
}

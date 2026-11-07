//! Files where the budget's walk once disagreed with what `netcdf-reader` then did, each run in its
//! own process through the built `nc_dump`. A stack overflow or a failed infallible allocation ends
//! the process, so only the exit status can show that one did not happen. On Windows with Python on
//! the path, that process runs under `oracle/vortex/nc_job.py` with a 128 MiB commit limit, as the
//! re-check's did. With neither, it runs with no limit, which still catches the overflow.

mod nc_probes;
mod nc_raw;

use nc_raw::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MSG_DATATYPE: u16 = 0x03;
const MSG_FILL_OLD: u16 = 0x04;
const MSG_FILL: u16 = 0x05;
const MSG_FILTERS: u16 = 0x0B;
const DECLARED: [(u16, &str); 4] = [
    (MSG_DATATYPE, "datatype"),
    (MSG_FILL_OLD, "fill_old"),
    (MSG_FILL, "fill"),
    (MSG_FILTERS, "filter"),
];

fn hostile_fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public/netcdf_hostile")
            .join(name),
    )
    .unwrap()
}

/// `target/<profile>/examples/nc_dump`, which `cargo test` builds. Built here if this test ran alone.
fn nc_dump() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    let profile = test.parent().unwrap().parent().unwrap();
    let tool = profile
        .join("examples")
        .join(format!("nc_dump{}", std::env::consts::EXE_SUFFIX));
    if !tool.exists() {
        let release = profile.file_name().is_some_and(|name| name == "release");
        let mut build = Command::new(env!("CARGO"));
        build.args([
            "build",
            "--locked",
            "-p",
            "dss-vortex",
            "--example",
            "nc_dump",
        ]);
        if release {
            build.arg("--release");
        }
        assert!(build.status().unwrap().success());
    }
    tool
}

/// How a probe's process is run.
#[derive(PartialEq)]
enum Limiter {
    /// `oracle/vortex/nc_job.py`: a Windows Job Object's process commit limit. The reader's typed
    /// refusals at a limit are qualified against this one.
    WindowsJob,
    /// No memory limit, and on Linux a wall-clock limit from `timeout`. A Linux address-space
    /// limit (`ulimit -v`) is not used: it counts what a commit limit does not, and glibc can leave
    /// a probe of headroom passing while the next small allocation fails, so it would hold the
    /// reader to a limit it is not qualified against.
    Unlimited,
}

fn limiter() -> Limiter {
    let limited = cfg!(windows)
        && Command::new("python")
            .args(["-I", "-c", "pass"])
            .status()
            .is_ok_and(|status| status.success());
    if limited {
        Limiter::WindowsJob
    } else {
        Limiter::Unlimited
    }
}

/// Seconds a probe may run on Linux before `timeout` ends it, so a hang fails by name.
const PROBE_SECONDS: u32 = 300;

/// Run every probe in a process of its own. Each must exit 0, and its dump must hold `expected`.
fn run_probes(set: &str, probes: Vec<(String, Vec<u8>, &str)>) {
    run_probes_at(set, probes, 128)
}

/// For probes that, with no limit, really allocate the gigabytes they ask for, or whose typed error
/// is the limit's. They run only under the Windows Job Object, and are skipped elsewhere rather than
/// left to take the machine's memory, which is what hung the Linux runner.
fn run_limited_probes_at(set: &str, probes: Vec<(String, Vec<u8>, &str)>, limit_mib: u32) {
    if limiter() != Limiter::WindowsJob {
        // Windows CI is where these run. A runner that lost Python would otherwise pass them empty.
        assert!(
            !(cfg!(windows) && std::env::var_os("CI").is_some()),
            "{set}: Windows CI needs `python` on the path for the Job Object"
        );
        eprintln!("{set}: skipped, it needs the Windows Job Object's memory limit");
        return;
    }
    run_probes_at(set, probes, limit_mib)
}

fn run_probes_at(set: &str, probes: Vec<(String, Vec<u8>, &str)>, limit_mib: u32) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("nc_hostile_job_{set}"));
    let _ = fs::remove_dir_all(&root);
    let (input, out) = (root.join("in"), root.join("out"));
    fs::create_dir_all(&input).unwrap();
    fs::create_dir_all(&out).unwrap();
    for (name, bytes, _) in &probes {
        fs::write(input.join(name), bytes).unwrap();
    }
    let tool = nc_dump();
    let job = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/nc_job.py");
    let limiter = limiter();
    if limiter == Limiter::WindowsJob {
        let output = Command::new("python")
            .arg("-I")
            .arg(&job)
            .arg(&tool)
            .arg(&input)
            .arg(&out)
            .arg(limit_mib.to_string())
            // `nc_dump` over every step of a long record runs its own JSON out of memory.
            .args(["--max-steps", "4"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{text}");
        assert!(output.status.success(), "{set}: {text}");
    } else {
        for (name, _, _) in &probes {
            let has_timeout = cfg!(target_os = "linux")
                && Command::new("timeout")
                    .args(["5", "true"])
                    .status()
                    .is_ok_and(|status| status.success());
            let mut command = if has_timeout {
                let mut timeout = Command::new("timeout");
                timeout.arg(PROBE_SECONDS.to_string()).arg(&tool);
                timeout
            } else {
                Command::new(&tool)
            };
            let status = command
                .arg(input.join(name))
                .arg(out.join(name))
                .args(["--max-steps", "4"])
                .status()
                .unwrap();
            // `timeout` exits 124 when it ends the probe.
            assert!(status.success(), "{set}/{name}: {status}");
        }
    }
    for (name, _, expected) in &probes {
        let dump = fs::read_to_string(out.join(name).join("dtos.json"))
            .unwrap_or_else(|e| panic!("{name}: no dump: {e}"));
        assert!(dump.contains(expected), "{name}: wanted {expected}: {dump}");
    }
}

fn probe(
    name: impl Into<String>,
    bytes: Vec<u8>,
    expected: &'static str,
) -> (String, Vec<u8>, &'static str) {
    (name.into(), bytes, expected)
}

#[test]
fn messages_behind_a_shared_message_are_refused() {
    let without = "without one";
    let mut probes = Vec::new();
    for (declared, label) in DECLARED {
        probes.push(probe(
            format!("link_{label}.nc4"),
            shared_hides_cycle(declared),
            without,
        ));
        probes.push(probe(
            format!("symbol_table_{label}.nc4"),
            shared_hides_symbol_table(declared),
            without,
        ));
        let attribute_message = message(0x0C, 0, &attribute("zz", &int32_type(), &[1], &[0; 4]));
        probes.push(probe(
            format!("attribute_{label}.nc4"),
            shared_hides(declared, attribute_message),
            without,
        ));
        let mut info = vec![0, 0];
        info.extend_from_slice(&UNDEFINED.to_le_bytes());
        info.extend_from_slice(&UNDEFINED.to_le_bytes());
        probes.push(probe(
            format!("attribute_info_{label}.nc4"),
            shared_hides(declared, message(0x15, 0, &info)),
            without,
        ));
        probes.push(probe(
            format!("dataspace_{label}.nc4"),
            dataset_hiding(
                vec![datatype_message(), contiguous_layout_message(UNDEFINED, 0)],
                declared,
                dataspace_message(&[4, 4]),
            ),
            without,
        ));
        probes.push(probe(
            format!("layout_{label}.nc4"),
            dataset_hiding(
                vec![datatype_message(), dataspace_message(&[1024, 1024])],
                declared,
                chunked_layout_message(0, &[1, 1, 4]),
            ),
            without,
        ));
    }
    assert_eq!(shared_hides_cycle(MSG_DATATYPE).len(), 176);
    run_probes("shared", probes);
}

#[test]
fn a_hidden_layout_cannot_escape_the_chunk_limit() {
    // `page_offsets` makes a table of this many pages. Its only check is that the page bitmap is in
    // the file, which the padding satisfies: 350 million pages is a request for 5.6 GB.
    let mut false_count = hostile_fixture("hidden_layout_fixed_array.nc4");
    let at = false_count.windows(4).position(|w| w == b"FAHD").unwrap();
    false_count[at + 7] = 1;
    false_count[at + 8..at + 16].copy_from_slice(&700_000_000u64.to_le_bytes());
    let sum = hdf5_reader::checksum::jenkins_lookup3(&false_count[at..at + 24]);
    false_count[at + 24..at + 28].copy_from_slice(&sum.to_le_bytes());
    false_count.resize(48_000_016, 0);
    let mut probes: Vec<_> = ["btree", "fixed_array", "extensible_array"]
        .iter()
        .map(|index| {
            let name = format!("hidden_layout_{index}.nc4");
            let bytes = hostile_fixture(&name);
            probe(name, bytes, "without one")
        })
        .collect();
    probes.push(probe(
        "hidden_fixed_array_pages.nc4",
        false_count,
        "without one",
    ));
    run_probes("layout", probes);
}

#[test]
fn a_header_the_reader_could_read_two_ways_is_refused() {
    let plain = || vec![datatype_message(), contiguous_layout_message(UNDEFINED, 0)];
    let mut twice_space = plain();
    twice_space.push(dataspace_message(&[4, 4]));
    twice_space.push(dataspace_message(&[1 << 20, 1 << 20]));
    let mut twice_layout = plain();
    twice_layout.push(dataspace_message(&[1024, 1024]));
    twice_layout.push(chunked_layout_message(0, &[1, 1, 4]));
    let mut both = plain();
    both.push(dataspace_message(&[4, 4]));
    both.push(link_message("loop", 96));
    let mut rank = vec![datatype_message(), dataspace_message(&[4, 4])];
    rank.push(chunked_layout_message(0, &[1, 1, 1, 1, 4]));
    let mut szip = plain();
    szip.push(dataspace_message(&[4, 4]));
    szip.push(filters_message(&[4, 1]));
    let mut unknown = plain();
    unknown.push(dataspace_message(&[4, 4]));
    unknown.push(filters_message(&[32000, 1]));
    run_probes(
        "header",
        vec![
            probe(
                "twice_space.nc4",
                object_with(twice_space),
                "more than one dataspace",
            ),
            probe(
                "twice_layout.nc4",
                object_with(twice_layout),
                "more than one data layout",
            ),
            probe(
                "group_and_dataset.nc4",
                object_with(both),
                "both a group and a dataset",
            ),
            probe("rank.nc4", object_with(rank), "has chunks of shape"),
            probe("szip_first.nc4", object_with(szip), "no limit"),
            probe("unknown_first.nc4", object_with(unknown), "no limit"),
        ],
    );
}

#[test]
fn user_defined_attributes_are_counted_field_by_element() {
    // The re-check's 8.9 GiB from 60 KB, and its neighbours.
    let overlap = "overlapping members";
    let member = int8_type();
    // An array of no elements has no width, and `decode_value` would reserve 2^31 values of it.
    let zero_width = array_type(0, &[0], &member);
    let array_of_nothing = array_type(0, &[1 << 31], &zero_width);
    let outer = compound_type(1, &[("a", 0, array_of_nothing), ("b", 0, member.clone())]);
    run_probes(
        "compound",
        vec![
            probe(
                "overlap_1000_3000.nc4",
                overlapping_compound(1000, 3000),
                overlap,
            ),
            probe(
                "overlap_1000_10000.nc4",
                overlapping_compound(1000, 10_000),
                overlap,
            ),
            probe(
                "overlap_2142_30000.nc4",
                overlapping_compound(2142, 30_000),
                overlap,
            ),
            // 60 KB of members that do not overlap costs what it says, and stays under the limit.
            probe(
                "plain_100_100.nc4",
                plain_compound(100, 100),
                "\"variables\":[]",
            ),
            // Each attribute is within reach of the limit. Twelve of them are not, and the sum is held.
            probe(
                "plain_12_attributes.nc4",
                plain_compounds(12, 500, 60),
                "metadata needing",
            ),
            probe(
                "array_of_nothing.nc4",
                group_attribute(&outer, 1, 1),
                "zero-sized dimension",
            ),
        ],
    );
}

#[test]
fn a_read_is_decided_with_the_lengths_the_reader_reports() {
    // Its own shape says one chunk. The dimensions it is padded to say 2^26.
    run_probes(
        "padded",
        vec![probe(
            "padded_chunks.nc4",
            hostile_fixture("padded_chunks.nc4"),
            "over the limit of 4194304",
        )],
    );
}

#[test]
fn a_read_too_large_for_the_process_is_refused_quickly() {
    // 354 KB. One step decodes 32 GiB, under the limit, and it does not fit in 128 MiB.
    let started = std::time::Instant::now();
    run_limited_probes_at(
        "cpu",
        vec![probe(
            "double_deflate_8x8.nc4",
            hostile_fixture("double_deflate_8x8.nc4"),
            "could not allocate",
        )],
        128,
    );
    assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
}

#[test]
fn a_file_opened_by_path_costs_only_what_is_read() {
    // The file is padded to several times the process's limit. A step takes one small chunk, so
    // this would not open if the file were held in memory.
    let mut padded = hostile_fixture("real_shared_and_user_types.nc4");
    padded.resize(400 << 20, 0);
    run_probes(
        "padded_file",
        vec![probe("padded_file.nc4", padded, "\"dto_count\":1")],
    );
}

#[test]
fn a_chunk_listed_a_million_times_is_a_typed_error() {
    let source = hostile_fixture("chunk_index_source.nc4");
    run_probes(
        "btree",
        vec![
            probe(
                "btree_dup_1000000.nc4",
                duplicate_chunk_entries(&source, 1_000_000),
                "\"error\"",
            ),
            probe(
                "btree_dup_2.nc4",
                duplicate_chunk_entries(&source, 2),
                "duplicate chunk",
            ),
            // The negative control: the same file with its one entry reads.
            probe(
                "btree_one.nc4",
                duplicate_chunk_entries(&source, 1),
                "\"dto_count\":1",
            ),
        ],
    );
}

#[test]
fn a_version_2_index_is_held_to_the_same_rule() {
    let source = hostile_fixture("chunk_index_source_v2.nc4");
    run_probes(
        "btree2",
        vec![
            probe(
                "btree2_dup_1000000.nc4",
                duplicate_chunk_entries_v2(&source, 1_000_000),
                "\"error\"",
            ),
            probe(
                "btree2_dup_120000.nc4",
                duplicate_chunk_entries_v2(&source, 120_000),
                "duplicate chunk",
            ),
            // The file as written, with its index unchanged.
            probe("btree2_source.nc4", source, "\"dto_count\":1"),
        ],
    );
}

/// Everything the third re-check's generators built that ended the process, under its names. A
/// typed result is the requirement, so `"variables"`, which any dump holds, is what is asked of the
/// file: the exit status is the test.
fn third_recheck_probes() -> Vec<(String, Vec<u8>, &'static str)> {
    let v1 = hostile_fixture("chunk_index_source.nc4");
    let v2 = hostile_fixture("chunk_index_source_v2.nc4");
    let shuffled = hostile_fixture("real_shared_and_user_types.nc4");
    let any = "\"variables\"";
    let mut probes = vec![
        probe(
            "gheap_dup_16k_4000.nc4",
            nc_probes::repeated_heap_object(16 << 10, 4000),
            any,
        ),
        probe(
            "gheap_dup_1m_4000.nc4",
            nc_probes::repeated_heap_object(1 << 20, 4000),
            any,
        ),
        probe("v1_chain_24.nc4", nc_probes::tree_chain(&v1, 24), any),
        probe("v1_type0_32.nc4", nc_probes::type0_index(&v1, 32), any),
        probe("v1_type0_64.nc4", nc_probes::type0_index(&v1, 64), any),
        probe("group_leaves_32.nc4", nc_probes::group_leaves(32), any),
        probe(
            "oversize_plain_64m.nc4",
            nc_probes::oversized_chunk(&v1, 64 << 20),
            "decoded to 67108864 bytes",
        ),
        probe(
            "oversize_filtered_64m.nc4",
            nc_probes::oversized_chunk(&shuffled, 64 << 20),
            any,
        ),
        probe(
            "v2_dup_pad7_1m.nc4",
            nc_probes::v2_records(&v2, 1_000_000, 7, None, false),
            any,
        ),
        probe(
            "v2_unknown_type_2m.nc4",
            nc_probes::v2_records(&v2, 2_000_000, 0, Some(200), true),
            any,
        ),
        probe(
            "v2_unknown_type_8m.nc4",
            nc_probes::v2_records(&v2, 8_000_000, 0, Some(200), true),
            any,
        ),
    ];
    for mib in [5, 16, 32, 60] {
        probes.push(probe(
            format!("nil_{mib}m.nc4"),
            nc_probes::nil_header(mib << 20),
            any,
        ));
    }
    probes
}

#[test]
fn what_the_third_re_check_built_ends_in_a_result_or_a_typed_error() {
    run_limited_probes_at("recheck3", third_recheck_probes(), 128);
}

#[test]
fn the_fourth_re_checks_large_copies_are_typed_errors() {
    // Each was one infallible allocation of tens of megabytes at the size the limit left. A valid
    // file with 3.75 million time steps, a 1-minute record for 7 years, is the first.
    let times: Vec<f64> = (0..3_750_000).map(f64::from).collect();
    let axis = classic_time_file(b"minutes since 2000-01-01", None, &times);
    let heap = hostile_fixture("dense_attributes.nc4");
    for limit in [64, 96, 128] {
        run_limited_probes_at(
            "recheck4",
            vec![
                probe("axis_3m.nc", axis.clone(), "\"variables\""),
                probe(
                    "bigblock_32m.nc4",
                    nc_probes::big_heap_block(&heap, 32 << 20),
                    "\"variables\"",
                ),
                probe(
                    "bigblock_48m.nc4",
                    nc_probes::big_heap_block(&heap, 48 << 20),
                    "\"variables\"",
                ),
            ],
            limit,
        );
    }
}

#[test]
fn what_real_writers_do_with_these_messages_keeps_reading() {
    run_probes(
        "real",
        vec![
            probe(
                "real_shared_and_user_types.nc4",
                hostile_fixture("real_shared_and_user_types.nc4"),
                "\"dto_count\":1",
            ),
            probe(
                "chunk_index_source.nc4",
                hostile_fixture("chunk_index_source.nc4"),
                "\"dto_count\":1",
            ),
            probe(
                "fixed_array.nc4",
                hostile_fixture("fixed_array.nc4"),
                "\"dto_count\":1",
            ),
        ],
    );
}

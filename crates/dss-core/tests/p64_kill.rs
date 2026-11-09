//! Killed-process qualification of both flush policies (`WriteOptions { sync }`), and of
//! the repair of finding F1 (`docs/format/dss7.md` §6.4).
//!
//! The crash model of `docs/format/dss7.md` and `dss6.md` is process death: the operating
//! system keeps every completed `write`. The existing fault matrices (`dss7_write.rs`, `dss7_write_irr.rs`, `pd_write.rs`,
//! `grid_write.rs`, `ts_variants.rs`, `pl_write.rs`)
//! model it by returning an injected error after a step. This test kills a real child
//! process instead. The child installs the hidden barrier hook
//! (`dss7::write::set_barrier_hook`), runs one write and blocks at the n-th ordering
//! boundary: after each `WriteStep`, and at each flush point inside a step (such as
//! between an entry body and its publication word). The parent then kills it
//! (`TerminateProcess` on Windows, `SIGKILL` on Unix) with the file open and the write
//! lock held, for every n until the write completes.
//!
//! After each kill the parent checks, as separate phases so that one failing check never
//! hides another:
//!
//! 1. **killed / payload**: every record that existed before the write, looked up by its
//!    known pathname (not through the catalog), reads exactly as before; each record the
//!    write touches reads as its old or its new version, nothing else;
//! 2. **killed / structure** (DSS6): `check_file6` reports only the documented
//!    interrupted-write windows;
//! 3. the next native write (the default, durable mode) reconciles and completes the
//!    write. Then, separately:
//!    - **repaired / structure**: the checked catalog has no anomalies, `knumberRecords`
//!      equals the walked count, every listed record is reachable, and the catalog lists
//!      exactly the records of the reference (DSS6: `check_file6` is clean);
//!    - **repaired / payload**: every known pathname reads as in a reference file that
//!      the same write produced without interruption in the default mode;
//! 4. for a subset of cases, a second kill inside that repairing write (the child again
//!    in the no-flush mode), followed by a clean write, passes the same two checks.
//!
//! No failure is allowed anywhere (finding F1 is fixed, so there is no allowance for it).
//!
//! `DSS_P64_KILL_OUT=<dir>` keeps the killed, repaired and second-kill repaired files and
//! the uninterrupted references, so they can also be read with HEC software.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use dss_core::dss6::write::{
    check_file6, create_dss6, write_grid6, write_its6, write_pd6, write_rts6,
    ItsInput as ItsInput6, ItsMode,
};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, repair_dss7, set_barrier_hook, write_grid, write_its, write_location,
    write_pattern, write_pd, write_profile, write_rts, write_rts_variants, AlbersParameters,
    FaultPoint, GridCompression, GridInput, GridProjection, ItsInput, LocationInput, PatternInput,
    PdArrays, PdInput, ProfileArrays, ProfileInput, RtsInput, RtsValues, TsVariants, WriteError,
    WriteOptions, WriteStep,
};
use dss_core::dss7::{table_hash, ChainState, Dss7File};
use dss_core::time::ymd_to_julian;

const DAY: i64 = 86_400;

fn t(y: i64, m: u32, d: u32, secs: i64) -> i64 {
    ymd_to_julian(y, m, d) * DAY + secs
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p64_kill_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn mode(sync: bool) -> WriteOptions {
    WriteOptions {
        sync,
        ..WriteOptions::default()
    }
}

// ---------------------------------------------------------------------------------
// Cases: a base file, and the write that is interrupted.
// ---------------------------------------------------------------------------------

/// The original cases, then the additions: a new record of every DSS7 record kind, a new
/// bin in the last bin of a bin block and in a new bin block, a bin overflow, the reuse
/// of a deleted entry, and a write that deletes a record.
const CASES: [&str; 25] = [
    "rts7_new",
    "rts7_multi",
    "its7",
    "pd7",
    "grid7",
    "rts6_multi",
    "its6",
    "pd6",
    "grid6",
    "its7_new",
    "pd7_new",
    "profile7_new",
    "location7_new",
    "variants7_new",
    "pattern7_new",
    "fill7_last",
    "fill7_block",
    "overflow7",
    "reuse7",
    "delete7",
    // R17 (dss7.md §6.4): replacements that change the stored width and record type.
    "rts7_widen",
    "its7_widen",
    "pd7_widen",
    "rts6_widen",
    "pd6_widen",
];

/// DSS7 cases whose write takes a new pathname bin (so the F1 window exists).
const NEW_BIN_CASES: [&str; 12] = [
    "rts7_new",
    "rts7_multi",
    "grid7",
    "its7_new",
    "pd7_new",
    "profile7_new",
    "location7_new",
    "variants7_new",
    "pattern7_new",
    "fill7_last",
    "fill7_block",
    "overflow7",
];

/// The matrix cases; `P64_CASES=a,b` restricts them (diagnostic runs only: the F1
/// coverage assertion then fails unless every new-bin case is included).
fn cases() -> Vec<&'static str> {
    match std::env::var("P64_CASES") {
        Ok(v) => CASES
            .iter()
            .copied()
            .filter(|c| v.split(',').any(|x| x == *c))
            .collect(),
        Err(_) => CASES.to_vec(),
    }
}

fn version(case: &str) -> u8 {
    if case.contains('6') {
        6
    } else {
        7
    }
}

fn hourly(
    v: u8,
    f: &Path,
    p: &[u8],
    start: i64,
    vals: &[f32],
    o: &WriteOptions,
) -> Result<(), WriteError> {
    let input = RtsInput::new(p, start, RtsValues::F32(vals), b"CFS", b"INST-VAL");
    if v == 6 {
        write_rts6(f, &input, o).map(|_| ())
    } else {
        write_rts(f, &input, o).map(|_| ())
    }
}

fn irregular(
    v: u8,
    f: &Path,
    p: &[u8],
    times: &[i64],
    vals: &[f32],
    o: &WriteOptions,
) -> Result<(), WriteError> {
    if v == 6 {
        let input = ItsInput6 {
            pathname: p,
            times,
            values: RtsValues::F32(vals),
            quality: None,
            units: b"FT",
            data_type: b"INST-VAL",
            mode: ItsMode::Replace,
        };
        write_its6(f, &input, o).map(|_| ())
    } else {
        let input = ItsInput::new(p, times, RtsValues::F32(vals), b"FT", b"INST-VAL");
        write_its(f, &input, o).map(|_| ())
    }
}

fn paired(v: u8, f: &Path, p: &[u8], scale: f32, o: &WriteOptions) -> Result<(), WriteError> {
    let ord = [0.0f32, 1.0, 2.5, 4.0, 7.0];
    let vals: Vec<f32> = (0..10).map(|i| scale * (i as f32 + 0.25)).collect();
    let input = PdInput::new(
        p,
        PdArrays::F32 {
            ordinates: &ord,
            values: &vals,
        },
        2,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    if v == 6 {
        write_pd6(f, &input, o).map(|_| ())
    } else {
        write_pd(f, &input, o).map(|_| ())
    }
}

const WIDE_RTS: &[u8] = b"/T/WIDTH/FLOW//1HOUR/S/";
const WIDE_ITS: &[u8] = b"/T/WIDEIRR/STAGE//IR-MONTH/S/";
const WIDE_PD: &[u8] = b"/T/WIDEPD/STAGE-FLOW///S/";

/// R17 (dss7.md §6.4): an F64 write over an F32 record (type 100 -> 105, 110 -> 115,
/// 200 -> 205): the replacement changes the entry's record type.
fn widen(case: &str, f: &Path, o: &WriteOptions) -> Result<(), WriteError> {
    let v = version(case);
    if case.starts_with("rts") {
        let vals = [11.0f64, 22.0, 33.0];
        let input = RtsInput::new(
            WIDE_RTS,
            t(2021, 3, 1, 3600),
            RtsValues::F64(&vals),
            b"CFS",
            b"INST-VAL",
        );
        return if v == 6 {
            write_rts6(f, &input, o).map(|_| ())
        } else {
            write_rts(f, &input, o).map(|_| ())
        };
    }
    if case.starts_with("its") {
        let times = [t(2021, 5, 2, 0), t(2021, 5, 3, 0)];
        let vals = [7.25f64, 8.75];
        let input = ItsInput::new(WIDE_ITS, &times, RtsValues::F64(&vals), b"FT", b"INST-VAL");
        return write_its(f, &input, o).map(|_| ());
    }
    let ord = [0.0f64, 1.0, 2.5, 4.0, 7.0];
    let vals: Vec<f64> = (0..10).map(|i| 3.0 * (f64::from(i) + 0.125)).collect();
    let input = PdInput::new(
        WIDE_PD,
        PdArrays::F64 {
            ordinates: &ord,
            values: &vals,
        },
        2,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    if v == 6 {
        write_pd6(f, &input, o).map(|_| ())
    } else {
        write_pd(f, &input, o).map(|_| ())
    }
}

fn grid(v: u8, f: &Path, p: &[u8], seed: u32, o: &WriteOptions) -> Result<(), WriteError> {
    let data: Vec<f32> = (0..30u32)
        .map(|i| ((i * 7 + seed * 13) % 11) as f32 * 0.5)
        .collect();
    let input = GridInput {
        pathname: p,
        projection: GridProjection::Albers(AlbersParameters::shg()),
        time_stamped: true,
        data: &data,
        nx: 6,
        ny: 5,
        lower_left_cell_x: -1000,
        lower_left_cell_y: 700,
        cell_size: 2000.0,
        x_coord_of_grid_cell_zero: 0.0,
        y_coord_of_grid_cell_zero: 0.0,
        units: b"MM",
        data_type: 1,
        compression: GridCompression::Zlib,
    };
    if v == 6 {
        write_grid6(f, &input, o).map(|_| ())
    } else {
        write_grid(f, &input, o).map(|_| ())
    }
}

fn ramp(n: usize, a: f32) -> Vec<f32> {
    (0..n).map(|i| a + i as f32 * 0.5).collect()
}

fn profile(f: &Path, p: &[u8], o: &WriteOptions) -> Result<(), WriteError> {
    let depths = [0.5f64, 1.5, 3.0];
    let values: Vec<f64> = (0..12).map(|i| 10.0 + i as f64 * 0.25).collect();
    let input = ProfileInput::new(
        p,
        t(2021, 5, 1, 3600),
        ProfileArrays::F64 {
            depths: &depths,
            values: &values,
        },
        b"m",
        b"deg C",
        b"INST-VAL",
    );
    write_profile(f, &input, o).map(|_| ())
}

fn location(f: &Path, p: &[u8], o: &WriteOptions) -> Result<(), WriteError> {
    let input = LocationInput {
        pathname: p,
        x: Some(-97.25),
        y: Some(30.5),
        z: Some(612.5),
        coordinate_system: Some(1),
        coordinate_id: Some(0),
        horizontal_units: Some(3),
        horizontal_datum: Some(1),
        vertical_units: Some(1),
        vertical_datum: Some(2),
        time_zone: Some(b"America/Chicago"),
        supplemental: Some(b"Key:Value;"),
        keep_existing: false,
    };
    write_location(f, &input, o).map(|_| ())
}

fn variants(f: &Path, p: &[u8], o: &WriteOptions) -> Result<(), WriteError> {
    let v = ramp(24, 3.0);
    let q: Vec<i32> = (0..24).map(|i| i * 3 + 1).collect();
    let var = TsVariants {
        quality_words: Some((&q, 1)),
        notes: None,
    };
    let input = RtsInput::new(
        p,
        t(2021, 7, 1, 3600),
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    write_rts_variants(f, &input, &var, o).map(|_| ())
}

fn pattern(f: &Path, p: &[u8], o: &WriteOptions) -> Result<(), WriteError> {
    let v: Vec<f32> = (0..12).map(|i| i as f32 / 2.0).collect();
    let input = PatternInput::new(p, RtsValues::F32(&v), b"CFS", b"PER-AVER");
    write_pattern(f, &input, o).map(|_| ())
}

/// Header word `i` of a DSS7 file.
fn hword(f: &Path, i: usize) -> i64 {
    Dss7File::open(f).unwrap().header().words[i]
}

const BINS_REMAIN_IN_BLOCK: usize = 22;
const NEXT_EMPTY_BIN: usize = 25;
const TOTAL_BINS: usize = 26;
const PENDING_WORD: usize = 29;

const GAP: &[u8] = b"/T/GAP/STAGE//IR-MONTH/S/";

/// `n` pd pathnames of equal length whose table hash is the same (a bin overflow).
fn colliding_paths(max_hash: i64, n: usize) -> Vec<Vec<u8>> {
    let mut groups: BTreeMap<i64, Vec<Vec<u8>>> = BTreeMap::new();
    for i in 0..2_000_000u32 {
        let p = format!("/O/B{i:07}/STAGE-FLOW///S/").into_bytes();
        let g = groups.entry(table_hash(&p, max_hash)).or_default();
        g.push(p);
        if g.len() == n {
            return g.clone();
        }
    }
    panic!("no table hash with {n} pathnames");
}

/// How many entries of `pw` pathname words fit in one bin of `bs` words before a further
/// one overflows (`zbinUpdate()`; the writer's `insertion_target()`).
fn bin_capacity(bs: usize, pw: usize) -> usize {
    let mut ic = 0;
    let mut n = 0;
    while ic + 7 + pw + 1 < bs {
        n += 1;
        ic += 7 + pw;
    }
    n
}

fn overflow_paths() -> Vec<Vec<u8>> {
    let p0 = b"/O/B0000000/STAGE-FLOW///S/";
    let cap = bin_capacity(200, p0.len().div_ceil(8));
    colliding_paths(8192, cap + 1)
}

/// The base file of `case`, written in the default mode.
fn make_base(case: &str, f: &Path) {
    let v = version(case);
    let o = WriteOptions::default();
    if v == 6 {
        create_dss6(f).unwrap();
    } else {
        create_dss7(f).unwrap();
    }
    // Records every case keeps.
    hourly(
        v,
        f,
        b"/K/KEEP/FLOW//1HOUR/S/",
        t(2021, 1, 1, 3600),
        &ramp(72, 1.0),
        &o,
    )
    .unwrap();
    hourly(
        v,
        f,
        b"/K/DAILY/FLOW//1DAY/S/",
        t(2021, 1, 2, 0),
        &ramp(5, 9.0),
        &o,
    )
    .unwrap();
    irregular(
        v,
        f,
        b"/K/IRR/STAGE//IR-MONTH/S/",
        &[t(2021, 2, 1, 1800), t(2021, 2, 3, 7200), t(2021, 2, 9, 0)],
        &[4.5, 5.25, 6.0],
        &o,
    )
    .unwrap();
    paired(v, f, b"/K/PD/STAGE-FLOW///S/", 1.0, &o).unwrap();
    grid(
        v,
        f,
        b"/SHG/K/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
        1,
        &o,
    )
    .unwrap();
    // R17: the F32 records that the widen cases replace with F64.
    if case.contains("widen") {
        hourly(v, f, WIDE_RTS, t(2021, 3, 1, 3600), &[1.0, 2.0, 3.0], &o).unwrap();
        if v == 7 {
            irregular(
                v,
                f,
                WIDE_ITS,
                &[t(2021, 5, 2, 0), t(2021, 5, 3, 0)],
                &[1.5, 2.5],
                &o,
            )
            .unwrap();
        }
        paired(v, f, WIDE_PD, 1.5, &o).unwrap();
    }
    // Records some cases rewrite.
    hourly(
        v,
        f,
        b"/T/MULTI/FLOW//1HOUR/S/",
        t(2021, 3, 20, 3600),
        &ramp(48, 100.0),
        &o,
    )
    .unwrap();
    irregular(
        v,
        f,
        b"/T/IRR/STAGE//IR-MONTH/S/",
        &[
            t(2021, 4, 2, 0),
            t(2021, 4, 5, 0),
            t(2021, 4, 8, 0),
            t(2021, 4, 20, 0),
        ],
        &[1.0, 2.0, 3.0, 4.0],
        &o,
    )
    .unwrap();
    paired(v, f, b"/T/PD/STAGE-FLOW///S/", 2.0, &o).unwrap();
    grid(
        v,
        f,
        b"/SHG/T/PRECIP/02SEP2018:0100/02SEP2018:0200/S/",
        2,
        &o,
    )
    .unwrap();
    // Case extras.
    let fast = mode(false);
    match case {
        "fill7_last" | "fill7_block" => {
            // New-bin records until the cursor stands at the block's last bin
            // (`fill7_last`) or past it (`fill7_block`: the next bin starts a block).
            let want = if case == "fill7_last" { 1 } else { 0 };
            let mut n = 0;
            while hword(f, BINS_REMAIN_IN_BLOCK) != want {
                assert!(n < 400, "{case}: bin block never filled");
                let p = format!("/F/FILL{n:04}/STAGE-FLOW///S/");
                paired(7, f, p.as_bytes(), n as f32, &fast).unwrap();
                n += 1;
            }
        }
        "overflow7" => {
            // A full bin of one table hash: the case's write overflows it.
            let ps = overflow_paths();
            for (i, p) in ps[..ps.len() - 1].iter().enumerate() {
                paired(7, f, p, i as f32 + 0.5, &fast).unwrap();
            }
        }
        "reuse7" | "delete7" => {
            irregular(
                7,
                f,
                GAP,
                &[t(2021, 2, 10, 0), t(2021, 3, 10, 0), t(2021, 4, 10, 0)],
                &[1.0, 2.0, 3.0],
                &fast,
            )
            .unwrap();
            if case == "reuse7" {
                // The March record is deleted (its block lies inside the window and gets
                // no value); the case's write stores it again in the deleted entry.
                gap_write(f, &fast).unwrap();
            }
        }
        _ => {}
    }
}

/// Replaces February to April of [`GAP`] with values in February and April only, which
/// deletes the March record (`DeleteGap`, the writer's `delete()`).
fn gap_write(f: &Path, o: &WriteOptions) -> Result<(), WriteError> {
    irregular(
        7,
        f,
        GAP,
        &[t(2021, 2, 5, 0), t(2021, 4, 25, 0)],
        &[11.0, 33.0],
        o,
    )
}

/// The write that `case` interrupts.
fn run_write(case: &str, f: &Path, o: &WriteOptions) -> Result<(), WriteError> {
    let v = version(case);
    match case {
        "rts7_new" => hourly(
            v,
            f,
            b"/T/NEW/FLOW//1HOUR/S/",
            t(2021, 3, 1, 3600),
            &ramp(30, 10.0),
            o,
        ),
        // 40 days from 20 March: merges into the March block and adds the April block.
        "rts7_multi" | "rts6_multi" => hourly(
            v,
            f,
            b"/T/MULTI/FLOW//1HOUR/S/",
            t(2021, 3, 21, 3600),
            &ramp(24 * 40, 500.0),
            o,
        ),
        // Replaces the values inside the window (and deletes one).
        "its7" | "its6" => irregular(
            v,
            f,
            b"/T/IRR/STAGE//IR-MONTH/S/",
            &[t(2021, 4, 4, 0), t(2021, 4, 6, 3600)],
            &[7.5, 8.5],
            o,
        ),
        "pd7" | "pd6" => paired(v, f, b"/T/PD/STAGE-FLOW///S/", 3.0, o),
        // One replaced grid (DSS7 and DSS6 each also add one in the second write below).
        "grid7" | "grid6" => {
            grid(
                v,
                f,
                b"/SHG/T/PRECIP/02SEP2018:0100/02SEP2018:0200/S/",
                9,
                o,
            )?;
            grid(
                v,
                f,
                b"/SHG/T/PRECIP/03SEP2018:0100/03SEP2018:0200/S/",
                4,
                o,
            )
        }
        "its7_new" => irregular(
            7,
            f,
            b"/T/NEWIRR/STAGE//IR-MONTH/S/",
            &[t(2021, 6, 2, 60), t(2021, 6, 4, 0)],
            &[2.5, 3.5],
            o,
        ),
        "pd7_new" | "fill7_last" | "fill7_block" => {
            paired(7, f, b"/T/NEWPD/STAGE-FLOW///S/", 4.0, o)
        }
        "profile7_new" => profile(f, b"/T/PROF/TEMP-WATER//1HOUR/S/", o),
        "location7_new" => location(f, b"/T/LOC/STAGE//1HOUR/S/", o),
        "variants7_new" => variants(f, b"/T/QUAL/FLOW//1HOUR/S/", o),
        "pattern7_new" => pattern(f, b"/T/PAT/FLOW/TS-PATTERN/1MON/S/", o),
        "overflow7" => {
            let ps = overflow_paths();
            paired(7, f, ps.last().unwrap(), 99.0, o)
        }
        "reuse7" => irregular(7, f, GAP, &[t(2021, 3, 15, 0)], &[22.0], o),
        "delete7" => gap_write(f, o),
        "rts7_widen" | "its7_widen" | "pd7_widen" | "rts6_widen" | "pd6_widen" => widen(case, f, o),
        // The copy-out repair, killed like a write (it writes only `dest`).
        "repair7" => {
            let _ = o;
            repair_dss7(f, &copy_dest(f)).map(|_| ())
        }
        // A write that reconciles an interrupted write in place.
        "reconcile7" => paired(7, f, b"/Z/RECON/STAGE-FLOW///S/", 6.0, o),
        _ => panic!("unknown case {case}"),
    }
}

// ---------------------------------------------------------------------------------
// Snapshots: records decoded by their known pathnames, independent of the catalog.
// ---------------------------------------------------------------------------------

type Snap = BTreeMap<Vec<u8>, String>;
/// Known pathnames (with D parts) and their record types.
type Known = BTreeMap<Vec<u8>, i32>;

/// The live records the catalog of a clean file lists.
fn listed(v: u8, f: &Path) -> Known {
    if v == 6 {
        let file = Dss6File::open(f).unwrap();
        let cat = file.catalog().unwrap();
        cat.live()
            .map(|e| (e.pathname.clone(), e.record_type))
            .collect()
    } else {
        let mut file = Dss7File::open(f).unwrap();
        let cat = file.catalog().unwrap();
        cat.catalog
            .live()
            .map(|e| (e.pathname.clone(), e.record_type))
            .collect()
    }
}

/// Every known pathname that a lookup finds, decoded.
fn snapshot(v: u8, f: &Path, known: &Known) -> Snap {
    let mut out = BTreeMap::new();
    if v == 6 {
        let file = Dss6File::open(f).unwrap();
        for (p, &rt) in known {
            if file.lookup(p).unwrap().is_none() {
                continue;
            }
            let d = match rt {
                100..=119 => format!("{:?}", file.read_ts_record(p).unwrap()),
                200..=209 => format!("{:?}", file.read_pd(p).unwrap().pd),
                400..=499 => format!("{:?}", file.read_grid(p).unwrap()),
                rt => format!("record type {rt}"),
            };
            out.insert(p.clone(), d);
        }
    } else {
        let mut file = Dss7File::open(f).unwrap();
        for (p, &rt) in known {
            let Some(r) = file.lookup(p).unwrap() else {
                continue;
            };
            let d = match rt {
                102 | 107 | 112 | 117 => format!("{:?}", file.read_ts_profile_record(&r).unwrap()),
                100..=119 => format!("{:?}", file.read_ts_record(&r).unwrap()),
                200..=209 => format!("{:?}", file.read_pd_record(&r).unwrap().pd),
                400..=499 => format!("{:?}", file.read_grid_record(&r).unwrap()),
                20 => {
                    // The write time is the clock's, not the record's content.
                    let mut l = file.read_location_record(&r).unwrap();
                    l.last_write_ms = 0;
                    format!("{l:?}")
                }
                rt => format!("record type {rt}"),
            };
            out.insert(p.clone(), d);
        }
    }
    out
}

/// Problems `check_file6` may report on an interrupted DSS6 file (dss6.md §9.3).
fn allowed_after_kill6(problems: &[String]) -> bool {
    problems.iter().all(|p| {
        p.starts_with("in-progress marker")
            || p.starts_with("KNRECS is")
            || p.starts_with("KBINS is")
            || p.starts_with("KHUSED is")
            || p.starts_with("KBOVER is")
    })
}

/// heclib `zcheckPathnameBins()` (`SemiPublic/zcheckPathnameBins.c` lines 99-270),
/// mirrored: in every bin of every block of the bin-block chain, every word after the
/// last entry is zero, except the bin's last (overflow) word. Returns the offending
/// `(bin address, position)` pairs.
fn bins_not_zero_after_entries(f: &Path) -> Vec<(i64, usize)> {
    let w = Dss7File::open(f).unwrap().header().words;
    let (bs, bpb) = (w[23] as usize, w[21] as usize);
    let bytes = std::fs::read(f).unwrap();
    let word = |a: i64| {
        let i = a as usize * 8;
        i64::from_le_bytes(bytes[i..i + 8].try_into().unwrap())
    };
    let mut bad = Vec::new();
    let mut block = w[24];
    let mut seen = 0;
    while block != 0 && seen < 100_000 {
        seen += 1;
        for j in 0..bpb {
            let bin = block + (j * bs) as i64;
            let mut ic = 0usize;
            while ic + 7 < bs && word(bin + ic as i64) != 0 {
                let pw = (word(bin + ic as i64 + 2) >> 32) as usize;
                ic += 7 + pw;
            }
            for pos in ic..bs - 1 {
                if word(bin + pos as i64) != 0 {
                    bad.push((bin, pos));
                    break;
                }
            }
        }
        block = word(block + (bs * bpb) as i64);
    }
    bad
}

/// Structural cleanliness, and the catalog lists exactly `want_listed`.
fn assert_clean(v: u8, f: &Path, what: &str, want_listed: Option<&BTreeSet<Vec<u8>>>) {
    if v == 6 {
        let c = check_file6(f).unwrap();
        assert!(c.is_clean(), "{what}: {:?}", c.problems);
    } else {
        let mut file = Dss7File::open(f).unwrap();
        let cat = file.catalog().unwrap();
        assert!(
            cat.catalog.anomalies.is_empty(),
            "{what}: {:?}",
            cat.catalog.anomalies
        );
        let live: Vec<Vec<u8>> = cat.catalog.live().map(|e| e.pathname.clone()).collect();
        assert_eq!(file.header().number_records, live.len() as i64, "{what}");
        // What heclib's zcheckPathnameBins() checks.
        let bad = bins_not_zero_after_entries(f);
        assert!(
            bad.is_empty(),
            "{what}: non-zero words after the last entry: {bad:?}"
        );
        for p in &live {
            assert!(
                file.lookup(p).unwrap().is_some(),
                "{what}: {p:?} unreachable"
            );
        }
    }
    if let Some(w) = want_listed {
        let got: BTreeSet<Vec<u8>> = listed(v, f).into_keys().collect();
        assert_eq!(&got, w, "{what}: catalog listing");
    }
}

/// The on-disk F1 state: the cursor bin (or the first bin of a bin block the chain word
/// already links) has a zero first word and a non-zero body.
fn f1_state(f: &Path) -> bool {
    let w = Dss7File::open(f).unwrap().header().words;
    let mut file = std::fs::File::open(f).unwrap();
    let mut word = |a: i64| {
        let mut b = [0u8; 8];
        file.seek(SeekFrom::Start(a as u64 * 8)).unwrap();
        file.read_exact(&mut b).unwrap();
        i64::from_le_bytes(b)
    };
    let mut cursor = w[NEXT_EMPTY_BIN];
    if w[BINS_REMAIN_IN_BLOCK] <= 0 {
        cursor = word(cursor);
        if cursor == 0 {
            return false;
        }
    }
    let bs = w[23];
    word(cursor) == 0 && (1..bs).any(|i| word(cursor + i) != 0)
}

// ---------------------------------------------------------------------------------
// The child process.
// ---------------------------------------------------------------------------------

static KILL_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
static SEEN: AtomicUsize = AtomicUsize::new(0);

fn pause_hook(block: usize, step: Option<WriteStep>) {
    let i = SEEN.fetch_add(1, Ordering::SeqCst);
    if std::env::var_os("P64_KILL_TRACE").is_some() {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "P64-TRACE {i} block={block} step={step:?}");
        let _ = out.flush();
    }
    if i == KILL_AT.load(Ordering::SeqCst) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "P64-PAUSED {i} block={block} step={step:?}");
        let _ = out.flush();
        drop(out);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}

/// Child half of the kill matrix: runs one write and blocks at barrier `P64_KILL_AT`.
#[test]
#[ignore = "child process of the P64 kill matrix"]
fn p64_kill_child() {
    let (Ok(case), Ok(file), Ok(at), Ok(sync)) = (
        std::env::var("P64_KILL_CASE"),
        std::env::var("P64_KILL_FILE"),
        std::env::var("P64_KILL_AT"),
        std::env::var("P64_KILL_SYNC"),
    ) else {
        return;
    };
    KILL_AT.store(at.parse().unwrap(), Ordering::SeqCst);
    // R17: with P64_KILL_FINE=1 the hook also runs before every separate write.
    dss_core::dss7::write::set_write_barriers(std::env::var_os("P64_KILL_FINE").is_some());
    assert!(set_barrier_hook(pause_hook));
    let r = run_write(&case, Path::new(&file), &mode(sync == "1"));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "P64-DONE {} {:?}",
        SEEN.load(Ordering::SeqCst),
        r.err()
    );
    let _ = out.flush();
}

/// What the child reported.
enum Outcome {
    Killed(String),
    Done(usize),
}

/// Run the child on `file` and kill it at coarse barrier `at` (flush points and step
/// boundaries only; the numbering the fixed-index regressions use).
fn kill_at(case: &str, file: &Path, at: usize, sync: bool) -> Outcome {
    kill_at_mode(case, file, at, sync, false)
}

/// Run the child and kill it at barrier `at`, where every separate file write is also a
/// barrier (R17, dss7.md §6.4). The enumerating matrices use this.
fn kill_at_fine(case: &str, file: &Path, at: usize, sync: bool) -> Outcome {
    kill_at_mode(case, file, at, sync, true)
}

fn kill_at_mode(case: &str, file: &Path, at: usize, sync: bool, fine: bool) -> Outcome {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    if fine {
        cmd.env("P64_KILL_FINE", "1");
    }
    let mut child = cmd
        .args([
            "--exact",
            "p64_kill_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("P64_KILL_CASE", case)
        .env("P64_KILL_FILE", file)
        .env("P64_KILL_AT", at.to_string())
        .env("P64_KILL_SYNC", if sync { "1" } else { "0" })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let outcome = loop {
        let Some(line) = lines.next() else {
            let st = child.wait().unwrap();
            panic!("{case} at {at}: child ended without a report ({st})");
        };
        let line = line.unwrap();
        // libtest may print "test p64_kill_child ... " before the report on one line.
        if let Some(rest) = line.split_once("P64-PAUSED ").map(|x| x.1) {
            // The OS kill, while the child holds the file and its write lock.
            child.kill().unwrap();
            break Outcome::Killed(rest.to_string());
        }
        if let Some(rest) = line.split_once("P64-DONE ").map(|x| x.1) {
            let mut it = rest.splitn(2, ' ');
            let n: usize = it.next().unwrap().parse().unwrap();
            let err = it.next().unwrap_or("");
            assert_eq!(
                err, "None",
                "{case}: the uninterrupted child write failed: {err}"
            );
            break Outcome::Done(n);
        }
    };
    let st = child.wait().unwrap();
    if matches!(outcome, Outcome::Done(_)) {
        assert!(st.success(), "{case}: child exit {st}");
    }
    outcome
}

/// `f` once the killed child's lock is gone, retrying a refusal that lags `wait` (on
/// Windows the kernel releases the lock as part of process teardown).
fn retry_locked<T>(what: &str, mut op: impl FnMut() -> Result<T, WriteError>) -> T {
    for i in 0..50 {
        match op() {
            Ok(x) => return x,
            Err(WriteError::Locked(_)) if i < 49 => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            Err(e) => panic!("{what}: {e}"),
        }
    }
    unreachable!()
}

/// The next write after a kill, in the default mode.
fn repair(case: &str, f: &Path) {
    retry_locked(case, || run_write(case, f, &WriteOptions::default()));
}

fn open_after_kill(f: &Path) {
    // The killed child's whole-file lock (Windows) is gone once the file opens for reading.
    for _ in 0..50 {
        if std::fs::read(f).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("{f:?} still unreadable after the kill");
}

#[derive(Default)]
struct Counts {
    kills: usize,
    /// Coarse barriers (flush points and step boundaries) of the uninterrupted writes;
    /// `kills - coarse` kill points lie immediately before a separate write (R17).
    coarse: usize,
    second_kills: usize,
    /// Kills that left the F1 state on disk, per case.
    f1_kills: BTreeMap<String, usize>,
    /// `(case, mode, kill point, phase, message)` of every failed check.
    failures: Vec<(String, String, String, String, String)>,
}

impl Counts {
    /// Run one phase of checks; a failed assertion is recorded, not propagated, so the
    /// whole matrix is mapped and no phase hides another.
    fn phase(&mut self, case: &str, tag: &str, point: &str, phase: &str, f: impl FnOnce()) {
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            self.failures.push((
                case.into(),
                tag.into(),
                point.into(),
                phase.into(),
                msg.lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(1000)
                    .collect(),
            ));
        }
    }
}

/// What a case's checks compare against.
struct Expect {
    known: Known,
    before: Snap,
    want: Snap,
    want_listed: BTreeSet<Vec<u8>>,
    targets: BTreeSet<Vec<u8>>,
}

fn check_killed_payload(v: u8, f: &Path, what: &str, x: &Expect) {
    let now = snapshot(v, f, &x.known);
    for k in x.known.keys() {
        let (old, new, got) = (x.before.get(k), x.want.get(k), now.get(k));
        if x.targets.contains(k) {
            assert!(
                got == old || got == new,
                "{what}: {k:?} is neither old nor new"
            );
        } else {
            assert_eq!(got, old, "{what}: prior record {k:?}");
        }
    }
}

fn check_killed_structure6(f: &Path, what: &str) {
    let c = check_file6(f).unwrap();
    assert!(allowed_after_kill6(&c.problems), "{what}: {:?}", c.problems);
}

/// Repair, then the structure and payload checks as separate phases.
#[allow(clippy::too_many_arguments)]
fn repaired_phases(
    counts: &mut Counts,
    case: &str,
    tag: &str,
    point: &str,
    label: &str,
    f: &Path,
    x: &Expect,
) {
    let v = version(case);
    let what = format!("{case} {tag} kill {point} {label}");
    counts.phase(case, tag, point, &format!("{label} write"), || {
        repair(case, f)
    });
    counts.phase(case, tag, point, &format!("{label} structure"), || {
        assert_clean(v, f, &what, Some(&x.want_listed))
    });
    counts.phase(case, tag, point, &format!("{label} payload"), || {
        assert_eq!(&snapshot(v, f, &x.known), &x.want, "{what}")
    });
}

fn expect(case: &str, base: &Path, reference: &Path) -> Expect {
    let v = version(case);
    let mut known = listed(v, base);
    let ref_listed = listed(v, reference);
    known.extend(ref_listed.clone());
    let before = snapshot(v, base, &known);
    let want = snapshot(v, reference, &known);
    let targets: BTreeSet<Vec<u8>> = known
        .keys()
        .filter(|k| before.get(*k) != want.get(*k))
        .cloned()
        .collect();
    assert!(!targets.is_empty(), "{case}: the write changes nothing");
    Expect {
        known,
        before,
        want,
        want_listed: ref_listed.into_keys().collect(),
        targets,
    }
}

/// Case-specific checks of the reference: the write takes the path the case is for.
fn check_reference_shape(case: &str, base: &Path, reference: &Path) {
    let cat = |f: &Path| Dss7File::open(f).unwrap().catalog().unwrap();
    match case {
        "fill7_last" => assert_eq!(hword(reference, BINS_REMAIN_IN_BLOCK), 0, "{case}"),
        "fill7_block" => {
            assert_eq!(hword(base, BINS_REMAIN_IN_BLOCK), 0, "{case}");
            let (b, r) = (cat(base), cat(reference));
            assert_eq!(
                r.stats.blocks,
                b.stats.blocks + 1,
                "{case}: a new bin block"
            );
        }
        "overflow7" => {
            let c = cat(reference);
            let ps = overflow_paths();
            let i = c
                .catalog
                .entries
                .iter()
                .position(|e| &e.pathname == ps.last().unwrap())
                .unwrap();
            assert_eq!(
                c.meta[i].chain,
                ChainState::OwnChain(1),
                "{case}: overflow bin"
            );
        }
        "reuse7" => {
            let (b, r) = (cat(base), cat(reference));
            assert_eq!(
                b.catalog.entries.len(),
                r.catalog.entries.len(),
                "{case}: reuse"
            );
            assert_eq!(b.stats.deleted, 1, "{case}");
            assert_eq!(r.stats.deleted, 0, "{case}");
        }
        "delete7" => assert_eq!(cat(reference).stats.deleted, 1, "{case}"),
        _ => {}
    }
}

fn check_case(case: &str, dir: &Path, sync: bool, second: bool, counts: &mut Counts) {
    let v = version(case);
    let base = dir.join(format!("{case}_base.dss"));
    make_base(case, &base);
    assert_clean(v, &base, &format!("{case} base"), None);
    // The reference: the same write, uninterrupted, in the default mode.
    let reference = dir.join(format!("{case}_ref.dss"));
    std::fs::copy(&base, &reference).unwrap();
    run_write(case, &reference, &WriteOptions::default()).unwrap();
    assert_clean(v, &reference, &format!("{case} reference"), None);
    if v == 7 {
        check_reference_shape(case, &base, &reference);
    }
    let x = expect(case, &base, &reference);
    let keep = std::env::var_os("DSS_P64_KILL_OUT").map(PathBuf::from);
    let tag = if sync { "sync" } else { "nosync" };
    let mut at = 0;
    loop {
        let f = dir.join(format!("{case}_{tag}_{at}.dss"));
        std::fs::copy(&base, &f).unwrap();
        let point = match kill_at_fine(case, &f, at, sync) {
            Outcome::Done(n) => {
                // Every barrier of the write was a kill point.
                assert_eq!(n, at, "{case}: barrier count");
                assert_eq!(snapshot(v, &f, &x.known), x.want, "{case}: child write");
                assert_clean(v, &f, &format!("{case} child"), Some(&x.want_listed));
                break;
            }
            Outcome::Killed(p) => p,
        };
        open_after_kill(&f);
        counts.kills += 1;
        if v == 7 && f1_state(&f) {
            *counts.f1_kills.entry(case.to_string()).or_default() += 1;
        }
        let what = format!("{case} {tag} kill {point}");
        counts.phase(case, tag, &point, "killed payload", || {
            check_killed_payload(v, &f, &what, &x)
        });
        if v == 6 {
            counts.phase(case, tag, &point, "killed structure", || {
                check_killed_structure6(&f, &what)
            });
        }
        if let Some(k) = &keep {
            std::fs::create_dir_all(k).unwrap();
            std::fs::copy(&f, k.join(format!("{case}_{tag}_{at}_killed.dss"))).unwrap();
            std::fs::copy(&reference, k.join(format!("{case}_ref.dss"))).unwrap();
        }
        // A second kill inside the repairing write, at a few of its barriers.
        if second {
            for at2 in [0usize, 1, 2, 3, 5, 8, 13] {
                let g = dir.join(format!("{case}_{tag}_{at}_{at2}.dss"));
                std::fs::copy(&f, &g).unwrap();
                match kill_at_fine(case, &g, at2, false) {
                    Outcome::Done(_) => {}
                    Outcome::Killed(_) => {
                        counts.second_kills += 1;
                        open_after_kill(&g);
                    }
                }
                let label = format!("second kill {at2}, repaired");
                repaired_phases(counts, case, tag, &point, &label, &g, &x);
                if let Some(k) = &keep {
                    let name = format!("{case}_{tag}_{at}_second{at2}_repaired.dss");
                    std::fs::copy(&g, k.join(name)).unwrap();
                }
                let _ = std::fs::remove_file(&g);
            }
        }
        // The next native write repairs and completes it.
        repaired_phases(counts, case, tag, &point, "repaired", &f, &x);
        if let Some(k) = &keep {
            std::fs::copy(&f, k.join(format!("{case}_{tag}_{at}_repaired.dss"))).unwrap();
        }
        let _ = std::fs::remove_file(&f);
        at += 1;
    }
    // Hook coverage: the same write with coarse barriers only.
    let g = dir.join(format!("{case}_{tag}_coarse.dss"));
    std::fs::copy(&base, &g).unwrap();
    let coarse = match kill_at(case, &g, usize::MAX, sync) {
        Outcome::Done(n) => n,
        Outcome::Killed(_) => unreachable!(),
    };
    let _ = std::fs::remove_file(&g);
    counts.coarse += coarse;
    eprintln!(
        "{case} {tag}: {at} kill points ({coarse} coarse, {} before a separate write)",
        at - coarse
    );
}

fn report(counts: &Counts, what: &str) {
    eprintln!(
        "P64 {what}: {} kills ({} coarse barriers, {} before a separate write), {} second kills, {} failed checks; F1-state kills {:?}",
        counts.kills,
        counts.coarse,
        counts.kills - counts.coarse,
        counts.second_kills,
        counts.failures.len(),
        counts.f1_kills
    );
    for (c, t, p, ph, m) in &counts.failures {
        eprintln!("P64-FAIL {c} {t} [{p}] {ph}: {m}");
    }
}

/// Every DSS7 case that takes a new bin reaches the F1 window (a kill there leaves a bin
/// with a body and a zero first word), and no other case does.
fn assert_f1_window_reached(counts: &Counts) {
    for case in NEW_BIN_CASES {
        assert!(
            counts.f1_kills.get(case).copied().unwrap_or(0) >= 1,
            "{case}: the F1 window was not reached: {:?}",
            counts.f1_kills
        );
    }
    for case in counts.f1_kills.keys() {
        assert!(NEW_BIN_CASES.contains(&case.as_str()), "{case}");
    }
}

const SECOND_KILL_CASES: [&str; 8] = [
    "rts7_new",
    "rts6_multi",
    "its7",
    "pd6",
    "fill7_block",
    "overflow7",
    "reuse7",
    "pattern7_new",
];

/// Opt-in mode (`sync: false`): every barrier of every case, DSS7 and DSS6, plus second
/// kills inside the repairing write for a subset.
#[test]
fn killed_at_every_barrier_nosync() {
    let d = tmp("nosync");
    let mut counts = Counts::default();
    for case in cases() {
        check_case(
            case,
            &d,
            false,
            SECOND_KILL_CASES.contains(&case),
            &mut counts,
        );
    }
    report(&counts, "nosync");
    assert!(counts.kills > 300, "{}", counts.kills);
    assert!(counts.failures.is_empty(), "{:?}", counts.failures);
    assert_f1_window_reached(&counts);
    let _ = std::fs::remove_dir_all(&d);
}

/// The default mode under the same kills.
#[test]
fn killed_at_every_barrier_default_sync() {
    let d = tmp("sync");
    let mut counts = Counts::default();
    for case in cases() {
        check_case(case, &d, true, false, &mut counts);
    }
    report(&counts, "default sync");
    assert!(counts.failures.is_empty(), "{:?}", counts.failures);
    assert_f1_window_reached(&counts);
    let _ = std::fs::remove_dir_all(&d);
}

/// Both modes write the same records.
#[test]
fn modes_write_the_same_records() {
    let d = tmp("modes");
    for case in CASES {
        let v = version(case);
        let a = d.join(format!("{case}_a.dss"));
        let b = d.join(format!("{case}_b.dss"));
        make_base(case, &a);
        std::fs::copy(&a, &b).unwrap();
        run_write(case, &a, &mode(true)).unwrap();
        run_write(case, &b, &mode(false)).unwrap();
        let known = listed(v, &a);
        assert_eq!(listed(v, &b), known, "{case}");
        assert_eq!(snapshot(v, &a, &known), snapshot(v, &b, &known), "{case}");
        assert_clean(v, &b, case, None);
    }
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// The F1 window itself, and files damaged by engines before 0.1.1.
// ---------------------------------------------------------------------------------

/// The F1 kill point of `rts7_new`: the flush between the new bin's body and its first
/// word (`step=None` right before `Publish`).
const RTS7_NEW_F1: usize = 7;

/// A killed F1 write is repaired as if uninterrupted: the record goes into the bin the
/// killed write left (now rewritten whole), and later new records stay listed.
#[test]
fn f1_window_is_repaired_in_place() {
    let d = tmp("f1");
    let case = "rts7_new";
    let base = d.join("base.dss");
    make_base(case, &base);
    let f = d.join("f1.dss");
    std::fs::copy(&base, &f).unwrap();
    let cursor = hword(&base, NEXT_EMPTY_BIN);
    assert!(
        matches!(kill_at(case, &f, RTS7_NEW_F1, false), Outcome::Killed(p) if p == "7 block=0 step=None")
    );
    open_after_kill(&f);
    assert!(f1_state(&f));
    repair(case, &f);
    // The bin the killed write had taken holds the record.
    let mut file = Dss7File::open(&f).unwrap();
    let cat = file.catalog().unwrap();
    let i = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname.starts_with(b"/T/NEW/"))
        .expect("listed");
    assert_eq!(cat.meta[i].bin_address, cursor);
    drop(file);
    let o = WriteOptions::default();
    for i in 0..3 {
        let p = format!("/T/LATER{i}/FLOW//1DAY/S/");
        hourly(7, &f, p.as_bytes(), t(2021, 6, 1, 0), &[i as f32], &o).unwrap();
    }
    assert_clean(7, &f, "f1 later writes", None);
    let names = listed(7, &f);
    for i in 0..3 {
        let p = format!("/T/LATER{i}/FLOW/01Jan2021/1Day/S/");
        assert!(names.contains_key(p.as_bytes()), "{p}");
    }
    // Byte layout of the repaired bin equals the uninterrupted reference's.
    let r = d.join("ref.dss");
    std::fs::copy(&base, &r).unwrap();
    run_write(case, &r, &o).unwrap();
    let bin = |p: &Path| {
        let b = std::fs::read(p).unwrap();
        let a = cursor as usize * 8;
        let mut v = b[a..a + 200 * 8].to_vec();
        // Words 5 (last write time) of the entry differ by clock only.
        v[5 * 8..6 * 8].fill(0);
        v
    };
    let g = d.join("f1_again.dss");
    std::fs::copy(&base, &g).unwrap();
    assert!(matches!(
        kill_at(case, &g, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&g);
    repair(case, &g);
    let (got, want) = (bin(&g), bin(&r));
    // Word 3 (info address) differs: the repairing write's record is stored after the
    // killed write's dead one.
    let strip = |mut v: Vec<u8>| {
        v[3 * 8..4 * 8].fill(0);
        v
    };
    assert_eq!(strip(got), strip(want));
    let _ = std::fs::remove_dir_all(&d);
}

/// Where the copy-out repair of `f` writes.
fn copy_dest(f: &Path) -> PathBuf {
    f.with_extension("copyout.dss")
}

/// Decoded payload of a record by its type (time-dependent fields cleared).
fn decode_rec(
    file: &mut Dss7File<std::fs::File>,
    rt: i32,
    r: &dss_core::dss7::RecordRef,
) -> Option<String> {
    // Pathnames are compared case-insensitively, as lookups match them (copy-out
    // writes a record under its info block's own spelling).
    let up = |p: &mut Vec<u8>| p.make_ascii_uppercase();
    match rt {
        102 | 107 | 112 | 117 => file
            .read_ts_profile_record(r)
            .map(|mut x| {
                up(&mut x.pathname);
                format!("{x:?}")
            })
            .ok(),
        100..=119 => file
            .read_ts_record(r)
            .map(|mut x| {
                up(&mut x.ts.pathname);
                x.meta.block_pathnames.iter_mut().for_each(up);
                format!("{x:?}")
            })
            .ok(),
        200..=209 => file
            .read_pd_record(r)
            .map(|mut x| {
                up(&mut x.pd.pathname);
                format!("{:?}", x.pd)
            })
            .ok(),
        400..=499 => file
            .read_grid_record(r)
            .map(|mut x| {
                up(&mut x.pathname);
                format!("{x:?}")
            })
            .ok(),
        20 => file.read_location_record(r).ok().map(|mut l| {
            l.last_write_ms = 0;
            l.program.clear();
            up(&mut l.location.pathname);
            format!("{l:?}")
        }),
        rt => Some(format!("record type {rt}")),
    }
}

/// Every record a reader can read in `f`, by pathname: the recovery catalog's live
/// entries, each through a lookup or, when that copy does not decode, any catalog copy
/// of the pathname that does; then every alias's primary record under the
/// primary's own pathname.
fn readable(f: &Path) -> BTreeMap<Vec<u8>, String> {
    let mut out = BTreeMap::new();
    let Ok(mut file) = Dss7File::open(f) else {
        return out;
    };
    let opts = dss_core::dss7::CatalogOptions {
        recover_unlinked: true,
        ..Default::default()
    };
    let Ok(cat) = file.catalog_with(&opts) else {
        return out;
    };
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        if !matches!(cat.meta[i].raw_status, 0 | 1) {
            continue;
        }
        let key = e.pathname.to_ascii_uppercase();
        if out.contains_key(&key) {
            continue;
        }
        let mut cands = Vec::new();
        if let Ok(Some(r)) = file.lookup(&e.pathname) {
            cands.push(r);
        }
        if let Ok(r) = file.record_at(&cat, i) {
            cands.push(r);
        }
        for r in cands {
            if let Some(d) = decode_rec(&mut file, e.record_type, &r) {
                out.insert(key, d);
                break;
            }
        }
    }
    for i in 0..cat.catalog.entries.len() {
        if cat.meta[i].raw_status != 2 {
            continue;
        }
        let Ok(r) = file.record_at(&cat, i) else {
            continue;
        };
        let n = r.info.pathname_length.clamp(0, 392) as usize;
        let primary: Vec<u8> = (0..n.div_ceil(8))
            .flat_map(|j| file_word(f, r.info_address + 30 + j as i64).to_le_bytes())
            .take(n)
            .collect();
        let key = primary.to_ascii_uppercase();
        if out.contains_key(&key) {
            continue;
        }
        let mut r = r;
        r.pathname = primary;
        if let Some(d) = decode_rec(&mut file, r.info.record_type, &r) {
            out.insert(key, d);
        }
    }
    out
}

/// Copy-out of `src`: `src` stays byte-identical, and `dest` holds every record
/// `readable` finds in `src`, equal, and catalogs cleanly. Returns the report.
fn copy_out_ok(src: &Path, what: &str) -> dss_core::dss7::write::RepairReport {
    let dest = copy_dest(src);
    let _ = std::fs::remove_file(&dest);
    let bytes = std::fs::read(src).unwrap();
    let want = readable(src);
    let r = repair_dss7(src, &dest).unwrap_or_else(|e| panic!("{what}: copy-out: {e}"));
    assert_eq!(std::fs::read(src).unwrap(), bytes, "{what}: src changed");
    let got = readable(&dest);
    for (k, v) in &want {
        assert_eq!(
            got.get(k),
            Some(v),
            "{what}: {:?} not copied",
            String::from_utf8_lossy(k)
        );
    }
    assert_eq!(r.records_copied, got.len(), "{what}: report");
    assert_clean(7, &dest, what, None);
    let _ = std::fs::remove_file(&dest);
    r
}

/// Copy-out that may refuse: in every case `src` stays byte-identical, and when it
/// succeeds `dest` holds every record readable in `src`.
fn copy_out_preserves(src: &Path, what: &str) -> bool {
    let dest = copy_dest(src);
    let _ = std::fs::remove_file(&dest);
    let bytes = std::fs::read(src).unwrap();
    let want = readable(src);
    let r = repair_dss7(src, &dest);
    assert_eq!(
        std::fs::read(src).unwrap(),
        bytes,
        "{what}: src changed ({r:?})"
    );
    let ok = r.is_ok();
    if ok {
        let got = readable(&dest);
        for (k, v) in &want {
            assert_eq!(
                got.get(k),
                Some(v),
                "{what}: {:?} not copied",
                String::from_utf8_lossy(k)
            );
        }
    } else {
        assert!(
            std::fs::symlink_metadata(&dest).is_err(),
            "{what}: dest left after refusal"
        );
    }
    let _ = std::fs::remove_file(&dest);
    ok
}

/// The words of the bin at `bin`.
fn bin_words(f: &Path, bin: i64) -> Vec<i64> {
    (0..hword(f, 23)).map(|i| file_word(f, bin + i)).collect()
}

/// A file as an engine before 0.1.1 left it: killed at F1, then the old reconciliation
/// skipped the half-written bin (cursor past it) and the repairing write stored the
/// record in the next bin, with the marker cleared.
fn legacy_damaged(d: &Path) -> PathBuf {
    let case = "rts7_new";
    let base = d.join("legacy_base.dss");
    make_base(case, &base);
    let f = d.join("legacy.dss");
    std::fs::copy(&base, &f).unwrap();
    assert!(matches!(
        kill_at(case, &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    assert!(f1_state(&f));
    // The old reconcile step 2: the non-zero bin at the cursor counts as used.
    let w = Dss7File::open(&f).unwrap().header().words;
    let patch = |i: usize, x: i64| {
        let mut h = std::fs::OpenOptions::new().write(true).open(&f).unwrap();
        h.seek(SeekFrom::Start(i as u64 * 8)).unwrap();
        h.write_all(&x.to_le_bytes()).unwrap();
    };
    assert!(w[BINS_REMAIN_IN_BLOCK] > 0);
    patch(NEXT_EMPTY_BIN, w[NEXT_EMPTY_BIN] + w[23]);
    patch(BINS_REMAIN_IN_BLOCK, w[BINS_REMAIN_IN_BLOCK] - 1);
    patch(TOTAL_BINS, w[TOTAL_BINS] + 1);
    patch(PENDING_WORD, 0);
    repair(case, &f);
    f
}

fn hidden_count(f: &Path) -> (i64, Vec<String>) {
    let mut file = Dss7File::open(f).unwrap();
    let cat = file.catalog().unwrap();
    let listed = cat.catalog.live().count() as i64;
    let anomalies = cat
        .catalog
        .anomalies
        .iter()
        .map(|a| format!("{:?}: {}", a.kind, a.detail))
        .collect();
    (file.header().number_records - listed, anomalies)
}

/// The F1 persistence pin, flipped now that in-place hole repair is gone: a damaged file reports its hidden
/// records, later writes still land behind the hole, and the copy-out repair writes a
/// new file in which every record is listed; the damaged file is not changed.
#[test]
fn legacy_f1_hole_is_reported_and_repaired() {
    let d = tmp("legacy");
    let f = legacy_damaged(&d);
    let (missing, anomalies) = hidden_count(&f);
    assert_eq!(missing, 1);
    assert!(
        anomalies.iter().any(|a| a.contains("RecordCountMismatch"))
            && anomalies
                .iter()
                .any(|a| a.contains("lookup can reach but the catalog does not list")),
        "{anomalies:?}"
    );
    let o = WriteOptions::default();
    for i in 0..3 {
        let p = format!("/T/LATER{i}/FLOW//1DAY/S/");
        hourly(7, &f, p.as_bytes(), t(2021, 6, 1, 0), &[i as f32], &o).unwrap();
    }
    assert_eq!(hidden_count(&f).0, 4);
    if let Some(k) = std::env::var_os("DSS_P64_KILL_OUT").map(PathBuf::from) {
        std::fs::create_dir_all(&k).unwrap();
        std::fs::copy(&f, k.join("legacy_damaged.dss")).unwrap();
    }
    let r = copy_out_ok(&f, "legacy");
    assert_eq!(r.records_copied, 13);
    assert_eq!(r.hidden_records_copied, 4);
    assert!(r.complete());
    // The new file lists all 13 (copy_out_ok checked them record by record).
    let dest = copy_dest(&f);
    repair_dss7(&f, &dest).unwrap();
    assert_eq!(listed(7, &dest).len(), 13);
    if let Some(k) = std::env::var_os("DSS_P64_KILL_OUT").map(PathBuf::from) {
        std::fs::copy(&dest, k.join("legacy_copyout.dss")).unwrap();
    }
    // `dest` exists now: a second copy-out refuses and changes nothing.
    let bytes = std::fs::read(&dest).unwrap();
    assert!(matches!(
        repair_dss7(&f, &dest),
        Err(WriteError::AlreadyExists(_))
    ));
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    let _ = std::fs::remove_dir_all(&d);
}

/// Round 6: a write that reconciles an interrupted write on a damaged file does not
/// touch the old hole (no in-place repair of earlier damage); the hole bin's words are
/// unchanged, and the copy-out repair recovers the hidden record.
#[test]
fn legacy_hole_is_not_written_by_a_reconciling_write() {
    let d = tmp("legacy_rec");
    let f = legacy_damaged(&d);
    let hole = legacy_hole(&f);
    let hole_before = bin_words(&f, hole);
    assert_eq!(hidden_count(&f).0, 1);
    let case = "pd7_new";
    let mut killed = false;
    for at in 0..30 {
        let g = d.join(format!("g{at}.dss"));
        std::fs::copy(&f, &g).unwrap();
        match kill_at(case, &g, at, false) {
            Outcome::Done(_) => break,
            Outcome::Killed(_) => {
                open_after_kill(&g);
                if hword(&g, PENDING_WORD) == 0 {
                    continue;
                }
                killed = true;
                repair(case, &g);
                assert_eq!(bin_words(&g, hole), hole_before, "hole written at {at}");
                assert!(hidden_count(&g).0 >= 1, "hole repaired in place at {at}");
                let r = copy_out_ok(&g, &format!("reconciled at {at}"));
                assert!(r.hidden_records_copied >= 1);
            }
        }
    }
    assert!(killed);
    let _ = std::fs::remove_dir_all(&d);
}

/// A hole that does not have the exact shape of an unpublished entry: the damaged file
/// is never rewritten; copy-out copies what is readable and changes nothing in it.
#[test]
fn legacy_hole_of_unknown_shape_is_reported_not_rewritten() {
    let d = tmp("legacy_bad");
    let f = legacy_damaged(&d);
    let w = Dss7File::open(&f).unwrap().header().words;
    let hole = w[NEXT_EMPTY_BIN] - 2 * w[23];
    let mut bytes = std::fs::read(&f).unwrap();
    let word = |b: &[u8], a: i64| {
        i64::from_le_bytes(b[a as usize * 8..a as usize * 8 + 8].try_into().unwrap())
    };
    assert_eq!(word(&bytes, hole), 0);
    assert_ne!(word(&bytes, hole + 1), 0);
    let a = (hole + 150) as usize * 8;
    bytes[a..a + 8].copy_from_slice(&7i64.to_le_bytes());
    std::fs::write(&f, &bytes).unwrap();
    copy_out_ok(&f, "unknown-shape hole");
    let (missing, anomalies) = hidden_count(&f);
    assert_eq!(missing, 1);
    assert!(!anomalies.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// Word `a` of a file.
fn file_word(f: &Path, a: i64) -> i64 {
    let mut h = std::fs::File::open(f).unwrap();
    let mut b = [0u8; 8];
    h.seek(SeekFrom::Start(a as u64 * 8)).unwrap();
    h.read_exact(&mut b).unwrap();
    i64::from_le_bytes(b)
}

fn set_word(f: &Path, a: i64, x: i64) {
    let mut h = std::fs::OpenOptions::new().write(true).open(f).unwrap();
    h.seek(SeekFrom::Start(a as u64 * 8)).unwrap();
    h.write_all(&x.to_le_bytes()).unwrap();
}

/// The hole of [`legacy_damaged`]: the first bin of the bin-block chain whose first word
/// is zero while another word is not (test-side scan).
fn legacy_hole(f: &Path) -> i64 {
    let w = Dss7File::open(f).unwrap().header().words;
    let (bs, bpb) = (w[23], w[21]);
    let mut block = w[24];
    while block != 0 {
        for j in 0..bpb {
            let bin = block + j * bs;
            if file_word(f, bin) == 0 && (1..bs).any(|i| file_word(f, bin + i) != 0) {
                return bin;
            }
        }
        block = file_word(f, block + bs * bpb);
    }
    panic!("no hole in {f:?}");
}

const CAT_SORT_STATUS: usize = 33;
/// Info-block status word (`zdssInfoKeys.kinfoStatus`).
const INFO_STATUS: i64 = 1;

/// The legacy variants the repair is killed on: as left, with a stored catalog sort list
/// marked valid (`kcatSortStatus` 1), and with the in-progress marker set so
/// the reconciliation in `Session::begin` does the repair.
fn legacy_variant(d: &Path, variant: &str) -> PathBuf {
    let f = legacy_damaged(d);
    let g = d.join(format!("legacy_{variant}.dss"));
    std::fs::rename(&f, &g).unwrap();
    match variant {
        "plain" => {}
        "sorted" => set_word(&g, CAT_SORT_STATUS as i64, 1),
        "marker" => {
            let info = file_word(&g, legacy_hole(&g) + 3);
            set_word(&g, PENDING_WORD as i64, info);
        }
        _ => panic!("{variant}"),
    }
    g
}

fn header_words(f: &Path) -> Vec<i64> {
    Dss7File::open(f).unwrap().header().words.to_vec()
}

/// Round 6: the copy-out repair killed at each of its barriers (both modes; the copy
/// writes only its temporary file): `src` stays byte-identical, `dest` does not exist,
/// and a second copy-out then completes with every record. Variants: as left, with a
/// stored sort list marked valid, and with the in-progress marker set.
#[test]
fn legacy_repair_killed_at_every_barrier() {
    let mut kills = 0;
    for variant in ["plain", "sorted", "marker"] {
        let d = tmp(&format!("legacy_kill_{variant}"));
        let f = legacy_variant(&d, variant);
        let want = readable(&f);
        let r0 = copy_out_ok(&f, variant);
        assert_eq!(r0.src_interrupted_write, variant == "marker");
        for sync in [false, true] {
            let mut at = 0;
            loop {
                let g = d.join(format!("r{sync}{at}.dss"));
                std::fs::copy(&f, &g).unwrap();
                let bytes = std::fs::read(&g).unwrap();
                let what = format!("{variant} sync={sync} at {at}");
                match kill_at_fine("repair7", &g, at, sync) {
                    Outcome::Done(_) => {
                        assert_eq!(std::fs::read(&g).unwrap(), bytes, "{what}");
                        assert_eq!(readable(&copy_dest(&g)), want, "{what}");
                        let _ = std::fs::remove_file(copy_dest(&g));
                        break;
                    }
                    Outcome::Killed(p) => {
                        kills += 1;
                        open_after_kill(&g);
                        assert_eq!(std::fs::read(&g).unwrap(), bytes, "{what}: src after {p}");
                        assert!(
                            std::fs::symlink_metadata(copy_dest(&g)).is_err(),
                            "{what}: dest exists after a kill at {p}"
                        );
                        let r = retry_locked("copy-out", || repair_dss7(&g, &copy_dest(&g)));
                        assert!(r.complete(), "{what}");
                        assert_eq!(readable(&copy_dest(&g)), want, "{what}");
                        if let Some(k) = std::env::var_os("DSS_P64_KILL_OUT").map(PathBuf::from) {
                            std::fs::create_dir_all(&k).unwrap();
                            let n = format!("copyout_{variant}_{sync}_{at}.dss");
                            std::fs::copy(copy_dest(&g), k.join(n)).unwrap();
                        }
                        let _ = std::fs::remove_file(copy_dest(&g));
                    }
                }
                at += 1;
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
    assert!(kills >= 12, "{kills}");
    eprintln!("copy-out: {kills} kills");
}

/// A legacy file with the [`GAP`] series added, so a delete can be interrupted on it.
fn legacy_with_gap(d: &Path) -> PathBuf {
    let f = legacy_damaged(d);
    irregular(
        7,
        &f,
        GAP,
        &[t(2021, 2, 10, 0), t(2021, 3, 10, 0), t(2021, 4, 10, 0)],
        &[1.0, 2.0, 3.0],
        &WriteOptions::default(),
    )
    .unwrap();
    f
}

/// Every in-progress marker a real killed write leaves on a legacy file: a delete
/// (`delete7`, negative markers, rolled back or standing) and a new record (`pd7_new`,
/// positive markers), one file per kill point whose marker is set.
fn real_marker_variants(d: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for case in ["delete7", "pd7_new"] {
        let src = d.join(format!("{case}_src"));
        std::fs::create_dir_all(&src).unwrap();
        let base = if case == "delete7" {
            legacy_with_gap(&src)
        } else {
            legacy_damaged(&src)
        };
        for at in 0.. {
            let g = d.join(format!("{case}_{at}.dss"));
            std::fs::copy(&base, &g).unwrap();
            match kill_at_fine(case, &g, at, false) {
                Outcome::Done(_) => {
                    let _ = std::fs::remove_file(&g);
                    break;
                }
                Outcome::Killed(_) => {
                    open_after_kill(&g);
                    let m = hword(&g, PENDING_WORD);
                    if m == 0 {
                        let _ = std::fs::remove_file(&g);
                    } else {
                        let sign = if m < 0 { "neg" } else { "pos" };
                        out.push((format!("{case}_{at}_{sign}"), g));
                    }
                }
            }
        }
    }
    out
}

/// Every marker a real killed delete or new-record write leaves on a
/// legacy file is reconciled in place by the next write. That write killed at each
/// barrier and retried ends with the header (all words but `klastWriteTime`), catalog
/// listing and records of the uninterrupted write, in both modes. The legacy hole is
/// never written.
#[test]
fn repair_with_every_real_marker_killed_at_every_barrier() {
    let d = tmp("legacy_markers");
    let variants = real_marker_variants(&d);
    let neg = variants.iter().filter(|(n, _)| n.ends_with("neg")).count();
    let pos = variants.len() - neg;
    assert!(
        neg >= 3 && pos >= 3,
        "{neg} negative, {pos} positive markers"
    );
    let keep = std::env::var_os("DSS_P64_KILL_OUT").map(PathBuf::from);
    let strip = |mut h: Vec<i64>| {
        h[13] = 0; // klastWriteTime: the clock
        h
    };
    let (mut kills, mut rollbacks) = (0, 0);
    for (name, f) in &variants {
        let hole = legacy_hole(f);
        let hole_before = bin_words(f, hole);
        let r0 = d.join(format!("{name}_ref.dss"));
        std::fs::copy(f, &r0).unwrap();
        let before = header_words(f);
        run_write("reconcile7", &r0, &WriteOptions::default()).unwrap();
        assert_eq!(hword(&r0, PENDING_WORD), 0, "{name}");
        // The legacy hole stays (there is no in-place hole repair), so the reference keeps
        // its hidden-record anomalies; the killed-and-retried file must match it.
        let want_anomalies = hidden_count(&r0);
        let want_header = strip(header_words(&r0));
        if before[10] != want_header[10] {
            rollbacks += 1; // knumberDeletes taken back: the delete was rolled back
        }
        let mut known = listed(7, &r0);
        known.insert(b"/T/NEW/FLOW/01Mar2021/1Hour/S/".to_vec(), 100);
        let want = snapshot(7, &r0, &known);
        let want_listed: BTreeSet<Vec<u8>> = listed(7, &r0).into_keys().collect();
        if let Some(k) = &keep {
            std::fs::create_dir_all(k).unwrap();
            std::fs::copy(&r0, k.join(format!("marker_{name}_ref.dss"))).unwrap();
        }
        for sync in [false, true] {
            for at in 0.. {
                let g = d.join(format!("{name}_{sync}_{at}.dss"));
                std::fs::copy(f, &g).unwrap();
                let what = format!("{name} sync={sync} reconcile kill {at}");
                let done = match kill_at_fine("reconcile7", &g, at, sync) {
                    Outcome::Done(_) => true,
                    Outcome::Killed(_) => {
                        kills += 1;
                        open_after_kill(&g);
                        retry_locked("retry", || {
                            run_write("reconcile7", &g, &WriteOptions::default())
                        });
                        false
                    }
                };
                // Every header word equals the uninterrupted write's, except that the
                // space a killed attempt had allocated is abandoned: kfileSize (5) may be
                // higher, and kdead (6) by at most as much (reconciliation does not book an
                // interrupted record's space into kdead; dss7.md, "Not rebuilt"). A
                // repeated rollback would lower kdead instead.
                let got_header = strip(header_words(&g));
                let diff: Vec<(usize, i64, i64)> = (0..100)
                    // Also not rebuilt after an interrupted write (dss7.md, rebuild_statistics):
                    // kcatSortNewWrites (34), the area maxima (58-63) and area counts (64-69).
                    .filter(|&i| !matches!(i, 5 | 6 | 34 | 58..=69))
                    .filter(|&i| got_header[i] != want_header[i])
                    .map(|i| (i, got_header[i], want_header[i]))
                    .collect();
                assert!(
                    diff.is_empty(),
                    "{what}: header words (index, got, want) {diff:?}"
                );
                let grown = got_header[5] - want_header[5];
                assert!(grown >= 0, "{what}: kfileSize");
                let dead = got_header[6] - want_header[6];
                assert!(
                    (0..=grown).contains(&dead),
                    "{what}: kdead +{dead}, kfileSize +{grown}"
                );
                let got_listed: BTreeSet<Vec<u8>> = listed(7, &g).into_keys().collect();
                assert_eq!(got_listed, want_listed, "{what}: listing");
                assert_eq!(hidden_count(&g), want_anomalies, "{what}: anomalies");
                assert_eq!(snapshot(7, &g, &known), want, "{what}: records");
                assert_eq!(bin_words(&g, hole), hole_before, "{what}: hole written");
                if let (Some(k), false) = (&keep, done) {
                    let n = format!("marker_{name}_{sync}_{at}_repaired.dss");
                    std::fs::copy(&g, k.join(n)).unwrap();
                }
                let _ = std::fs::remove_file(&g);
                if done {
                    break;
                }
            }
        }
    }
    assert!(rollbacks >= 1, "no rolled-back delete among the variants");
    eprintln!(
        "real markers: {neg} negative, {pos} positive, {rollbacks} rolled back; {kills} reconcile kills"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A delete killed before Publish (rolled back)
/// on a file with a legacy hole; the reconciling write killed at its first barrier and
/// retried: `kdead` and every other header word (but the write time) equal the
/// uninterrupted write's.
#[test]
fn interrupted_delete_rollback_with_hole_is_restartable() {
    let d = tmp("r4");
    let base = legacy_with_gap(&d);
    let mut pick = None;
    for at in 0.. {
        let g = d.join(format!("del{at}.dss"));
        std::fs::copy(&base, &g).unwrap();
        match kill_at("delete7", &g, at, false) {
            Outcome::Done(_) => break,
            Outcome::Killed(_) => {
                open_after_kill(&g);
                if hword(&g, PENDING_WORD) < 0
                    && Dss7File::open(&g)
                        .unwrap()
                        .lookup(b"/T/GAP/STAGE/01Mar2021/IR-Month/S/")
                        .unwrap()
                        .is_some()
                {
                    pick = Some(g);
                }
            }
        }
    }
    let f = pick.expect("a delete killed before Publish");
    let r0 = d.join("ref.dss");
    std::fs::copy(&f, &r0).unwrap();
    run_write("reconcile7", &r0, &WriteOptions::default()).unwrap();
    let g = d.join("killed.dss");
    std::fs::copy(&f, &g).unwrap();
    assert!(matches!(
        kill_at("reconcile7", &g, 1, true),
        Outcome::Killed(_)
    ));
    open_after_kill(&g);
    retry_locked("retry", || {
        run_write("reconcile7", &g, &WriteOptions::default())
    });
    let (mut a, mut b) = (header_words(&g), header_words(&r0));
    a[13] = 0;
    b[13] = 0;
    assert_eq!(a[6], b[6], "kdead");
    assert_eq!(a, b);
    let _ = std::fs::remove_dir_all(&d);
}

/// The body a killed F1 Publish leaves in the cursor bin is this write's own entry: the
/// next reconciliation clears it in place even when no later write takes a new bin (a
/// replacement). The copy-out repair of the killed file leaves it as it is and writes a
/// clean new file.
#[test]
fn f1_leftover_is_cleared_when_no_new_bin_follows() {
    let d = tmp("f1_clear");
    let case = "rts7_new";
    let base = d.join("base.dss");
    make_base(case, &base);
    let f = d.join("replace.dss");
    std::fs::copy(&base, &f).unwrap();
    assert!(matches!(
        kill_at(case, &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    assert!(f1_state(&f));
    assert!(!bins_not_zero_after_entries(&f).is_empty());
    copy_out_ok(&f, "F1-killed file");
    assert!(f1_state(&f), "copy-out changed the source");
    let cursor = hword(&f, NEXT_EMPTY_BIN);
    retry_locked("replace", || {
        paired(
            7,
            &f,
            b"/T/PD/STAGE-FLOW///S/",
            7.0,
            &WriteOptions::default(),
        )
    });
    assert_eq!(hword(&f, NEXT_EMPTY_BIN), cursor, "cursor kept");
    assert!((0..200).all(|i| file_word(&f, cursor + i) == 0));
    assert_clean(7, &f, "replace", None);
    let _ = std::fs::remove_dir_all(&d);
}

/// Round 6: a cursor bin whose leftover body is not exactly this interrupted write's own
/// entry (here its info address word altered) is refused in place: the next write
/// returns an error and changes no byte.
#[test]
fn foreign_leftover_at_the_cursor_is_refused() {
    let d = tmp("f1_foreign");
    let f = d.join("f.dss");
    make_base("rts7_new", &f);
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let cursor = hword(&f, NEXT_EMPTY_BIN);
    set_word(&f, cursor + 3, file_word(&f, cursor + 3) + 8);
    let bytes = std::fs::read(&f).unwrap();
    let r = paired(
        7,
        &f,
        b"/T/PD/STAGE-FLOW///S/",
        7.0,
        &WriteOptions::default(),
    );
    assert!(
        matches!(&r, Err(WriteError::Corrupt(m)) if m.contains("not this interrupted write")),
        "{r:?}"
    );
    assert_eq!(std::fs::read(&f).unwrap(), bytes);
    copy_out_preserves(&f, "foreign leftover");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Repair writes never trust an unvalidated allocation cursor (dss7.md §6.4).
// ---------------------------------------------------------------------------------

/// Every live or recoverable record (the recovery catalog, `recover_unlinked`), keyed by
/// pathname and info address, decoded through `record_at` (independent of lookups).
fn recoverable(f: &Path) -> BTreeMap<(Vec<u8>, i64), String> {
    let mut file = Dss7File::open(f).unwrap();
    let opts = dss_core::dss7::CatalogOptions {
        recover_unlinked: true,
        ..Default::default()
    };
    let cat = file.catalog_with(&opts).unwrap();
    let mut out = BTreeMap::new();
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        let m = cat.meta[i];
        if !matches!(m.raw_status, 0..=2) {
            continue;
        }
        let Ok(r) = file.record_at(&cat, i) else {
            continue;
        };
        let d = match e.record_type {
            102 | 107 | 112 | 117 => file
                .read_ts_profile_record(&r)
                .map(|x| format!("{x:?}"))
                .ok(),
            100..=119 => file.read_ts_record(&r).map(|x| format!("{x:?}")).ok(),
            200..=209 => file.read_pd_record(&r).map(|x| format!("{:?}", x.pd)).ok(),
            400..=499 => file.read_grid_record(&r).map(|x| format!("{x:?}")).ok(),
            rt => Some(format!("record type {rt}")),
        };
        if let Some(d) = d {
            out.insert((e.pathname.clone(), m.info_address), d);
        }
    }
    out
}

/// The words of every entry the recovery catalog lists (bin entry regions).
fn entry_words(f: &Path) -> BTreeMap<i64, i64> {
    let mut file = Dss7File::open(f).unwrap();
    let opts = dss_core::dss7::CatalogOptions {
        recover_unlinked: true,
        ..Default::default()
    };
    let cat = file.catalog_with(&opts).unwrap();
    let mut out = BTreeMap::new();
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        let m = cat.meta[i];
        let a = m.bin_address + i64::from(m.entry_offset);
        let n = 7 + e.pathname.len().div_ceil(8) as i64;
        for w in a..a + n {
            out.insert(w, file_word(f, w));
        }
    }
    out
}

/// A real killed F1 write (positive marker), then only
/// `kaddNextEmptyBin` moved into the zero tail of the first bin, so a bin-sized write
/// there would reach the next bin's live entry. Repair and the next write refuse with an
/// error and change nothing; every record still reads.
#[test]
fn cursor_inside_a_bin_tail_is_refused() {
    let d = tmp("r5");
    let f = d.join("file.dss");
    make_base("rts7_new", &f);
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    assert!(hword(&f, PENDING_WORD) > 0);
    let known = listed(7, &f);
    let want = snapshot(7, &f, &known);
    let bin = Dss7File::open(&f).unwrap().catalog().unwrap().meta[0].bin_address;
    assert_eq!(file_word(&f, bin + 100), 0);
    set_word(&f, NEXT_EMPTY_BIN as i64, bin + 100);
    assert_eq!(snapshot(7, &f, &known), want, "input records readable");
    let before = std::fs::read(&f).unwrap();
    // Round 6: the copy-out repair never writes the source.
    copy_out_preserves(&f, "R5 cursor");
    let w = paired(
        7,
        &f,
        b"/T/OTHER/STAGE-FLOW///S/",
        1.0,
        &WriteOptions::default(),
    );
    assert!(matches!(w, Err(WriteError::Corrupt(_))), "{w:?}");
    assert_eq!(std::fs::read(&f).unwrap(), before, "write wrote");
    assert_eq!(snapshot(7, &f, &known), want);
    let _ = std::fs::remove_dir_all(&d);
}

/// Minimal deterministic generator for the property test.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: i64) -> i64 {
        (self.next() % n.max(1) as u64) as i64
    }
}

/// Property: with a corrupted bin allocation cursor or bin-block chain pointer, a repair
/// (`repair_dss7`) and a new-record write either refuse or succeed, and in either case
/// no live or recoverable record's payload changes and no word of any listed or
/// recoverable bin entry changes. Starting states: a real F1-killed write (positive
/// marker), a legacy hole file, a real interrupted delete on a legacy file, and a clean
/// file.
#[test]
fn corrupted_cursor_and_chain_never_change_records() {
    let d = tmp("r5_fuzz");
    let mut starts: Vec<(String, PathBuf)> = Vec::new();
    let f1 = d.join("f1.dss");
    make_base("rts7_new", &f1);
    assert!(matches!(
        kill_at("rts7_new", &f1, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f1);
    starts.push(("f1".into(), f1));
    let src = d.join("legacy_src");
    std::fs::create_dir_all(&src).unwrap();
    starts.push(("legacy".into(), legacy_damaged(&src)));
    let gsrc = d.join("gap_src");
    std::fs::create_dir_all(&gsrc).unwrap();
    let gap = legacy_with_gap(&gsrc);
    let del = d.join("del.dss");
    for at in 0.. {
        std::fs::copy(&gap, &del).unwrap();
        match kill_at("delete7", &del, at, false) {
            Outcome::Done(_) => panic!("no delete marker"),
            Outcome::Killed(_) => {
                open_after_kill(&del);
                if hword(&del, PENDING_WORD) < 0 {
                    break;
                }
            }
        }
    }
    starts.push(("delete".into(), del));
    let clean = d.join("clean.dss");
    make_base("pd7_new", &clean);
    starts.push(("clean".into(), clean));

    let mut rng = Lcg(0x5065_5235);
    let (mut runs, mut refused, mut succeeded) = (0, 0, 0);
    for (name, start) in &starts {
        let w = Dss7File::open(start).unwrap().header().words;
        let (bs, bpb, first, fsize) = (w[23], w[21], w[24], w[5]);
        let chain_word = first + bs * bpb;
        for i in 0..60 {
            let g = d.join(format!("{name}_{i}.dss"));
            std::fs::copy(start, &g).unwrap();
            let kind = i % 6;
            match kind {
                // Cursor anywhere in the file.
                0 => set_word(&g, NEXT_EMPTY_BIN as i64, rng.below(fsize + 50)),
                // Cursor inside a bin: a bin start plus 1..bs-1.
                1 => {
                    let b = first + rng.below(bpb) * bs + 1 + rng.below(bs - 1);
                    set_word(&g, NEXT_EMPTY_BIN as i64, b)
                }
                // Remaining count inconsistent or out of range.
                2 => set_word(&g, BINS_REMAIN_IN_BLOCK as i64, rng.below(bpb + 5) - 2),
                // First block's chain word into a bin, past the file, or a loop.
                3 => {
                    let v = match rng.below(3) {
                        0 => first + rng.below(bpb * bs),
                        1 => fsize + rng.below(1000),
                        _ => first,
                    };
                    set_word(&g, chain_word, v)
                }
                // First bin address moved.
                4 => set_word(&g, 24, first + 1 + rng.below(bs * 2)),
                // Cursor and remaining both random.
                _ => {
                    set_word(&g, NEXT_EMPTY_BIN as i64, first + rng.below(bpb * bs + 1));
                    set_word(&g, BINS_REMAIN_IN_BLOCK as i64, rng.below(bpb + 1));
                }
            }
            let recs = recoverable(&g);
            let words = entry_words(&g);
            for op in ["repair", "write"] {
                runs += 1;
                let r = match op {
                    "repair" => {
                        copy_out_preserves(&g, "R5 property");
                        Ok(())
                    }
                    _ => paired(
                        7,
                        &g,
                        format!("/Z/FUZZ{i}/STAGE-FLOW///S/").as_bytes(),
                        2.0,
                        &mode(false),
                    ),
                };
                if r.is_ok() {
                    succeeded += 1;
                } else {
                    refused += 1;
                }
                let what = format!("{name} mutation {i} (kind {kind}) {op}: {r:?}");
                let after = recoverable(&g);
                for (k, v) in &recs {
                    assert_eq!(
                        after.get(k),
                        Some(v),
                        "{what}: record {:?}",
                        String::from_utf8_lossy(&k.0)
                    );
                }
                for (a, v) in &words {
                    assert_eq!(file_word(&g, *a), *v, "{what}: entry word {a}");
                }
            }
            let _ = std::fs::remove_file(&g);
        }
    }
    eprintln!("R5 property: {runs} operations, {refused} refused, {succeeded} succeeded");
    assert!(refused > 0 && succeeded > 0);
    let _ = std::fs::remove_dir_all(&d);
}

/// The heclib cursor rule the repair writes now require (dss7.md §6.4): `kaddNextEmptyBin`
/// is `block + (kbinsPerBlock − remaining) · kbinSize` for a block of the bin-block chain,
/// `0 ≤ remaining ≤ kbinsPerBlock`. Independent of the engine's implementation.
fn cursor_rule_holds(f: &Path) -> Result<(), String> {
    let file = Dss7File::open(f).map_err(|e| format!("open: {e}"))?;
    let w = file.header().words;
    drop(file);
    let (bs, bpb, next, remain, fsize) = (w[23], w[21], w[25], w[22], w[5]);
    if !(0..=bpb).contains(&remain) {
        return Err(format!("remaining {remain}"));
    }
    let len_words = (std::fs::metadata(f).unwrap().len() / 8) as i64;
    let mut block = w[24];
    let mut n = 0;
    while block != 0 && n < 1_000_000 {
        n += 1;
        if block + bs * bpb >= len_words.min(fsize + 1) {
            return Err(format!("block {block} past the file"));
        }
        if next == block + (bpb - remain) * bs {
            return Ok(());
        }
        block = file_word(f, block + bs * bpb);
    }
    Err(format!(
        "cursor {next} (remaining {remain}) not a bin boundary"
    ))
}

/// Blocks of the bin-block chain (bounded).
fn chain_blocks(f: &Path) -> i64 {
    let w = Dss7File::open(f).unwrap().header().words;
    let len_words = (std::fs::metadata(f).unwrap().len() / 8) as i64;
    let (mut block, mut n) = (w[24], 0);
    while block != 0 && block + w[23] * w[21] < len_words && n < 1_000_000 {
        n += 1;
        block = file_word(f, block + w[23] * w[21]);
    }
    n
}

/// Every DSS7 file under `fixtures/public` (and, with `DSS_CORPUS_LIST`, every DSS7 file
/// it lists; files are only read) satisfies the cursor rule, so the cursor refusal never
/// triggers on a file HEC or this engine wrote.
#[test]
fn cursor_rule_holds_on_real_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dss")))
        .collect();
    if let Ok(list) = std::env::var("DSS_CORPUS_LIST") {
        for l in std::fs::read_to_string(list).unwrap().lines() {
            if !l.trim().is_empty() {
                files.push(PathBuf::from(l.trim()));
            }
        }
    }
    let (mut checked, mut no_state, mut broken, mut bad) = (0, 0, 0, Vec::new());
    for f in files {
        // DSS6 files, and deliberately damaged fixtures, do not open as DSS7.
        if Dss7File::open(&f).is_err() {
            continue;
        }
        checked += 1;
        if let Err(e) = cursor_rule_holds(&f) {
            let w = Dss7File::open(&f).unwrap().header().words;
            if w[25] == 0 && w[22] == 0 {
                // Hand-built fixtures without bin allocation state.
                no_state += 1;
            } else if chain_blocks(&f) * w[21] < w[26] {
                // A damaged file whose bin-block chain ends before the block of
                // ktotalBins: the cursor's block is unreachable. Refusing is correct.
                broken += 1;
            } else {
                bad.push(format!("{}: {e}", f.file_name().unwrap().to_string_lossy()));
            }
        }
    }
    eprintln!(
        "cursor rule: {checked} DSS7 files checked; {no_state} without allocation state, \
         {broken} with a broken bin-block chain, {} other failures",
        bad.len()
    );
    assert!(checked > 5, "{checked}");
    assert!(bad.is_empty(), "{bad:?}");
}

/// A base file plus one large paired-data record (20,000 ordinates, values zero except
/// `nonzero`): returns the record's pathname and the word address of its values2 area.
fn base_with_large_pd(f: &Path, nonzero: Option<usize>) -> (&'static [u8], i64) {
    make_base("rts7_new", f);
    let ord: Vec<f32> = (0..20000).map(|i| i as f32).collect();
    let mut vals = vec![0.0f32; 20000];
    if let Some(i) = nonzero {
        vals[i] = 123.0;
    }
    let p: &'static [u8] = b"/REVIEW/ZERO/STAGE-FLOW///S/";
    let input = PdInput::new(
        p,
        PdArrays::F32 {
            ordinates: &ord,
            values: &vals,
        },
        1,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    write_pd(f, &input, &mode(false)).unwrap();
    let mut r = Dss7File::open(f).unwrap();
    let info = r.lookup(p).unwrap().unwrap().info_address;
    drop(r);
    (p, file_word(f, info + 21))
}

fn pd_payload(f: &Path, p: &[u8]) -> String {
    let mut r = Dss7File::open(f).unwrap();
    let rec = r.lookup(p).unwrap().unwrap();
    format!("{:?}", r.read_pd_record(&rec).unwrap().pd)
}

/// A real F1 kill, then the first block's chain word and the cursor moved into
/// a live record's values area. In place, the reconciling write refuses (the cursor bin
/// is not this write's own leftover) and changes no byte; the copy-out repair leaves the
/// file byte-identical and copies the record with its values.
#[test]
fn chain_pointer_into_live_data_is_refused() {
    let d = tmp("r6");
    let f = d.join("file.dss");
    let (p, values2) = base_with_large_pd(&f, Some(250));
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let x = values2 + 100;
    let chain_word = hword(&f, 24) + hword(&f, 23) * hword(&f, 21);
    set_word(&f, chain_word, x);
    set_word(&f, NEXT_EMPTY_BIN as i64, x);
    set_word(&f, BINS_REMAIN_IN_BLOCK as i64, 32);
    let before = pd_payload(&f, p);
    let bytes = std::fs::read(&f).unwrap();
    let w = paired(7, &f, b"/Z/OTHER/STAGE-FLOW///S/", 2.0, &mode(false));
    assert!(matches!(&w, Err(WriteError::Corrupt(_))), "{w:?}");
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "nothing written");
    assert_eq!(pd_payload(&f, p), before);
    copy_out_ok(&f, "R6");
    let _ = std::fs::remove_dir_all(&d);
}

/// The allocation-limitation file: no marker, a large all-zero record, and only the cursor moved into its
/// values area (bin allocation metadata damaged; the bin-block chain itself is intact).
fn r7_file(d: &Path) -> (PathBuf, &'static [u8]) {
    let f = d.join("file.dss");
    let (p, values2) = base_with_large_pd(&f, None);
    set_word(&f, NEXT_EMPTY_BIN as i64, values2 + 100);
    set_word(&f, BINS_REMAIN_IN_BLOCK as i64, 32);
    (f, p)
}

/// Documented limitation (dss7.md §6.4, damaged allocation metadata): a normal write does not build the allocation map,
/// as on main, so on a file whose cursor already points into zero-valued data the new
/// bin is published over that data. heclib's own allocator (`zbinNew()`) checks less. This
/// pins the behaviour; a future fix must update it.
#[test]
fn r7_limitation_normal_write_on_damaged_cursor_is_pinned() {
    let d = tmp("r7");
    let (f, p) = r7_file(&d);
    let before = pd_payload(&f, p);
    let r = paired(7, &f, b"/REVIEW/OTHER/STAGE-FLOW///S/", 2.0, &mode(false));
    assert!(r.is_ok(), "{r:?}");
    assert_ne!(
        pd_payload(&f, p),
        before,
        "R7: the zero-valued data is overwritten"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// The copy-out repair of the allocation-limitation file leaves it byte-identical and copies its records,
/// the zero-valued data included, into a clean new file.
#[test]
fn copy_out_of_the_r7_file_leaves_it_unchanged() {
    let d = tmp("r7_repair");
    let (f, p) = r7_file(&d);
    let before = pd_payload(&f, p);
    copy_out_ok(&f, "R7");
    assert_eq!(pd_payload(&f, p), before);
    let _ = std::fs::remove_dir_all(&d);
}

/// The property test extended to damaged allocation metadata: chain pointers, first-bin
/// addresses and cursors into data extents, on files with a large zero-valued (or
/// one-nonzero) record, clean and with a real F1 marker: the copy-out repair never
/// changes the file and copies every readable record its discovery reaches.
#[test]
fn pointers_into_data_extents_never_change_records() {
    let d = tmp("r67_fuzz");
    let mut rng = Lcg(0x5236_3700);
    let (mut runs, mut copied) = (0, 0);
    for (si, nonzero) in [None, Some(250usize)].into_iter().enumerate() {
        let base = d.join(format!("base{si}.dss"));
        let (_, values2) = base_with_large_pd(&base, nonzero);
        let marker = d.join(format!("marker{si}.dss"));
        std::fs::copy(&base, &marker).unwrap();
        assert!(matches!(
            kill_at("rts7_new", &marker, RTS7_NEW_F1, false),
            Outcome::Killed(_)
        ));
        open_after_kill(&marker);
        for (name, start) in [("clean", &base), ("marker", &marker)] {
            let w = Dss7File::open(start).unwrap().header().words;
            let chain_word = w[24] + w[23] * w[21];
            for i in 0..24 {
                let g = d.join(format!("{name}{si}_{i}.dss"));
                std::fs::copy(start, &g).unwrap();
                let x = values2 + rng.below(9000);
                match i % 4 {
                    0 => set_word(&g, NEXT_EMPTY_BIN as i64, x),
                    1 => {
                        set_word(&g, chain_word, x);
                        set_word(&g, NEXT_EMPTY_BIN as i64, x);
                        set_word(&g, BINS_REMAIN_IN_BLOCK as i64, 32);
                    }
                    2 => set_word(&g, 24, x),
                    _ => {
                        set_word(&g, NEXT_EMPTY_BIN as i64, x - (x - values2) % 200);
                        set_word(&g, BINS_REMAIN_IN_BLOCK as i64, 1 + rng.below(32));
                    }
                }
                runs += 1;
                if copy_out_preserves(&g, &format!("{name}{si} mutation {i}")) {
                    copied += 1;
                }
                let _ = std::fs::remove_file(&g);
            }
        }
    }
    eprintln!("R6/R7 property: {runs} copy-outs, {copied} completed, src never changed");
    let _ = std::fs::remove_dir_all(&d);
}

/// The PD record's bin entry moved one word inside its bin with its hash slot
/// following it (still readable by pathname), then the chain word and cursor moved into
/// its values area. In place the reconciling write refuses and changes nothing; the
/// copy-out repair leaves the file byte-identical and copies the record, values intact.
#[test]
fn interior_hash_target_is_refused() {
    let d = tmp("r8");
    let f = d.join("file.dss");
    let (p, values2) = base_with_large_pd(&f, Some(250));
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let mut r = Dss7File::open(&f).unwrap();
    let bin = r.lookup(p).unwrap().unwrap().bin_entry_address;
    let (ht, mh) = (r.header().hash_table_address, r.header().max_hash);
    drop(r);
    let bs = hword(&f, 23);
    let old: Vec<i64> = (0..bs - 2).map(|i| file_word(&f, bin + i)).collect();
    for (i, w) in old.iter().enumerate() {
        set_word(&f, bin + 1 + i as i64, *w);
    }
    set_word(&f, bin, 0);
    set_word(&f, ht + table_hash(p, mh), bin + 1);
    let x = values2 + 100;
    set_word(&f, hword(&f, 24) + hword(&f, 21) * bs, x);
    set_word(&f, NEXT_EMPTY_BIN as i64, x);
    set_word(&f, BINS_REMAIN_IN_BLOCK as i64, 32);
    let before = pd_payload(&f, p);
    let bytes = std::fs::read(&f).unwrap();
    let w = paired(7, &f, b"/Z/OTHER/STAGE-FLOW///S/", 2.0, &mode(false));
    assert!(matches!(&w, Err(WriteError::Corrupt(_))), "{w:?}");
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "nothing written");
    assert_eq!(pd_payload(&f, p), before);
    copy_out_ok(&f, "R8");
    let _ = std::fs::remove_dir_all(&d);
}

/// The PD entry turned into an alias (status 2) that points straight at its
/// info block (`zaliasAdd()` lines 185-189), then the chain word and cursor moved into
/// its values area. In place the reconciling write refuses and changes nothing; the
/// copy-out repair leaves the file byte-identical and copies what a reader can read.
#[test]
fn alias_reachable_data_is_never_written() {
    let d = tmp("r10");
    let f = d.join("file.dss");
    let (p, values2) = base_with_large_pd(&f, Some(250));
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let bin = Dss7File::open(&f)
        .unwrap()
        .lookup(p)
        .unwrap()
        .unwrap()
        .bin_entry_address;
    set_word(&f, bin + 1, 2);
    set_word(&f, 4, 1);
    let x = values2 + 100;
    let bs = hword(&f, 23);
    set_word(&f, hword(&f, 24) + hword(&f, 21) * bs, x);
    set_word(&f, NEXT_EMPTY_BIN as i64, x);
    set_word(&f, BINS_REMAIN_IN_BLOCK as i64, 32);
    let before = pd_payload(&f, p);
    let bytes = std::fs::read(&f).unwrap();
    let w = paired(7, &f, b"/Z/OTHER/STAGE-FLOW///S/", 2.0, &mode(false));
    assert!(matches!(&w, Err(WriteError::Corrupt(_))), "{w:?}");
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "nothing written");
    assert_eq!(pd_payload(&f, p), before);
    let r = copy_out_preserves(&f, "R10");
    assert!(r, "copy-out of the R10 file");
    let _ = std::fs::remove_dir_all(&d);
}

/// A second reclaim table chained from the first, and a bin block and the
/// cursor placed on it. In place the reconciling write refuses and changes nothing;
/// the copy-out repair leaves the file byte-identical.
#[test]
fn chained_reclaim_table_is_never_written() {
    let d = tmp("r11");
    let f = d.join("file.dss");
    base_with_large_pd(&f, Some(250));
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let (x, bs, bpb) = (hword(&f, 5), hword(&f, 23), hword(&f, 21));
    let bw = bs * bpb + 1;
    for i in 0..bw {
        set_word(&f, x + i, 0);
    }
    set_word(&f, x + bw, -97532);
    set_word(&f, 5, x + bw);
    let (rt, rs) = (hword(&f, 42), hword(&f, 48));
    set_word(&f, rt + rs - 1, x);
    set_word(&f, 43, x);
    set_word(&f, x + 24, 200);
    set_word(&f, x + 25, 50000);
    set_word(&f, hword(&f, 24) + bs * bpb, x);
    set_word(&f, NEXT_EMPTY_BIN as i64, x);
    set_word(&f, BINS_REMAIN_IN_BLOCK as i64, bpb);
    let bytes = std::fs::read(&f).unwrap();
    let w = paired(7, &f, b"/Z/OTHER/STAGE-FLOW///S/", 2.0, &mode(false));
    assert!(matches!(&w, Err(WriteError::Corrupt(_))), "{w:?}");
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "nothing written");
    assert_eq!(file_word(&f, x + 25), 50000);
    copy_out_preserves(&f, "R11");
    let _ = std::fs::remove_dir_all(&d);
}

/// A chained bin block inside the lock arrays. In place, a write refuses to
/// hand out a bin in the permanent section and changes nothing; the copy-out repair
/// leaves the file byte-identical.
#[test]
fn chain_block_in_lock_area_is_refused() {
    let d = tmp("r9");
    let f = d.join("file.dss");
    create_dss7(&f).unwrap();
    let first = hword(&f, 24);
    set_word(&f, 23, 20);
    set_word(&f, 21, 1);
    set_word(&f, first + 20, 103);
    set_word(&f, NEXT_EMPTY_BIN as i64, 103);
    set_word(&f, BINS_REMAIN_IN_BLOCK as i64, 1);
    let bytes = std::fs::read(&f).unwrap();
    let w = paired(7, &f, b"/Z/OTHER/STAGE-FLOW///S/", 2.0, &mode(false));
    assert!(matches!(&w, Err(WriteError::Corrupt(_))), "{w:?}");
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "nothing written");
    copy_out_preserves(&f, "R9");
    let _ = std::fs::remove_dir_all(&d);
}

/// Hidden-hole repair no longer exists in place. A hole whose info
/// block is active is left byte for byte by a reconciling write; the copy-out repair
/// leaves the file unchanged and copies what is readable.
#[test]
fn legacy_hole_with_activated_info_is_left_alone() {
    let d = tmp("legacy_active");
    let f = legacy_damaged(&d);
    let hole = legacy_hole(&f);
    let info = file_word(&f, hole + 3);
    assert_eq!(file_word(&f, info + INFO_STATUS), 10);
    set_word(&f, info + INFO_STATUS, 1);
    copy_out_preserves(&f, "R1 active info");
    let hole_before = bin_words(&f, hole);
    set_word(&f, PENDING_WORD as i64, info);
    let _ = run_write("reconcile7", &f, &WriteOptions::default());
    assert_eq!(bin_words(&f, hole), hole_before);
    assert!(hidden_count(&f).0 >= 1);
    let _ = std::fs::remove_dir_all(&d);
}

/// A hole whose bin status an earlier interrupted repair set to 11: copy-out leaves it
/// unchanged and copies every readable record its discovery reaches.
#[test]
fn legacy_hole_with_interrupted_repair_status_is_completed() {
    let d = tmp("legacy_st11");
    let f = legacy_damaged(&d);
    let hole = legacy_hole(&f);
    set_word(&f, hole + 1, 11);
    let r = copy_out_ok(&f, "status-11 hole");
    assert!(r.hidden_records_copied >= 1);
    let _ = std::fs::remove_dir_all(&d);
}

/// The copy-out report counts the hidden records it brought back
/// and reports the source's interrupted-write marker; the source is not reconciled.
#[test]
fn repair_report_counts_holes_repaired_by_reconciliation() {
    let d = tmp("legacy_r3");
    let f = legacy_variant(&d, "marker");
    let r = copy_out_ok(&f, "marker variant");
    assert!(r.src_interrupted_write);
    assert_eq!(r.hidden_records_copied, 1);
    assert_ne!(hword(&f, PENDING_WORD), 0, "src reconciled");
    let _ = std::fs::remove_dir_all(&d);
}

/// In-place hole publication no longer exists, so its restart window
/// is unreachable. On the sorted-list variant a reconciling write leaves the hole and its
/// words as they are; the copy-out repair leaves the file unchanged.
#[test]
fn legacy_repair_header_is_final_before_the_hash() {
    let d = tmp("legacy_r2");
    let f = legacy_variant(&d, "sorted");
    copy_out_ok(&f, "sorted variant");
    let hole = legacy_hole(&f);
    let hole_before = bin_words(&f, hole);
    run_write("reconcile7", &f, &WriteOptions::default()).unwrap();
    assert_eq!(bin_words(&f, hole), hole_before);
    let _ = std::fs::remove_dir_all(&d);
}

/// The bin a killed F1 write left is reused by a different, shorter entry: the whole bin
/// is rewritten, so no tail of the killed entry's body survives as a phantom entry.
#[test]
fn f1_bin_reused_by_a_shorter_entry() {
    let d = tmp("f1_short");
    let case = "rts7_new";
    let f = d.join("f.dss");
    make_base(case, &f);
    assert!(matches!(
        kill_at(case, &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    assert!(f1_state(&f));
    let cursor = hword(&f, NEXT_EMPTY_BIN);
    let short = b"/A/B/C///F/";
    retry_locked("short", || {
        paired(7, &f, short, 5.0, &WriteOptions::default())
    });
    let mut want: BTreeSet<Vec<u8>> = listed(7, &f).into_keys().collect();
    want.insert(short.to_vec());
    assert_clean(7, &f, "short entry", Some(&want));
    // One entry in the reused bin, then zeros to the end of the bin.
    let b = std::fs::read(&f).unwrap();
    let word =
        |a: i64| i64::from_le_bytes(b[a as usize * 8..a as usize * 8 + 8].try_into().unwrap());
    let pw = short.len().div_ceil(8) as i64;
    assert_ne!(word(cursor), 0);
    assert!((cursor + 7 + pw..cursor + 200).all(|a| word(a) == 0));
    // The killed record was never completed: it stays absent.
    let mut file = Dss7File::open(&f).unwrap();
    assert!(file
        .lookup(b"/T/NEW/FLOW/01Mar2021/1Hour/S/")
        .unwrap()
        .is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// Round 6: copy-out of every public DSS7 fixture. The source stays byte-identical, the
/// copy publishes, and every record readable in the source that was copied decodes
/// identically in the copy (the copy's own verification also compares decoded records).
#[test]
fn copy_out_of_every_public_fixture() {
    let d = tmp("copyout_fixtures");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    let mut n = 0;
    for e in std::fs::read_dir(&root).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| !x.eq_ignore_ascii_case("dss"))
            || Dss7File::open(&p).is_err()
        {
            continue;
        }
        let src = d.join(p.file_name().unwrap());
        std::fs::copy(&p, &src).unwrap();
        let bytes = std::fs::read(&src).unwrap();
        let dest = copy_dest(&src);
        let r = repair_dss7(&src, &dest).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        assert_eq!(std::fs::read(&src).unwrap(), bytes, "{p:?}");
        let skipped: BTreeSet<Vec<u8>> = r
            .not_copied
            .iter()
            .map(|(q, _)| q.to_ascii_uppercase())
            .collect();
        let (want, got) = (readable(&src), readable(&dest));
        for (k, v) in &want {
            if !skipped.contains(k) {
                assert_eq!(
                    got.get(k),
                    Some(v),
                    "{p:?}: {:?}",
                    String::from_utf8_lossy(k)
                );
            }
        }
        // heclib's zcheckInfo() reads 18 ints at every type-1xx record's internal-header
        // address, whatever its length; that read must stay inside the file.
        let mut file = Dss7File::open(&dest).unwrap();
        let size = file.header().words[5];
        let cat = file.catalog_with(&Default::default()).unwrap();
        for i in 0..cat.catalog.entries.len() {
            let r = file.record_at(&cat, i).unwrap();
            if (100..200).contains(&r.record_type) {
                let ih = r.info.internal_header.address;
                assert!(
                    ih + 9 <= size,
                    "{p:?}: internal header at {ih}, file size {size}"
                );
            }
        }
        n += 1;
    }
    assert!(n > 20, "{n}");
    eprintln!("copy-out of {n} public DSS7 fixtures");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------------
// Threat model (dss7.md §6.4): a file that
// was consistent before an interrupted write is reconciled exactly after ordinary
// process death; extra, independent damage is refused when cheap bounded checks detect
// it, without a promise to detect every case. Damaged files are copied out.
// ---------------------------------------------------------------------------------------

/// A real interrupted `rts7_new` write (barrier 8: bin hash
/// published, bin not linked), then only the pending pathname's hash-table slot set to a
/// zero-filled word of an unrelated, intact PD record. Roll-forward would have written
/// its link into the PD's values. Now refused (dss7.md §6.4, roll-forward link
/// targets): nothing is written.
#[test]
fn r12a_roll_forward_link_into_a_live_record_is_refused() {
    let d = tmp("r12a");
    let f = d.join("file.dss");
    let (p, values2) = base_with_large_pd(&f, Some(250));
    let before = pd_payload(&f, p);
    assert!(matches!(
        kill_at("rts7_new", &f, 8, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let cursor = hword(&f, NEXT_EMPTY_BIN);
    let pending = hword(&f, PENDING_WORD);
    let path = b"/T/NEW/FLOW/01Mar2021/1Hour/S/";
    let slot = hword(&f, 19) + table_hash(path, hword(&f, 15));
    assert_eq!(file_word(&f, slot), 0);
    assert_eq!(file_word(&f, cursor + 3), pending);
    let fake = values2 + 100;
    set_word(&f, slot, fake);
    assert_eq!(file_word(&f, fake + hword(&f, 23) - 1), 0);
    assert_eq!(before, pd_payload(&f, p), "the PD still reads identically");
    let bytes = std::fs::read(&f).unwrap();
    let w = hourly(
        7,
        &f,
        b"/K/KEEP/FLOW//1HOUR/S/",
        t(2021, 1, 1, 3600),
        &[88.0],
        &mode(false),
    );
    assert!(
        matches!(&w, Err(WriteError::Corrupt(m)) if m.contains("hash chain")),
        "{w:?}"
    );
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "nothing written");
    assert_eq!(pd_payload(&f, p), before);
    assert!(
        copy_out_preserves(&f, "R12 A"),
        "copy-out of the R12 A file"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// An authentic F1 leftover whose words 4, 5 and 6 (type and catalog
/// sequence, write time, first/last days) were changed is not this write's own entry:
/// refused, nothing written.
#[test]
fn r12b_leftover_with_foreign_entry_words_is_refused() {
    for word in 4..=6 {
        let d = tmp(&format!("r12b_{word}"));
        let f = d.join("file.dss");
        make_base("rts7_new", &f);
        assert!(matches!(
            kill_at("rts7_new", &f, RTS7_NEW_F1, false),
            Outcome::Killed(_)
        ));
        open_after_kill(&f);
        let cursor = hword(&f, NEXT_EMPTY_BIN);
        set_word(&f, cursor + word, file_word(&f, cursor + word) ^ 0x10);
        let bytes = std::fs::read(&f).unwrap();
        let w = paired(7, &f, b"/T/PD/STAGE-FLOW///S/", 3.0, &mode(false));
        assert!(
            matches!(&w, Err(WriteError::Corrupt(m)) if m.contains("4-6")),
            "word {word}: {w:?}"
        );
        assert_eq!(
            std::fs::read(&f).unwrap(),
            bytes,
            "word {word}: nothing written"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The documented shared-leftover limitation (dss7.md §6.4). An authentic, exactly matching
/// F1 leftover at a valid cursor, and an unrelated live PD record whose values2 pointer
/// was damaged to name that same bin. No cheap bounded check can tell that another
/// record's pointer names the leftover (that needs a whole-file ownership map, which
/// the in-place path deliberately does not build). Reconciliation clears its own
/// leftover, and the damaged PD's values (which were the leftover's words) read as
/// zeros. This test pins that behaviour; copy-out is the safe path for such a file.
#[test]
fn r12_limitation_shared_leftover_is_pinned() {
    let d = tmp("r12_limitation");
    let f = d.join("file.dss");
    make_base("rts7_new", &f);
    assert!(matches!(
        kill_at("rts7_new", &f, RTS7_NEW_F1, false),
        Outcome::Killed(_)
    ));
    open_after_kill(&f);
    let cursor = hword(&f, NEXT_EMPTY_BIN);
    let p = b"/K/PD/STAGE-FLOW///S/";
    let ia = Dss7File::open(&f)
        .unwrap()
        .lookup(p)
        .unwrap()
        .unwrap()
        .info_address;
    set_word(&f, ia + 21, cursor);
    let before = pd_payload(&f, p);
    // Copy-out first: the safe path keeps the PD exactly as the damaged file shows it.
    assert!(
        copy_out_preserves(&f, "R12 B"),
        "copy-out of the R12 B file"
    );
    let w = hourly(
        7,
        &f,
        b"/K/KEEP/FLOW//1HOUR/S/",
        t(2021, 1, 1, 3600),
        &[88.0],
        &mode(false),
    );
    assert!(w.is_ok(), "pinned: the in-place write succeeds: {w:?}");
    let after = pd_payload(&f, p);
    assert_ne!(after, before, "pinned: the shared words were cleared");
    assert!(
        (1..10).all(|i| file_word(&f, cursor + i) == 0),
        "pinned: the leftover body is zeroed"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// The only entry of a readable PD record turned into an alias (status 2)
/// of its own info block. Copy-out copies the record under its primary pathname and
/// says so; when the alias's primary cannot be read, the alias is reported as not
/// copied and the report is not complete.
#[test]
fn r14_alias_only_record_is_copied_or_reported() {
    let d = tmp("r14");
    let f = d.join("file.dss");
    make_base("rts7_new", &f);
    let p = b"/K/PD/STAGE-FLOW///S/";
    let r = Dss7File::open(&f).unwrap().lookup(p).unwrap().unwrap();
    set_word(&f, r.bin_entry_address + 1, 2);
    set_word(&f, 4, 1);
    let before = pd_payload(&f, p);
    let dest = d.join("repaired.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert!(rep.complete(), "{rep:?}");
    assert_eq!(
        pd_payload(&dest, p),
        before,
        "the alias-only record was copied"
    );
    assert!(
        rep.fallback_copies
            .iter()
            .any(|(q, a, why)| q == p && *a == r.info_address && why.contains("alias")),
        "{rep:?}"
    );
    assert!(copy_out_preserves(&f, "R14"));
    // Unreadable primary: the alias is reported, the copy is not complete.
    set_word(&f, r.info_address, 0);
    let dest2 = d.join("repaired2.dss");
    let rep = repair_dss7(&f, &dest2).unwrap();
    assert!(!rep.complete(), "{rep:?}");
    assert!(
        rep.not_copied
            .iter()
            .any(|(q, why)| q == p && why.starts_with("alias")),
        "{rep:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// The hash lookup reaches a copy of DUP whose values1 address is beyond the
/// end of the file; an intact catalog duplicate exists. Copy-out copies the intact one
/// and reports which.
#[test]
fn r15_unreadable_lookup_copy_falls_back_to_an_intact_duplicate() {
    let d = tmp("r15");
    let f = d.join("file.dss");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/synthetic_recovery7.dss");
    std::fs::copy(fixture, &f).unwrap();
    let p = b"/RECOVERY/DUP/FLOW/TS-PATTERN/1HOUR/SYN/";
    let mut rd = Dss7File::open(&f).unwrap();
    let cat = rd
        .catalog_with(&dss_core::dss7::CatalogOptions {
            recover_unlinked: true,
            ..Default::default()
        })
        .unwrap();
    let bad = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == p)
        .unwrap();
    let good = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == p.to_ascii_lowercase())
        .unwrap();
    let goodref = rd.record_at(&cat, good).unwrap();
    let want = rd.read_ts_record(&goodref).unwrap();
    let badinfo = cat.meta[bad].info_address;
    drop(rd);
    let n = 30 + p.len().div_ceil(8);
    for j in 0..n as i64 {
        set_word(&f, badinfo + j, file_word(&f, goodref.info_address + j));
    }
    set_word(&f, badinfo + 19, hword(&f, 5) + 100);
    let mut rd = Dss7File::open(&f).unwrap();
    let bypath = rd.lookup(p).unwrap().unwrap();
    assert!(rd.read_ts_record(&bypath).is_err());
    drop(rd);
    let bytes = std::fs::read(&f).unwrap();
    let dest = d.join("repaired.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), bytes);
    let mut rd = Dss7File::open(&dest).unwrap();
    let got = rd.lookup(p).unwrap().expect("DUP copied");
    assert_eq!(
        format!("{:?}", rd.read_ts_record(&got).unwrap()).to_ascii_uppercase(),
        format!("{want:?}").to_ascii_uppercase()
    );
    assert!(
        rep.fallback_copies
            .iter()
            .any(|(q, a, _)| q.eq_ignore_ascii_case(p) && *a == goodref.info_address),
        "{rep:?}"
    );
    assert!(copy_out_preserves(&f, "R15"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Files in `d` other than `keep`.
fn others(d: &Path, keep: &[&Path]) -> Vec<String> {
    std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| !keep.contains(&p.as_path()))
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect()
}

/// An error injected at every step of copy-out leaves `src` unchanged, no
/// `dest`, and no temporary file. When the hard link fails (as on a
/// filesystem without hard links), publication never replaces a file: with an atomic
/// no-replace rename (Linux `renameat2`) it publishes or refuses an existing `dest`;
/// without one (Windows) it fails, keeping the verified copy and naming it.
#[test]
fn r16_r13_copy_out_faults_and_publication() {
    use dss_core::dss7::write::{set_copy_out_fault, COPY_OUT_FAULT_STEPS};
    let d = tmp("r16");
    let src = d.join("src.dss");
    make_base("rts7_new", &src);
    let bytes = std::fs::read(&src).unwrap();
    let dest = d.join("dest.dss");
    for &step in COPY_OUT_FAULT_STEPS {
        set_copy_out_fault(Some(step));
        let r = repair_dss7(&src, &dest);
        set_copy_out_fault(None);
        assert_eq!(std::fs::read(&src).unwrap(), bytes, "{step}: src changed");
        match step {
            "hard_link" if cfg!(target_os = "linux") => {
                assert!(r.is_ok(), "{step}: {r:?}");
                assert!(dest.exists());
                assert!(others(&d, &[&src, &dest]).is_empty(), "{step}");
                std::fs::remove_file(&dest).unwrap();
            }
            "hard_link" => {
                let e = r.expect_err(step).to_string();
                let left = others(&d, &[&src]);
                assert_eq!(left.len(), 1, "{step}: the verified copy is kept: {left:?}");
                assert!(e.contains(&left[0]) && e.contains("kept"), "{step}: {e}");
                assert!(!dest.exists(), "{step}");
                std::fs::remove_file(d.join(&left[0])).unwrap();
            }
            "hard_link_race" => {
                assert!(r.is_err(), "{step}: {r:?}");
                assert_eq!(
                    std::fs::read(&dest).unwrap(),
                    b"created by another process",
                    "{step}: never replaced"
                );
                std::fs::remove_file(&dest).unwrap();
                if cfg!(target_os = "linux") {
                    assert!(matches!(r, Err(WriteError::AlreadyExists(_))), "{step}");
                    assert!(others(&d, &[&src]).is_empty(), "{step}");
                } else {
                    for x in others(&d, &[&src]) {
                        assert!(x.ends_with(".partial"), "{step}: {x}");
                        std::fs::remove_file(d.join(x)).unwrap();
                    }
                }
            }
            _ => {
                assert!(r.is_err(), "{step}: {r:?}");
                assert!(!dest.exists(), "{step}: no dest");
                assert!(
                    others(&d, &[&src]).is_empty(),
                    "{step}: temporary file left: {:?}",
                    others(&d, &[&src])
                );
            }
        }
    }
    // And the normal path still publishes.
    assert!(repair_dss7(&src, &dest).is_ok());
    assert!(others(&d, &[&src, &dest]).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------------
// Replacement metadata, aliases and duplicates in copy-out.
// ---------------------------------------------------------------------------------------

/// The WIDTH record's bin type, info type, bin words 4-6 and values.
fn width_state(f: &Path) -> (i32, i32, [i64; 3], String) {
    let mut rd = Dss7File::open(f).unwrap();
    let r = rd.lookup(WIDE_RTS_D).unwrap().unwrap();
    let w = [
        file_word(f, r.bin_entry_address + 4),
        file_word(f, r.bin_entry_address + 5),
        file_word(f, r.bin_entry_address + 6),
    ];
    let vals = format!("{:?}", rd.read_ts_record(&r).map(|x| x.ts));
    (r.record_type, r.info.record_type, w, vals)
}

const WIDE_RTS_D: &[u8] = b"/T/WIDTH/FLOW/01MAR2021/1HOUR/S/";

/// The state an engine before 0.1.1 left when killed between replacement's pointer
/// write (bin +3) and its metadata write (+4..+6), built
/// deterministically (fault after Retire, then exactly the pointer write).
fn r17_old_engine_gap(f: &Path) {
    make_base("rts7_widen", f);
    let mut o = mode(false);
    o.fault = Some(FaultPoint {
        block: 0,
        step: WriteStep::Retire,
    });
    assert!(matches!(
        widen("rts7_widen", f, &o),
        Err(WriteError::InjectedFault { .. })
    ));
    let pending = hword(f, PENDING_WORD);
    assert!(pending > 0);
    let r = Dss7File::open(f)
        .unwrap()
        .lookup(WIDE_RTS_D)
        .unwrap()
        .unwrap();
    set_word(f, r.bin_entry_address + 3, pending);
    let (bin_t, info_t, _, _) = width_state(f);
    assert_eq!((bin_t, info_t), (100, 105), "the mismatched bin/info state");
}

/// R17 (dss7.md §6.4). Reconciliation completes the published
/// entry's words 4-6 from the info block that won publication, so the record is exactly
/// the uninterrupted write's (type 105, F64 values), and the original write can be
/// retried. A reconciliation killed at any barrier (every separate write included)
/// reaches the same state on the next write.
#[test]
fn r17_replacement_metadata_is_completed_by_reconciliation() {
    let d = tmp("r17");
    // The uninterrupted reference.
    let reference = d.join("ref.dss");
    make_base("rts7_widen", &reference);
    widen("rts7_widen", &reference, &mode(true)).unwrap();
    paired(7, &reference, b"/Z/RECON/STAGE-FLOW///S/", 6.0, &mode(true)).unwrap();
    let want = width_state(&reference);
    assert_eq!((want.0, want.1), (105, 105));
    // Reconciled by an unrelated write.
    let f = d.join("gap.dss");
    r17_old_engine_gap(&f);
    paired(7, &f, b"/Z/RECON/STAGE-FLOW///S/", 6.0, &mode(false)).unwrap();
    assert_eq!(hword(&f, PENDING_WORD), 0);
    let got = width_state(&f);
    assert_eq!((got.0, got.1), (105, 105), "bin and info types agree");
    assert_eq!(got.2[0] as u32, want.2[0] as u32, "word 4: record type");
    assert_eq!(got.2[2], want.2[2], "word 6: first/last days");
    assert_eq!(got.3, want.3, "values");
    assert_clean(7, &f, "R17 reconciled", None);
    // The original write can be retried.
    widen("rts7_widen", &f, &mode(true)).unwrap();
    assert_eq!(width_state(&f).3, want.3);
    // Restartable: the reconciling write killed at every barrier, then completed.
    for sync in [false, true] {
        let src = d.join(format!("gap_src_{sync}.dss"));
        r17_old_engine_gap(&src);
        let mut kills = 0;
        for at in 0.. {
            let g = d.join(format!("gap_{sync}_{at}.dss"));
            std::fs::copy(&src, &g).unwrap();
            match kill_at_fine("reconcile7", &g, at, sync) {
                Outcome::Done(_) => break,
                Outcome::Killed(_) => {
                    kills += 1;
                    open_after_kill(&g);
                    paired(7, &g, b"/Z/RECON/STAGE-FLOW///S/", 6.0, &mode(true)).unwrap();
                    let s = width_state(&g);
                    assert_eq!((s.0, s.1), (105, 105), "sync={sync} kill {at}");
                    assert_eq!(s.3, want.3, "sync={sync} kill {at}");
                    assert_clean(7, &g, &format!("R17 sync={sync} kill {at}"), None);
                }
            }
            let _ = std::fs::remove_file(&g);
        }
        assert!(kills > 5, "{kills}");
        eprintln!("R17 reconcile kills (sync={sync}): {kills}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

fn ts_values(f: &Path, p: &[u8]) -> String {
    let mut rd = Dss7File::open(f).unwrap();
    let r = rd.lookup(p).unwrap().expect("record present");
    format!("{:?}", rd.read_ts_record(&r).unwrap().ts.values)
}

/// HEC-created aliases: the primary entry of `/P32/ALIAS PRIMARY/...` is
/// damaged (status 11); its real HEC aliases still read the series. Copy-out copies it
/// under the primary's own pathname, verifies it, and records the alias it came through.
#[test]
fn r18_alias_only_record_from_hec_fixture_is_copied() {
    let d = tmp("r18_hec_alias");
    let f = d.join("src.dss");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public/synthetic_catalog_ops7.dss"),
        &f,
    )
    .unwrap();
    let primary = b"/P32/ALIAS PRIMARY/FLOW/01JAN2001/1Day/OBS/";
    let alias = b"/P32/ALIAS ONE/FLOW/01JAN2001/1Day/OBS/";
    let pr = Dss7File::open(&f)
        .unwrap()
        .lookup(primary)
        .unwrap()
        .unwrap();
    let want = ts_values(&f, primary);
    set_word(&f, pr.bin_entry_address + 1, 11);
    assert_eq!(
        ts_values(&f, alias),
        want,
        "the alias still reads the series"
    );
    let before = std::fs::read(&f).unwrap();
    let dest = d.join("dest.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), before);
    assert!(rep.complete(), "{rep:?}");
    assert_eq!(ts_values(&dest, primary), want);
    assert!(
        rep.variants_copied
            .iter()
            .any(|(from, to, why)| !from.eq_ignore_ascii_case(primary)
                && to.eq_ignore_ascii_case(primary)
                && why == "alias"),
        "{rep:?}"
    );
    assert!(copy_out_preserves(&f, "R18 HEC alias"));
    let _ = std::fs::remove_dir_all(&d);
}

/// A native alias with a distinct name: `/K/ALIAS/...` made an alias of
/// `/K/PRIMARY/...`, whose own entry is damaged. Copied under the primary's pathname.
#[test]
fn r18_alias_only_record_with_distinct_name_is_copied() {
    let d = tmp("r18_alias_ts");
    let f = d.join("src.dss");
    create_dss7(&f).unwrap();
    hourly(
        7,
        &f,
        b"/K/PRIMARY/FLOW//1HOUR/S/",
        t(2021, 3, 1, 3600),
        &[1., 2., 3.],
        &mode(false),
    )
    .unwrap();
    hourly(
        7,
        &f,
        b"/K/ALIAS/FLOW//1HOUR/S/",
        t(2021, 3, 1, 3600),
        &[8., 9., 10.],
        &mode(false),
    )
    .unwrap();
    let pp = b"/K/PRIMARY/FLOW/01Mar2021/1Hour/S/";
    let ap = b"/K/ALIAS/FLOW/01Mar2021/1Hour/S/";
    let mut rd = Dss7File::open(&f).unwrap();
    let pr = rd.lookup(pp).unwrap().unwrap();
    let ar = rd.lookup(ap).unwrap().unwrap();
    drop(rd);
    let want = ts_values(&f, pp);
    set_word(&f, ar.bin_entry_address + 1, 2);
    set_word(&f, ar.bin_entry_address + 3, pr.info_address);
    set_word(&f, pr.bin_entry_address + 1, 11);
    set_word(&f, 4, 1);
    assert_eq!(ts_values(&f, ap), want);
    let before = std::fs::read(&f).unwrap();
    let dest = d.join("dest.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), before);
    assert!(rep.complete(), "{rep:?}");
    assert_eq!(ts_values(&dest, pp), want);
    assert!(
        rep.variants_copied
            .iter()
            .any(|(from, to, why)| from.as_slice() == ap.as_slice()
                && to.as_slice() == pp.as_slice()
                && why == "alias"),
        "{rep:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// `synthetic_recovery7` with the hash-chain DUP (info 608) given the intact info block
/// of the orphan duplicate (647, lowercase pathname), and 647 zeroed. Optionally the
/// orphan entry's spelling is made uppercase too.
fn r18_case_duplicate(f: &Path, same_spelling: bool) -> (Vec<u8>, i64) {
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/synthetic_recovery7.dss"),
        f,
    )
    .unwrap();
    let p = b"/RECOVERY/DUP/FLOW/TS-PATTERN/1HOUR/SYN/";
    let mut rd = Dss7File::open(f).unwrap();
    let cat = rd
        .catalog_with(&dss_core::dss7::CatalogOptions {
            recover_unlinked: true,
            ..Default::default()
        })
        .unwrap();
    let bad = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == p)
        .unwrap();
    let good = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == p.to_ascii_lowercase())
        .unwrap();
    let goodref = rd.record_at(&cat, good).unwrap();
    let preferred = cat.meta[bad].info_address;
    let goodentry = cat.meta[good].bin_address + i64::from(cat.meta[good].entry_offset);
    drop(rd);
    let n = 30 + p.len().div_ceil(8);
    for j in 0..n as i64 {
        set_word(f, preferred + j, file_word(f, goodref.info_address + j));
    }
    set_word(f, goodref.info_address, 0);
    if same_spelling {
        for (j, c) in p.chunks(8).enumerate() {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            set_word(f, goodentry + 7 + j as i64, i64::from_le_bytes(w));
        }
    }
    (p.to_vec(), preferred)
}

/// Case duplicates with a readable preferred copy: lookup reads
/// the intact uppercase DUP (its info block names the lowercase spelling); the orphan
/// duplicate is unreadable. The record is copied from the preferred copy, under its own
/// pathname, verified, with no fallback reported, and the case variant recorded
/// when the spellings differ.
#[test]
fn r18_r19_case_duplicate_with_readable_preferred_copy() {
    for same in [false, true] {
        let d = tmp(&format!("r18_case_{same}"));
        let f = d.join("src.dss");
        let (p, preferred) = r18_case_duplicate(&f, same);
        let want = ts_values(&f, &p);
        let before = std::fs::read(&f).unwrap();
        let dest = d.join("dest.dss");
        let rep = repair_dss7(&f, &dest).unwrap_or_else(|e| panic!("same={same}: {e}"));
        assert_eq!(std::fs::read(&f).unwrap(), before);
        assert_eq!(ts_values(&dest, &p), want, "same={same}");
        assert!(
            !rep.fallback_copies
                .iter()
                .any(|(q, _, _)| q.eq_ignore_ascii_case(&p)),
            "same={same}: no fallback occurred: {rep:?}"
        );
        assert!(
            !rep.not_copied
                .iter()
                .any(|(q, _)| q.eq_ignore_ascii_case(&p)),
            "same={same}: {rep:?}"
        );
        let _ = preferred;
        assert!(copy_out_preserves(&f, &format!("R18 case same={same}")));
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Two native RTS records, PRIMARY1 (1, 2, 3)
/// and PRIMARY2 (11, 22, 33); only PRIMARY1's info-block pathname bytes are changed to
/// PRIMARY2 (same length). Lookups still read both series. Copy-out must not adopt
/// PRIMARY1 under PRIMARY2's name (dropping one payload): PRIMARY1 is reported as not
/// copied, the report is not complete, and PRIMARY2 is copied with its own values.
#[test]
fn r20_primary_naming_another_record_is_reported_not_merged() {
    let d = tmp("r20");
    let f = d.join("src.dss");
    create_dss7(&f).unwrap();
    hourly(
        7,
        &f,
        b"/K/PRIMARY1/FLOW//1HOUR/S/",
        t(2021, 3, 1, 3600),
        &[1., 2., 3.],
        &mode(false),
    )
    .unwrap();
    hourly(
        7,
        &f,
        b"/K/PRIMARY2/FLOW//1HOUR/S/",
        t(2021, 3, 1, 3600),
        &[11., 22., 33.],
        &mode(false),
    )
    .unwrap();
    let p1 = b"/K/PRIMARY1/FLOW/01Mar2021/1Hour/S/";
    let p2 = b"/K/PRIMARY2/FLOW/01Mar2021/1Hour/S/";
    let want1 = ts_values(&f, p1);
    let want2 = ts_values(&f, p2);
    let r1 = Dss7File::open(&f).unwrap().lookup(p1).unwrap().unwrap();
    for (j, c) in p2.chunks(8).enumerate() {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        set_word(&f, r1.info_address + 30 + j as i64, i64::from_le_bytes(w));
    }
    assert_eq!(ts_values(&f, p1), want1, "the source still reads PRIMARY1");
    assert_eq!(ts_values(&f, p2), want2, "and PRIMARY2");
    let before = std::fs::read(&f).unwrap();
    let dest = d.join("dest.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), before);
    assert!(!rep.complete(), "{rep:?}");
    assert!(
        rep.not_copied
            .iter()
            .any(|(q, why)| q.as_slice() == p1.as_slice() && why.contains("names another record")),
        "{rep:?}"
    );
    assert!(rep.variants_copied.is_empty(), "{rep:?}");
    assert_eq!(rep.records_copied, 1);
    assert_eq!(
        ts_values(&dest, p2),
        want2,
        "PRIMARY2 keeps its own payload"
    );
    assert!(Dss7File::open(&dest).unwrap().lookup(p1).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// A duplicate pathname whose second readable copy has a different payload
/// is reported, not skipped. `synthetic_recovery7`'s DUP: the zeroed hash-chain copy
/// (info 608) gets the orphan's info block with one value changed.
#[test]
fn r20_duplicate_with_different_payload_is_reported() {
    let d = tmp("r20_dup");
    let f = d.join("src.dss");
    let (p, preferred) = r18_case_duplicate(&f, false);
    // Restore the orphan copy (zeroed by the helper) from the preferred one, then change
    // one value word of the preferred copy's data.
    let mut rd = Dss7File::open(&f).unwrap();
    let r = rd.lookup(&p).unwrap().unwrap();
    drop(rd);
    let cat = Dss7File::open(&f)
        .unwrap()
        .catalog_with(&dss_core::dss7::CatalogOptions {
            recover_unlinked: true,
            ..Default::default()
        })
        .unwrap();
    let orphan = cat
        .catalog
        .entries
        .iter()
        .zip(&cat.meta)
        .find(|(e, m)| e.pathname == p.to_ascii_lowercase() && m.info_address != preferred)
        .map(|(_, m)| m.info_address)
        .unwrap();
    set_word(&f, orphan, file_word(&f, preferred));
    // The preferred copy's values1 now names another record's values of the same length.
    let other = Dss7File::open(&f)
        .unwrap()
        .lookup(b"/RECOVERY/UNLINKED/FLOW/TS-PATTERN/1HOUR/SYN/")
        .unwrap()
        .unwrap();
    assert_eq!(other.info.values1.number_ints, r.info.values1.number_ints);
    set_word(&f, preferred + 19, other.info.values1.address);
    assert_ne!(ts_values(&f, &p), "".to_string());
    let rep = repair_dss7(&f, &d.join("dest.dss")).unwrap();
    assert!(!rep.complete(), "{rep:?}");
    assert!(
        rep.not_copied
            .iter()
            .any(|(q, why)| q.eq_ignore_ascii_case(&p) && why.contains("differs")),
        "{rep:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------------
// The copy-out ledger.
// ---------------------------------------------------------------------------------------

/// `synthetic_recovery7`'s DUP, where the hash
/// lookup's copy (info 608) reads values 60..62 but its info block names another record
/// (`/RECOVERY/BAD/...`), and the catalog fallback (647) reads 40..42. The fallback is
/// copied; the readable, identity-rejected, different lookup copy must be in the ledger
/// as not copied, so the report is not complete.
#[test]
fn r21_rejected_readable_candidate_before_a_fallback_is_reported() {
    use dss_core::dss7::write::CandidateOutcome;
    let d = tmp("r21");
    let f = d.join("src.dss");
    let (p, preferred) = r18_case_duplicate(&f, false);
    let cat = Dss7File::open(&f)
        .unwrap()
        .catalog_with(&dss_core::dss7::CatalogOptions {
            recover_unlinked: true,
            ..Default::default()
        })
        .unwrap();
    // Isolate the case: the fixture's zeroed record is deleted.
    let zi = cat
        .catalog
        .entries
        .iter()
        .position(|e| e.pathname == b"/RECOVERY/ZEROED/FLOW/TS-PATTERN/1HOUR/SYN/")
        .unwrap();
    set_word(
        &f,
        cat.meta[zi].bin_address + i64::from(cat.meta[zi].entry_offset) + 1,
        11,
    );
    let orphan = cat
        .catalog
        .entries
        .iter()
        .zip(&cat.meta)
        .find(|(e, m)| e.pathname == p.to_ascii_lowercase() && m.info_address != preferred)
        .map(|(_, m)| m.info_address)
        .unwrap();
    set_word(&f, orphan, file_word(&f, preferred));
    let other = Dss7File::open(&f)
        .unwrap()
        .lookup(b"/RECOVERY/UNLINKED/FLOW/TS-PATTERN/1HOUR/SYN/")
        .unwrap()
        .unwrap();
    set_word(&f, preferred + 19, other.info.values1.address);
    set_word(&f, other.bin_entry_address + 1, 11);
    let changed = b"/RECOVERY/BAD/FLOW/TS-PATTERN/1HOUR/SYN/";
    assert_eq!(p.len(), changed.len());
    for (j, c) in changed.chunks(8).enumerate() {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        set_word(&f, preferred + 30 + j as i64, i64::from_le_bytes(w));
    }
    let mut src = Dss7File::open(&f).unwrap();
    let r = src.lookup(&p).unwrap().unwrap();
    assert_eq!(r.info_address, preferred);
    let old = format!("{:?}", src.read_ts_record(&r).unwrap().ts.values);
    let alt = cat
        .catalog
        .entries
        .iter()
        .zip(&cat.meta)
        .position(|(_, m)| m.info_address == orphan)
        .unwrap();
    let ar = src.record_at(&cat, alt).unwrap();
    let fallback = format!("{:?}", src.read_ts_record(&ar).unwrap().ts.values);
    assert_ne!(old, fallback, "both source payloads read, and differ");
    drop(src);
    let before = std::fs::read(&f).unwrap();
    let dest = d.join("dest.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert_eq!(before, std::fs::read(&f).unwrap());
    assert_eq!(ts_values(&dest, &p), fallback, "the fallback is copied");
    assert!(!rep.complete(), "{rep:?}");
    let entry = rep
        .ledger
        .iter()
        .find(|c| c.info_address == Some(preferred))
        .expect("the rejected candidate is in the ledger");
    assert!(
        matches!(&entry.outcome, CandidateOutcome::NotCopied(why)
            if why.contains("names another record") && why.contains("differs")),
        "{entry:?}"
    );
    assert!(rep
        .ledger
        .iter()
        .any(|c| c.info_address == Some(orphan) && c.outcome == CandidateOutcome::Copied));
    assert!(rep
        .not_copied
        .iter()
        .any(|(q, _)| q.eq_ignore_ascii_case(&p)));
    let _ = std::fs::remove_dir_all(&d);
}

/// The ledger accounts for every source candidate, exactly once. Candidates
/// are enumerated here independently of copy-out: per live pathname group, the
/// lookup's record and each catalog entry's record (each distinct info block once), and
/// each alias entry. Each must have exactly one ledger entry; a group refused as a whole
/// has one entry without an address. `complete` and `not_copied` must agree with the
/// ledger, and `records_copied` with its `Copied` entries.
fn ledger_accounts_for(f: &Path) {
    use dss_core::dss7::write::CandidateOutcome;
    let what = f.display().to_string();
    let dest = d_for(f);
    let _ = std::fs::remove_file(&dest);
    let rep = match repair_dss7(f, &dest) {
        Ok(r) => r,
        Err(e) => panic!("{what}: {e}"),
    };
    let _ = std::fs::remove_file(&dest);
    let mut file = Dss7File::open(f).unwrap();
    let cat = file
        .catalog_with(&dss_core::dss7::CatalogOptions {
            recover_unlinked: true,
            ..Default::default()
        })
        .unwrap();
    let mut groups: BTreeMap<Vec<u8>, (Vec<u8>, Vec<usize>)> = BTreeMap::new();
    let mut aliases = 0usize;
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        match cat.meta[i].raw_status {
            0 | 1 => groups
                .entry(e.pathname.to_ascii_uppercase())
                .or_insert_with(|| (e.pathname.clone(), Vec::new()))
                .1
                .push(i),
            2 => aliases += 1,
            _ => {}
        }
    }
    // The ledger's primary entries by uppercase pathname (indexed once: linear).
    let mut by_key: BTreeMap<Vec<u8>, Vec<&dss_core::dss7::write::Candidate>> = BTreeMap::new();
    for c in rep.ledger.iter().filter(|c| !c.alias) {
        by_key
            .entry(c.pathname.to_ascii_uppercase())
            .or_default()
            .push(c);
    }
    for (key, (path, idxs)) in &groups {
        let mut found: BTreeSet<i64> = BTreeSet::new();
        if let Ok(Some(r)) = file.lookup(path) {
            found.insert(r.info_address);
        }
        for &i in idxs {
            if let Ok(r) = file.record_at(&cat, i) {
                found.insert(r.info_address);
            }
        }
        let mine: Vec<_> = by_key.get(key).cloned().unwrap_or_default();
        if mine.len() == 1 && mine[0].info_address.is_none() {
            continue; // refused as a whole, with its reason
        }
        let got: Vec<i64> = mine.iter().filter_map(|c| c.info_address).collect();
        let got_set: BTreeSet<i64> = got.iter().copied().collect();
        assert_eq!(
            got.len(),
            got_set.len(),
            "{what}: {key:?}: a candidate twice"
        );
        // On these files discovery reaches at least what the recovery catalog lists (a
        // partial bin block would not be reached; dss7.md §6.4, copy-out repair).
        assert!(
            found.is_subset(&got_set),
            "{what}: {:?}: catalog candidates {found:?} not all in the ledger {got_set:?}",
            String::from_utf8_lossy(key)
        );
    }
    // Every whole-group refusal (no address) carries one of the documented reasons.
    for c in rep.ledger.iter().filter(|c| c.info_address.is_none()) {
        if let CandidateOutcome::NotCopied(why) = &c.outcome {
            assert!(
                [
                    "implausible pathname",
                    "time series in the pre-7-HA",
                    "hash lookup",
                    "bin entry at word",
                    "no readable copy",
                    "unclassified",
                    "alias:"
                ]
                .iter()
                .any(|k| why.contains(k)),
                "{what}: refusal without a documented reason: {why}"
            );
        }
    }
    // Every alias entry of the catalog is a ledger alias entry, by pathname.
    let mut ledger_aliases: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for c in rep.ledger.iter().filter(|c| c.alias) {
        *ledger_aliases.entry(c.pathname.clone()).or_default() += 1;
    }
    let mut cat_aliases: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        if cat.meta[i].raw_status == 2 {
            *cat_aliases.entry(e.pathname.clone()).or_default() += 1;
        }
    }
    for (a, n) in &cat_aliases {
        assert!(
            ledger_aliases.get(a).copied().unwrap_or(0) >= *n,
            "{what}: alias {a:?}"
        );
    }
    let _ = aliases;
    let not: Vec<_> = rep
        .ledger
        .iter()
        .filter(|c| matches!(c.outcome, CandidateOutcome::NotCopied(_)))
        .collect();
    assert_eq!(rep.complete(), not.is_empty(), "{what}");
    // `not_copied` is exactly the ledger's NotCopied entries, in order.
    let from_ledger: Vec<(Vec<u8>, String)> = not
        .iter()
        .map(|c| match &c.outcome {
            CandidateOutcome::NotCopied(w) => (c.pathname.clone(), w.clone()),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(rep.not_copied, from_ledger, "{what}");
    let copied = rep
        .ledger
        .iter()
        .filter(|c| c.outcome == CandidateOutcome::Copied)
        .count();
    assert_eq!(copied, rep.records_copied, "{what}");
}

fn d_for(f: &Path) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("p64_kill_{}_ledger", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let k = N.fetch_add(1, Ordering::SeqCst);
    d.join(format!(
        "{}.{k}.copy.dss",
        f.file_name().unwrap().to_string_lossy()
    ))
}

/// The ledger accounting over every public DSS7 fixture, and over every
/// DSS7 file of `DSS_CORPUS_LIST` (one path per line; the files are read in place,
/// never written) when set.
#[test]
fn r21_ledger_accounts_for_every_candidate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dss")))
        .collect();
    if let Ok(list) = std::env::var("DSS_CORPUS_LIST") {
        for l in std::fs::read_to_string(list).unwrap().lines() {
            if !l.trim().is_empty() {
                files.push(PathBuf::from(l.trim()));
            }
        }
    }
    let mut n = 0;
    for f in files {
        if Dss7File::open(&f).is_err() {
            continue;
        }
        ledger_accounts_for(&f);
        n += 1;
    }
    assert!(n > 20, "{n}");
    eprintln!("ledger accounting: {n} DSS7 files");
}

// ---------------------------------------------------------------------------------------
// Discovery through empty bins, and an independent sweep.
// ---------------------------------------------------------------------------------------

/// A one-record file whose bin is moved to the
/// end of the file, the original bin zeroed except its overflow word, which names the
/// moved bin. Lookup reaches the record through the empty bin; neither catalog lists it.
/// Copy-out must find it (its discovery follows overflow links from empty bins) and copy
/// it, with the expected values, as a hidden record.
#[test]
fn r23_record_behind_an_empty_bin_is_found_and_copied() {
    let d = tmp("r23");
    let f = d.join("src.dss");
    create_dss7(&f).unwrap();
    let p = b"/K/HIDDEN/FLOW/01Mar2021/1Hour/S/";
    hourly(
        7,
        &f,
        b"/K/HIDDEN/FLOW//1HOUR/S/",
        t(2021, 3, 1, 3600),
        &[11., 22., 33.],
        &mode(false),
    )
    .unwrap();
    let want = ts_values(&f, p);
    let (bin, bs) = {
        let mut src = Dss7File::open(&f).unwrap();
        let r = src.lookup(p).unwrap().unwrap();
        (r.bin_entry_address, src.header().bin_size)
    };
    let mut bytes = std::fs::read(&f).unwrap();
    let appended = (bytes.len() / 8) as i64;
    let saved = bytes[(bin * 8) as usize..((bin + bs) * 8) as usize].to_vec();
    bytes.extend_from_slice(&saved);
    bytes[(bin * 8) as usize..((bin + bs) * 8) as usize].fill(0);
    bytes[((bin + bs - 1) * 8) as usize..((bin + bs) * 8) as usize]
        .copy_from_slice(&appended.to_le_bytes());
    let len = (bytes.len() / 8) as i64;
    bytes[5 * 8..6 * 8].copy_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    std::fs::write(&f, &bytes).unwrap();
    assert_eq!(ts_values(&f, p), want, "lookup still reaches it");
    let cat = Dss7File::open(&f)
        .unwrap()
        .catalog_with(&dss_core::dss7::CatalogOptions {
            recover_unlinked: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        !cat.catalog.entries.iter().any(|e| e.pathname == p),
        "no catalog lists it"
    );
    // The independent sweep first: it would catch the omission on its own.
    let sw = sweep_check(&f);
    assert_eq!((sw.readable, sw.in_ledger), (1, 1), "{sw:?}");
    let dest = d.join("dest.dss");
    let rep = repair_dss7(&f, &dest).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), bytes, "src unchanged");
    assert_eq!(ts_values(&dest, p), want, "copied with its values");
    assert_eq!(
        (rep.records_copied, rep.hidden_records_copied),
        (1, 1),
        "{rep:?}"
    );
    assert!(rep.complete(), "{rep:?}");
    ledger_accounts_for(&f);
    let _ = std::fs::remove_dir_all(&d);
}

/// Counts of [`sweep_check`].
#[derive(Default, Debug, Clone, Copy)]
struct Sweep {
    headers: usize,
    readable: usize,
    in_ledger: usize,
    unreachable: usize,
}

/// A brute-force check of copy-out's discovery, independent of it.
///
/// 1. Every word-aligned position of the file is tested for a plausible info-block
///    header (the info flag, a status word, a pathname length of 1..=392, and pathname
///    bytes that decode as a DSS pathname); each one is read as a record
///    (`read_info` plus the normal decoder).
/// 2. Independently of the library's traversal, this test walks every pointer of the
///    pathname structures: every bin of every block of the bin-block chain and every
///    hash-table slot, then every overflow word of every bin reached (empty bins
///    included), bounded and each bin once. It parses entries as `lookup()` does and
///    collects the info address each live (status 0/1) or alias entry names (an alias
///    chain followed to its info block). Deleted, renamed and moved entries are not
///    reader-reachable (`lookup()` skips them).
/// 3. Every readable record of step 1 must have a ledger entry at its address, or no
///    pointer of step 2 may name it.
fn sweep_check(f: &Path) -> Sweep {
    const FLAG: i64 = -97534;
    let dest = d_for(f);
    let _ = std::fs::remove_file(&dest);
    let rep = repair_dss7(f, &dest).unwrap_or_else(|e| panic!("{f:?}: {e}"));
    let _ = std::fs::remove_file(&dest);
    let in_ledger: BTreeSet<i64> = rep.ledger.iter().filter_map(|c| c.info_address).collect();
    let bytes = std::fs::read(f).unwrap();
    let n = (bytes.len() / 8) as i64;
    let w = |i: i64| -> i64 {
        if i < 0 || i >= n {
            return 0;
        }
        let o = (i * 8) as usize;
        i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap())
    };
    let mut file = Dss7File::open(f).unwrap();
    let h = file.header().clone();
    let bs = h.bin_size;
    // Step 2: pointers.
    let mut pointed: BTreeSet<i64> = BTreeSet::new();
    let max_bins = (n / bs.max(1) + 1) as usize;
    let mut work: Vec<i64> = Vec::new();
    let mut block = h.first_bin_address;
    let mut blocks: BTreeSet<i64> = BTreeSet::new();
    while block > 0 && block < n && blocks.insert(block) && blocks.len() <= max_bins {
        for i in 0..h.bins_per_block {
            work.push(block + i * bs);
        }
        block = w(block + bs * h.bins_per_block);
    }
    for i in 0..h.max_hash {
        let a = w(h.hash_table_address + i);
        if a != 0 {
            work.push(a);
        }
    }
    let mut visited: BTreeSet<i64> = BTreeSet::new();
    while let Some(a) = work.pop() {
        if visited.len() > max_bins || a <= 0 || a + bs > n || !visited.insert(a) {
            continue;
        }
        let mut ic = 0i64;
        while ic + 7 < bs {
            if w(a + ic) == 0 {
                break;
            }
            let lens = w(a + ic + 2);
            let (chars, words) = (lens as u32 as i32 as i64, lens >> 32);
            if chars <= 0 || chars > 392 || words != (chars + 7) / 8 || ic + 7 + words > bs - 1 {
                break;
            }
            let st = w(a + ic + 1);
            if matches!(st, 0..=2) {
                let mut x = w(a + ic + 3);
                // An alias chain runs through alias entries to an info block.
                for _ in 0..1000 {
                    if w(x) == FLAG || w(x + 1) != 2 {
                        break;
                    }
                    x = w(x + 3);
                }
                pointed.insert(x);
            }
            ic += 7 + words;
            if ic >= bs - 2 {
                break;
            }
        }
        let next = w(a + bs - 1);
        if next != 0 {
            work.push(next);
        }
    }
    // Steps 1 and 3.
    let mut out = Sweep::default();
    let mut missing: Vec<i64> = Vec::new();
    for i in 0..n.saturating_sub(31) {
        if w(i) != FLAG {
            continue;
        }
        let (st, plen) = (w(i + 1), w(i + 2));
        if !(0..=30).contains(&st) || !(1..=392).contains(&plen) {
            continue;
        }
        let pb: Vec<u8> = (0..(plen + 7) / 8)
            .flat_map(|j| w(i + 30 + j).to_le_bytes())
            .take(plen as usize)
            .collect();
        if pb.first() != Some(&b'/')
            || pb.last() != Some(&b'/')
            || pb.iter().filter(|&&c| c == b'/').count() != 7
            || pb.iter().any(|&c| c < 0x20 && c != 0)
        {
            continue;
        }
        out.headers += 1;
        let Ok(info) = file.read_info(i, plen as usize) else {
            continue;
        };
        let r = dss_core::dss7::RecordRef {
            pathname: pb.clone(),
            status: 1,
            record_type: info.record_type,
            bin_entry_address: 0,
            info_address: i,
            info: info.clone(),
        };
        if decode_rec(&mut file, info.record_type, &r).is_none() {
            continue;
        }
        out.readable += 1;
        if in_ledger.contains(&i) {
            out.in_ledger += 1;
        } else if pointed.contains(&i) {
            missing.push(i);
        } else {
            out.unreachable += 1;
        }
    }
    assert!(
        missing.is_empty(),
        "{f:?}: readable records that a pointer reaches but the ledger lacks: {:?}",
        &missing[..missing.len().min(10)]
    );
    out
}

/// The brute-force sweep over every public DSS7 fixture and, with
/// `DSS_CORPUS_LIST`, every DSS7 file it lists (read in place).
#[test]
fn r24_brute_force_sweep_finds_nothing_reachable_outside_the_ledger() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dss")))
        .collect();
    if let Ok(list) = std::env::var("DSS_CORPUS_LIST") {
        for l in std::fs::read_to_string(list).unwrap().lines() {
            let p = PathBuf::from(l.trim());
            if !l.trim().is_empty() && !files.contains(&p) {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut total = Sweep::default();
    let mut n = 0;
    for f in files {
        if Dss7File::open(&f).is_err() {
            continue;
        }
        let s = sweep_check(&f);
        eprintln!(
            "SWEEP {} headers={} readable={} in_ledger={} unreachable={}",
            f.file_name().unwrap().to_string_lossy(),
            s.headers,
            s.readable,
            s.in_ledger,
            s.unreachable
        );
        total.headers += s.headers;
        total.readable += s.readable;
        total.in_ledger += s.in_ledger;
        total.unreachable += s.unreachable;
        n += 1;
    }
    assert!(n > 20, "{n}");
    eprintln!("SWEEP TOTAL {n} files: {total:?}");
}

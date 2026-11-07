//! P38 re-check: a refusal that depends on the file must come before the first store of a
//! multi-record write. Each test makes a later record's store refuse, on synthetic damage or
//! a counter at its limit, and requires the whole file byte-identical afterwards.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use dss_core::dss6::write as w6;
use dss_core::dss7::interval::ymd_to_julian;
use dss_core::dss7::write::{
    create_dss7, write_its, write_its_variants, write_profile, write_rts, write_rts_variants,
    BlockOutcome, ItsInput, LocationInput, ProfileArrays, ProfileInput, RtsInput, RtsValues,
    TsVariants, WriteError, WriteOptions,
};
use dss_core::dss7::Dss7File;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("atomic_refusal_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("f.dss")
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: false,
        ..WriteOptions::default()
    }
}

fn at(y: i64, m: i64, d: i64) -> i64 {
    ymd_to_julian(y, m, d) * 86_400
}

fn patch(p: &Path, off: u64, b: &[u8]) {
    let mut f = OpenOptions::new().write(true).open(p).unwrap();
    f.seek(SeekFrom::Start(off)).unwrap();
    f.write_all(b).unwrap();
}

/// The write must refuse with the file byte-identical; returns the error.
fn refused<T: std::fmt::Debug>(p: &Path, w: impl FnOnce() -> Result<T, WriteError>) -> WriteError {
    let before = std::fs::read(p).unwrap();
    let e = w().expect_err("the write must refuse");
    assert!(
        before == std::fs::read(p).unwrap(),
        "file changed by a refused write: {e}"
    );
    e
}

/// An empty DSS7 file whose next bin allocation meets a damaged bin-block chain word.
fn damaged_chain7(p: &Path) {
    create_dss7(p).unwrap();
    let b = std::fs::read(p).unwrap();
    let w = |k: usize| i64::from_le_bytes(b[k * 8..k * 8 + 8].try_into().unwrap());
    let (next, bin_size) = (w(25), w(23));
    patch(p, 22 * 8, &1i64.to_le_bytes());
    patch(
        p,
        (next + bin_size) as u64 * 8,
        &999_999_999i64.to_le_bytes(),
    );
}

#[test]
fn dss7_scalar_future_bin_allocation_damage() {
    let vals = [1f32, 2.0];
    let q = [1, 2, 3, 4];
    let var = TsVariants {
        quality_words: Some((&q, 2)),
        notes: None,
    };
    let t = [at(2001, 1, 2), at(2002, 1, 2)];
    let irr = ItsInput::new(
        b"/R/C/FLOW//IR-Year/A/",
        &t,
        RtsValues::F32(&vals),
        b"CFS",
        b"INST-VAL",
    );
    let reg = RtsInput::new(
        b"/R/C/FLOW//1Day/A/",
        at(2002, 1, 1),
        RtsValues::F32(&vals),
        b"CFS",
        b"INST-VAL",
    );
    let p = tmp("a1");
    damaged_chain7(&p);
    let e = refused(&p, || write_rts(&p, &reg, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
    let e = refused(&p, || write_rts_variants(&p, &reg, &var, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
    let e = refused(&p, || write_its(&p, &irr, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
    let e = refused(&p, || write_its_variants(&p, &irr, &var, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
}

/// A single record's refusal stays the typed refusal it is: planning did not run, so it is
/// neither an internal error nor, in a debug build, a panic.
#[test]
fn dss7_single_record_allocation_refusal_is_typed() {
    let p = tmp("one");
    create_dss7(&p).unwrap();
    let b = std::fs::read(&p).unwrap();
    let w = |k: usize| i64::from_le_bytes(b[k * 8..k * 8 + 8].try_into().unwrap());
    let link = w(25) + w(23);
    patch(&p, 22 * 8, &0i64.to_le_bytes());
    patch(&p, 25 * 8, &link.to_le_bytes());
    patch(&p, link as u64 * 8, &999_999_999i64.to_le_bytes());
    let v = [1f32];
    let t = [at(2001, 1, 2)];
    let reg = RtsInput::new(
        b"/R/ONE/FLOW//1Day/A/",
        at(2001, 1, 2),
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    let irr = ItsInput::new(
        b"/R/ONE/FLOW//IR-Year/A/",
        &t,
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    let d = [1f32];
    let prof = ProfileInput::new(
        b"/R/ONE/FLOW//1Day/P/",
        at(2001, 1, 2),
        ProfileArrays::F32 {
            depths: &d,
            values: &v,
        },
        b"FT",
        b"CFS",
        b"INST-VAL",
    );
    let e = refused(&p, || write_rts(&p, &reg, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
    let e = refused(&p, || write_its(&p, &irr, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
    let e = refused(&p, || write_profile(&p, &prof, &opts()));
    assert!(matches!(e, WriteError::Corrupt(_)), "{e}");
}

/// Make a DSS6 file logically end at the last address, keeping its records: sparse, so the
/// test needs only a few blocks of disk (`fsutil` on NTFS; Unix file systems are sparse by
/// default). Returns false where the file cannot be made sparse.
fn extend_to_address_limit(p: &Path) -> bool {
    #[cfg(windows)]
    {
        let sparse = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(p)
            .output();
        if !sparse.is_ok_and(|o| o.status.success()) {
            return false;
        }
    }
    // dss6.md: 127 words in each 512-byte record; KFSIZE is word 6, and -9630 ends the file.
    let word = i32::MAX as u64 - 1;
    let f = OpenOptions::new().write(true).open(p).unwrap();
    f.set_len((word / 127 + 1) * 512).unwrap();
    drop(f);
    patch(p, 5 * 4, &i32::MAX.to_le_bytes());
    patch(
        p,
        (word / 127) * 512 + (word % 127) * 4,
        &(-9630i32).to_le_bytes(),
    );
    true
}

/// The allocated head and tail of an address-limit file; the rest is a hole.
fn ends(p: &Path, head: u64) -> (Vec<u8>, Vec<u8>, u64) {
    let mut f = std::fs::File::open(p).unwrap();
    let len = f.metadata().unwrap().len();
    let mut a = vec![0; head as usize];
    f.read_exact(&mut a).unwrap();
    f.seek(SeekFrom::Start(len - 4096)).unwrap();
    let mut b = vec![0; 4096];
    f.read_exact(&mut b).unwrap();
    (a, b, len)
}

/// A DSS6 file at the last address: an equal rewrite needs no new address and succeeds, for
/// one block and for two (planned) blocks with retained user headers, and real growth refuses
/// with the file unchanged.
#[test]
fn dss6_no_op_at_the_address_limit() {
    let p = tmp("limit");
    w6::create_dss6(&p).unwrap();
    let days = [1f32; 730];
    let two = RtsInput::new(
        b"/R/H/FLOW//1Day/A/",
        at(2001, 1, 1),
        RtsValues::F32(&days),
        b"CFS",
        b"INST-VAL",
    );
    w6::write_rts6(&p, &two, &opts()).unwrap();
    // The stored user header each rewrite compares and would have to carry.
    let header = [b'h'; 2000];
    let location = LocationInput {
        pathname: two.pathname,
        x: Some(9.0),
        supplemental: Some(&header),
        ..Default::default()
    };
    w6::write_location6(&p, &location, &opts()).unwrap();
    let one = RtsInput::new(
        b"/R/H/FLOW//1Year/B/",
        at(2001, 1, 1),
        RtsValues::F32(&[1.0]),
        b"CFS",
        b"INST-VAL",
    );
    w6::write_rts6(&p, &one, &opts()).unwrap();
    let head = std::fs::metadata(&p).unwrap().len();
    if !extend_to_address_limit(&p) {
        eprintln!("skipped: the file system cannot hold a sparse file");
        return;
    }
    assert!(w6::check_file6(&p).unwrap().is_clean());
    let before = ends(&p, head);
    for input in [&one, &two] {
        let r = w6::write_rts6(&p, input, &opts()).unwrap();
        assert!(r
            .blocks
            .iter()
            .all(|b| b.outcome == BlockOutcome::Unchanged));
    }
    let r = w6::write_location6(&p, &location, &opts()).unwrap();
    assert!(r
        .blocks
        .iter()
        .all(|b| b.outcome == BlockOutcome::Unchanged));
    assert!(before == ends(&p, head), "a no-op changed the file");
    // Real growth: a single record, then a planned write whose second record changes.
    let new_one = RtsInput::new(
        one.pathname,
        one.start,
        RtsValues::F32(&[2.0]),
        b"CFS",
        b"INST-VAL",
    );
    let e = w6::write_rts6(&p, &new_one, &opts()).unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    assert!(before == ends(&p, head), "a refused write changed the file");
    let mut changed = days;
    changed[729] = 7.0;
    let later = RtsInput::new(
        two.pathname,
        two.start,
        RtsValues::F32(&changed),
        b"CFS",
        b"INST-VAL",
    );
    let e = w6::write_rts6(&p, &later, &opts()).unwrap_err();
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    assert!(before == ends(&p, head), "a refused write changed the file");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn dss7_profile_later_block_area_damage() {
    let p = tmp("b");
    create_dss7(&p).unwrap();
    let d = [1f32, 2.0];
    let v: Vec<f32> = (0..730 * 2).map(|i| i as f32 + 1.0).collect();
    let input = ProfileInput::new(
        b"/R/P/FLOW//1Day/A/",
        at(2001, 1, 2),
        ProfileArrays::F32 {
            depths: &d,
            values: &v,
        },
        b"FT",
        b"CFS",
        b"INST-VAL",
    );
    write_profile(&p, &input, &opts()).unwrap();
    let later = Dss7File::new(std::fs::File::open(&p).unwrap())
        .unwrap()
        .lookup(b"/R/P/FLOW/01Jan2002/1Day/A/")
        .unwrap()
        .unwrap();
    patch(
        &p,
        (later.info_address + 17) as u64 * 8,
        &999_999_999i64.to_le_bytes(),
    );
    patch(
        &p,
        (later.info_address + 18) as u64 * 8,
        &1i64.to_le_bytes(),
    );
    let mut nv = v.clone();
    nv[0] = 999.0;
    let mut ni = input.clone();
    ni.data = ProfileArrays::F32 {
        depths: &d,
        values: &nv,
    };
    refused(&p, || write_profile(&p, &ni, &opts()));
}

/// KSEQNO one short of its limit: the first new record takes it, the second cannot.
fn seqno_at_limit(p: &Path) {
    w6::create_dss6(p).unwrap();
    patch(p, 2 * 4, &(i32::MAX - 1).to_le_bytes());
}

#[test]
fn dss6_series_cumulative_counter() {
    let v = [1f32, 2.0];
    let p = tmp("c1");
    seqno_at_limit(&p);
    let i = RtsInput::new(
        b"/R/C/FLOW//1Day/A/",
        at(2002, 1, 1),
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    let e = refused(&p, || w6::write_rts6(&p, &i, &opts()));
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
    let t = [at(2001, 1, 2), at(2002, 1, 2)];
    let i = w6::ItsInput {
        pathname: b"/R/C/FLOW//IR-YEAR/A/",
        times: &t,
        values: RtsValues::F32(&v),
        quality: None,
        units: b"CFS",
        data_type: b"INST-VAL",
        mode: w6::ItsMode::Replace,
    };
    let e = refused(&p, || w6::write_its6(&p, &i, &opts()));
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
}

#[test]
fn dss6_location_cumulative_counter() {
    let p = tmp("c2");
    w6::create_dss6(&p).unwrap();
    let v = [1f32, 2.0];
    let i = RtsInput::new(
        b"/R/LOC/FLOW//1Day/A/",
        at(2002, 1, 1),
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    w6::write_rts6(&p, &i, &opts()).unwrap();
    let l = LocationInput {
        pathname: i.pathname,
        x: Some(9.0),
        ..Default::default()
    };
    // What the two-record update adds to KDEAD, measured on a copy.
    let dry = p.with_file_name("dry.dss");
    std::fs::copy(&p, &dry).unwrap();
    w6::write_location6(&dry, &l, &opts()).unwrap();
    let b = std::fs::read(&dry).unwrap();
    let cost = i32::from_le_bytes(b[24..28].try_into().unwrap());
    patch(&p, 24, &(i32::MAX - cost / 2).to_le_bytes());
    let e = refused(&p, || w6::write_location6(&p, &l, &opts()));
    assert!(matches!(e, WriteError::Unsupported(_)), "{e}");
}

/// Planning changes nothing a successful write stores: the same write on a copy, with and
/// without other records, still reads back complete across a new bin block.
#[test]
fn planning_carries_earlier_stores_forward() {
    let p = tmp("ok");
    create_dss7(&p).unwrap();
    // One bin left in the first bin block: the second record must link a new block.
    patch(&p, 22 * 8, &1i64.to_le_bytes());
    let vals = [1f32, 2.0];
    let reg = RtsInput::new(
        b"/R/C/FLOW//1Day/A/",
        at(2002, 1, 1),
        RtsValues::F32(&vals),
        b"CFS",
        b"INST-VAL",
    );
    assert_eq!(write_rts(&p, &reg, &opts()).unwrap().blocks.len(), 2);
    let mut f = Dss7File::new(std::fs::File::open(&p).unwrap()).unwrap();
    for path in [
        &b"/R/C/FLOW/01Jan2001/1Day/A/"[..],
        b"/R/C/FLOW/01Jan2002/1Day/A/",
    ] {
        let r = f.lookup(path).unwrap().unwrap();
        f.read_ts_record(&r).unwrap();
    }
}

//! P74: the `*_to(&File, ...)` entry points added for the held `DssFile` handle write the
//! same bytes as the path writers, and `write_pattern6` builds its record once without
//! changing its result. Synthetic files in a temporary folder.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use dss_core::dss6::write as w6;
use dss_core::dss7::write::{
    self as w7, set_write_clock, PatternInput, RtsInput, RtsValues, SyncMode, TsVariants,
    WriteOptions,
};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p74_to_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rw(p: &Path) -> File {
    OpenOptions::new().read(true).write(true).open(p).unwrap()
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: SyncMode::None,
        ..WriteOptions::default()
    }
}

/// One test, so the fixed write clock (process-wide) is not shared with a parallel test.
#[test]
fn to_entry_points_write_the_same_bytes_as_the_path_writers() {
    set_write_clock(Some(1_600_000_000_000));
    let d = tmp("same");
    let vals: Vec<f32> = (0..12).map(|i| i as f32 * 0.5).collect();
    let q: Vec<i32> = (0..24).map(|i| i * 3 + 1).collect();
    let series: Vec<f32> = (0..24).map(|i| 3.0 + i as f32).collect();
    let pat = PatternInput::new(
        b"/T/PAT/FLOW/TS-PATTERN/1MON/S/",
        RtsValues::F32(&vals),
        b"CFS",
        b"PER-AVER",
    );
    let var = TsVariants {
        quality_words: Some((&q, 1)),
        notes: None,
    };
    let start = dss_core::dss7::interval::ymd_to_julian(2021, 7, 1) * 86_400 + 3600;
    let rts = RtsInput::new(
        b"/T/QUAL/FLOW//1HOUR/S/",
        start,
        RtsValues::F32(&series),
        b"CFS",
        b"INST-VAL",
    );
    for v in [6u8, 7] {
        let (a, b) = (d.join(format!("path{v}.dss")), d.join(format!("to{v}.dss")));
        if v == 6 {
            w6::create_dss6(&a).unwrap();
            w6::create_dss6(&b).unwrap();
            w6::write_pattern6(&a, &pat, &opts()).unwrap();
            w6::write_pattern6_to(&rw(&b), &pat, &opts()).unwrap();
        } else {
            w7::create_dss7(&a).unwrap();
            w7::create_dss7(&b).unwrap();
            w7::write_pattern(&a, &pat, &opts()).unwrap();
            w7::write_pattern_to(&rw(&b), &pat, &opts()).unwrap();
            w7::write_rts_variants(&a, &rts, &var, &opts()).unwrap();
            w7::write_rts_variants_to(&rw(&b), &rts, &var, &opts()).unwrap();
        }
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "DSS{v}"
        );
    }
    // A refused pattern write leaves the file unchanged on both paths (the record is built,
    // and validated, before the open).
    let bad = PatternInput::new(
        b"/T/PAT/FLOW/TS-PATTERN/1MON/S/",
        RtsValues::F32(&[]),
        b"CFS",
        b"PER-AVER",
    );
    let f = d.join("path6.dss");
    let before = std::fs::read(&f).unwrap();
    assert!(w6::write_pattern6(&f, &bad, &opts()).is_err());
    assert!(w6::write_pattern6_to(&rw(&f), &bad, &opts()).is_err());
    assert_eq!(std::fs::read(&f).unwrap(), before);
    set_write_clock(None);
    let _ = std::fs::remove_dir_all(&d);
}

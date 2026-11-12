//! P66: the DSS6 irregular-pattern opt-in holds on the
//! held-handle entry point `write_pattern6_to` as on the path writer. The default refuses
//! before writing and leaves the file unchanged; the opt-in writes the same bytes as the
//! path writer in every sync mode. Synthetic files in a temporary folder.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use dss_core::dss6::write::{self as w6, Dss6IrregularPattern, WriteError};
use dss_core::dss7::write::{set_write_clock, PatternInput, RtsValues, SyncMode, WriteOptions};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p66_held_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rw(p: &Path) -> File {
    OpenOptions::new().read(true).write(true).open(p).unwrap()
}

/// One test, so the fixed write clock (process-wide) is not shared with a parallel test.
#[test]
fn irregular_pattern_opt_in_on_the_held_handle() {
    set_write_clock(Some(1_600_000_000_000));
    let d = tmp("opt_in");
    let v: Vec<f32> = (0..12).map(|i| i as f32).collect();
    let times: Vec<i64> = (0..12).map(|k| (k + 1) * 3600).collect();
    let mut i = PatternInput::new(
        b"/P66/HELD/FLOW/TS-PATTERN/IR-YEAR/I/",
        RtsValues::F32(&v),
        b"CFS",
        b"PER-AVER",
    );
    i.times = Some(&times);
    for (k, sync) in [SyncMode::End, SyncMode::Step, SyncMode::None]
        .into_iter()
        .enumerate()
    {
        let opts = WriteOptions {
            sync,
            ..WriteOptions::default()
        };
        let (a, b) = (d.join(format!("path{k}.dss")), d.join(format!("to{k}.dss")));
        w6::create_dss6(&a).unwrap();
        w6::create_dss6(&b).unwrap();
        // Default: refused on the held handle, nothing written.
        let before = std::fs::read(&b).unwrap();
        assert!(matches!(
            w6::write_pattern6_to(&rw(&b), &i, &opts),
            Err(WriteError::Unsupported(m)) if m.contains("opt-in")
        ));
        assert!(matches!(
            w6::write_pattern6_to_with(&rw(&b), &i, &opts, Dss6IrregularPattern::Refuse),
            Err(WriteError::Unsupported(_))
        ));
        assert_eq!(std::fs::read(&b).unwrap(), before, "{sync:?}");
        // Opt-in: same bytes as the path writer.
        let ra =
            w6::write_pattern6_with(&a, &i, &opts, Dss6IrregularPattern::AllowUnconfirmed).unwrap();
        let rb =
            w6::write_pattern6_to_with(&rw(&b), &i, &opts, Dss6IrregularPattern::AllowUnconfirmed)
                .unwrap();
        assert_eq!(ra.blocks[0].record_type, 111);
        assert_eq!(rb.blocks[0].record_type, 111);
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "{sync:?}"
        );
        assert!(w6::check_file6(&b).unwrap().problems.is_empty());
    }
    set_write_clock(None);
    let _ = std::fs::remove_dir_all(&d);
}

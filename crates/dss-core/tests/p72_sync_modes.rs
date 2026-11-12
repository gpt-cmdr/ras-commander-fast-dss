//! The three flush policies of `WriteOptions { sync }`.
//!
//! `SyncMode::End`, the default, flushes once at the end of a write call; `Step` also
//! flushes at every flush point of the transaction; `None` never flushes. This test
//! counts the flushes through the hidden counter `dss7::write::flush_count`, which is
//! process-wide: everything is in one test function of its own test binary, so no other
//! write runs beside it. The killed-process qualification of the modes is
//! `tests/p64_kill.rs`.

use std::path::{Path, PathBuf};

use dss_core::dss6::write::{create_dss6, create_dss6_with, write_pd6, write_rts6};
use dss_core::dss7::write::{
    create_dss7, create_dss7_with, flush_count, repair_dss7, repair_dss7_with, sync_path, write_pd,
    write_rts, PdArrays, PdInput, RtsInput, RtsValues, SyncMode, WriteOptions,
};
use dss_core::time::ymd_to_julian;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("p72_sync_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opts(sync: SyncMode) -> WriteOptions {
    WriteOptions {
        sync,
        ..WriteOptions::default()
    }
}

/// Flushes made by `f`.
fn flushes(f: impl FnOnce()) -> u64 {
    let before = flush_count();
    f();
    flush_count() - before
}

fn rts(v: u8, file: &Path, path: &[u8], shift: f64, n: usize, o: &WriteOptions) {
    let vals: Vec<f64> = (0..n).map(|i| i as f64 * 0.25 + shift).collect();
    let start = ymd_to_julian(2021, 1, 1) * 86_400 + 3600;
    let input = RtsInput::new(path, start, RtsValues::F64(&vals), b"CFS", b"INST-VAL");
    if v == 6 {
        write_rts6(file, &input, o).unwrap();
    } else {
        write_rts(file, &input, o).unwrap();
    }
}

fn pd(v: u8, file: &Path, path: &[u8], shift: f64, o: &WriteOptions) {
    let x = [0.0f64, 1.0, 2.0];
    let y = [shift, shift + 1.0, shift + 2.0];
    let input = PdInput::new(
        path,
        PdArrays::F64 {
            ordinates: &x,
            values: &y,
        },
        1,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    if v == 6 {
        write_pd6(file, &input, o).unwrap();
    } else {
        write_pd(file, &input, o).unwrap();
    }
}

#[test]
fn flush_points_of_the_three_modes() {
    assert_eq!(WriteOptions::default().sync, SyncMode::End, "default");
    assert_eq!(SyncMode::default(), SyncMode::End);
    for m in [SyncMode::None, SyncMode::End, SyncMode::Step] {
        assert_eq!(SyncMode::from_name(m.as_str()), Some(m));
    }
    assert_eq!(SyncMode::from_name("true"), None);
    assert_eq!(SyncMode::End.batched(), (SyncMode::None, true));
    assert_eq!(SyncMode::Step.batched(), (SyncMode::Step, false));
    assert_eq!(SyncMode::None.batched(), (SyncMode::None, false));

    let d = dir();
    for v in [7u8, 6] {
        let create = |p: &Path, sync: Option<SyncMode>| match (v, sync) {
            (6, None) => create_dss6(p).unwrap(),
            (6, Some(s)) => create_dss6_with(p, s).unwrap(),
            (_, None) => create_dss7(p).unwrap(),
            (_, Some(s)) => create_dss7_with(p, s).unwrap(),
        };
        // Creating a file: one flush, except in `None`.
        assert_eq!(flushes(|| create(&d.join(format!("c{v}.dss")), None)), 1);
        for (m, want) in [(SyncMode::None, 0), (SyncMode::End, 1), (SyncMode::Step, 1)] {
            let p = d.join(format!("c{v}_{}.dss", m.as_str()));
            assert_eq!(flushes(|| create(&p, Some(m))), want, "create v{v} {m:?}");
        }

        let mut step_counts = Vec::new();
        for m in [SyncMode::None, SyncMode::End, SyncMode::Step] {
            let f = d.join(format!("w{v}_{}.dss", m.as_str()));
            create(&f, Some(SyncMode::None));
            let o = opts(m);
            // A new record; a replacement; a multi-block series (two years of hourly
            // values: several blocks in one write call); paired data new and replaced.
            let counts = [
                flushes(|| rts(v, &f, b"/P72/A/FLOW//1HOUR/S/", 0.0, 48, &o)),
                flushes(|| rts(v, &f, b"/P72/A/FLOW//1HOUR/S/", 5.0, 48, &o)),
                flushes(|| rts(v, &f, b"/P72/B/FLOW//1HOUR/S/", 0.0, 2 * 8760, &o)),
                flushes(|| pd(v, &f, b"/P72/A/STAGE-FLOW///S/", 0.0, &o)),
                flushes(|| pd(v, &f, b"/P72/A/STAGE-FLOW///S/", 3.0, &o)),
            ];
            match m {
                SyncMode::None => assert_eq!(counts, [0; 5], "v{v} none"),
                // One flush per write call, whatever the number of blocks.
                SyncMode::End => assert_eq!(counts, [1; 5], "v{v} end"),
                SyncMode::Step => {
                    assert!(counts.iter().all(|&c| c > 3), "v{v} step {counts:?}");
                    step_counts = counts.to_vec();
                }
            }
        }
        eprintln!("flushes per write call, DSS{v}, step mode: {step_counts:?} (end: 1, none: 0)");

        // The single flush of a multi-record call (`SyncMode::batched`).
        let f = d.join(format!("b{v}.dss"));
        create(&f, Some(SyncMode::None));
        let (each, at_end) = SyncMode::End.batched();
        let n = flushes(|| {
            for i in 0..10 {
                let p = format!("/P72/R{i}/FLOW//1HOUR/S/");
                rts(v, &f, p.as_bytes(), 0.0, 24, &opts(each));
            }
            assert!(at_end);
            sync_path(&f).unwrap();
        });
        assert_eq!(n, 1, "v{v} batched");
    }
    // A path that does not exist is not an error, and is not flushed.
    assert_eq!(flushes(|| sync_path(&d.join("absent.dss")).unwrap()), 0);

    // The copy-out repair. `End` (the default of `repair_dss7`) and `Step`
    // flush the unpublished new file once, after its last record; `None` never. Up to
    // 0.1.1 it made two uncounted `sync_all` calls in every case. The published file
    // has the same records in every mode.
    let src = d.join("repair_src.dss");
    create_dss7_with(&src, SyncMode::None).unwrap();
    for i in 0..3 {
        let p = format!("/P72/REP{i}/FLOW//1HOUR/S/");
        rts(7, &src, p.as_bytes(), i as f64, 48, &opts(SyncMode::None));
    }
    pd(
        7,
        &src,
        b"/P72/REP/STAGE-FLOW///S/",
        1.0,
        &opts(SyncMode::None),
    );
    let mut listings = Vec::new();
    for (m, want) in [
        (None, 1),
        (Some(SyncMode::End), 1),
        (Some(SyncMode::Step), 1),
        (Some(SyncMode::None), 0),
    ] {
        let dest = d.join(format!(
            "repair_{}.dss",
            m.map_or("default", SyncMode::as_str)
        ));
        let mut report = None;
        let n = flushes(|| {
            report = Some(match m {
                None => repair_dss7(&src, &dest).unwrap(),
                Some(m) => repair_dss7_with(&src, &dest, m).unwrap(),
            });
        });
        let report = report.unwrap();
        assert_eq!(n, want, "repair {m:?}");
        assert!(report.complete(), "repair {m:?}");
        assert_eq!(report.records_copied, 4, "repair {m:?}");
        let mut r = dss_core::dss7::Dss7File::open(&dest).unwrap();
        let mut names: Vec<Vec<u8>> = r
            .catalog()
            .unwrap()
            .catalog
            .live()
            .map(|e| e.pathname.clone())
            .collect();
        names.sort();
        listings.push(names);
    }
    assert!(listings.windows(2).all(|w| w[0] == w[1]), "{listings:?}");
    let _ = std::fs::remove_dir_all(&d);
}

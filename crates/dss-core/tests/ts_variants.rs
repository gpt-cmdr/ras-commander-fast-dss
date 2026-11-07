//! P38: time-series writer variants (multi-word quality, integer and character notes,
//! patterns, pseudo-regular data). Round trips, the merge rules heclib applies, a word
//! replay of the HEC-written fixture `p38_hec_ts7.dss`, refusals that leave the file
//! unchanged, other records unchanged, and a crash test after every step. Synthetic data;
//! every write goes to a new file or a temporary copy of a public fixture.
//!
//! HEC oracle checks are local-only: `evidence/P38/REPORT.md`.

use std::path::{Path, PathBuf};

use dss_core::dss6::write as w6;
use dss_core::dss7::interval::ymd_to_julian;
use dss_core::dss7::write::{
    create_dss7, write_its, write_its_variants, write_pattern, write_rts, write_rts_variants,
    BlockOutcome, FaultPoint, ItsInput, PatternInput, RtsInput, RtsValues, TsNotes, TsVariants,
    WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::{Dss7File, RecordRef};
use dss_core::TsValues;

const DAY: i64 = 86_400;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ts_variants_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn at(y: i64, m: i64, d: i64, h: i64) -> i64 {
    ymd_to_julian(y, m, d) * DAY + h * 3600
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: false,
        ..WriteOptions::default()
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

fn open(p: &Path) -> Dss7File<std::fs::File> {
    Dss7File::new(std::fs::File::open(p).unwrap()).unwrap()
}

fn record(p: &Path, path: &[u8]) -> dss_core::dss7::Dss7Ts {
    let mut f = open(p);
    let r = f.lookup(path).unwrap().unwrap();
    f.read_ts_record(&r).unwrap()
}

fn floats(t: &dss_core::dss7::Dss7Ts) -> Vec<f32> {
    match &t.ts.values {
        TsValues::F32(v) => v.clone(),
        TsValues::F64(v) => v.iter().map(|&x| x as f32).collect(),
    }
}

fn rts<'a>(p: &'a [u8], start: i64, v: &'a [f32]) -> RtsInput<'a> {
    RtsInput::new(p, start, RtsValues::F32(v), b"CFS", b"INST-VAL")
}

fn its<'a>(p: &'a [u8], t: &'a [i64], v: &'a [f32]) -> ItsInput<'a> {
    ItsInput::new(p, t, RtsValues::F32(v), b"CFS", b"INST-VAL")
}

#[test]
fn regular_quality_words_round_trip_and_merge() {
    let d = tmp("q");
    let f = d.join("q.dss");
    create_dss7(&f).unwrap();
    let v: Vec<f32> = (0..24).map(|i| i as f32).collect();
    let q: Vec<i32> = (0..48).collect();
    let var = TsVariants {
        quality_words: Some((&q, 2)),
        notes: None,
    };
    let p = b"/P38/Q/FLOW//1Hour/A/";
    write_rts_variants(&f, &rts(p, at(2001, 1, 1, 1), &v), &var, &opts()).unwrap();
    let path = b"/P38/Q/FLOW/01Jan2001/1Hour/A/";
    let t = record(&f, path);
    assert_eq!(t.meta.quality_element_size, 2);
    let w = t.meta.quality_words.clone().unwrap();
    assert_eq!(w.len(), 744 * 2);
    assert_eq!(&w[..48], &q[..]);
    assert!(w[48..].iter().all(|&x| x == 0));

    // A partial rewrite merges into the stored words.
    let (v2, q2) = ([90.0f32, 91.0], [7, 8, 9, 10]);
    let var = TsVariants {
        quality_words: Some((&q2, 2)),
        notes: None,
    };
    write_rts_variants(&f, &rts(p, at(2001, 1, 1, 5), &v2), &var, &opts()).unwrap();
    let t = record(&f, path);
    let w = t.meta.quality_words.clone().unwrap();
    assert_eq!(&w[8..12], &q2[..]);
    assert_eq!(&w[..8], &q[..8]);
    assert_eq!(&w[12..48], &q[12..48]);
    assert_eq!(floats(&t)[4], 90.0);

    // Single-word quality into 2-word quality truncates, as convertDataLength does.
    let (v3, q3) = ([50.0f32], [70]);
    let mut i = rts(p, at(2001, 1, 1, 2), &v3);
    i.quality = Some(&q3);
    write_rts(&f, &i, &opts()).unwrap();
    let t = record(&f, path);
    assert_eq!(t.meta.quality_element_size, 1);
    assert_eq!(t.ts.quality.as_ref().unwrap()[..3], [0, 70, 4]);

    // Without quality the block drops it, as heclib does.
    write_rts(&f, &rts(p, at(2001, 1, 1, 3), &v3), &opts()).unwrap();
    assert!(record(&f, path).ts.quality.is_none());
}

#[test]
fn notes_round_trip_across_blocks() {
    let d = tmp("n");
    let f = d.join("n.dss");
    create_dss7(&f).unwrap();
    // Daily across two year blocks: 10 in 2001, 30 in 2002.
    let v: Vec<f32> = (0..40).map(|i| i as f32).collect();
    let owned: Vec<Vec<u8>> = (0..40)
        .map(|i| {
            if i % 5 == 4 {
                Vec::new()
            } else {
                format!("n{i}").into_bytes()
            }
        })
        .collect();
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&refs)),
    };
    let r = write_rts_variants(
        &f,
        &rts(b"/P38/N/FLOW//1Day/C/", at(2001, 12, 23, 0), &v),
        &var,
        &opts(),
    )
    .unwrap();
    assert_eq!(r.blocks.len(), 2);
    assert!(r.blocks.iter().all(|b| b.outcome == BlockOutcome::Added));
    let a = record(&f, b"/P38/N/FLOW/01Jan2001/1Day/C/");
    let c = a.meta.cnotes.as_ref().unwrap();
    assert_eq!(c.len(), 365);
    assert_eq!(c[355], "n0");
    assert_eq!(c[359], "");
    let b = record(&f, b"/P38/N/FLOW/01Jan2002/1Day/C/");
    let c2 = b.meta.cnotes.as_ref().unwrap();
    // 22 Dec + 10 days ends the year; the next note starts the next block.
    let stored_first = a.meta.cnotes.as_ref().unwrap()[355..].len();
    assert_eq!(c2[0], format!("n{stored_first}"));

    // Integer notes of 3 words with quality.
    let inotes: Vec<i32> = (0..120).map(|i| 7 * i - 500).collect();
    let q: Vec<i32> = (1..=40).collect();
    let var = TsVariants {
        quality_words: Some((&q, 1)),
        notes: Some(TsNotes::Int {
            words: &inotes,
            element_size: 3,
        }),
    };
    write_rts_variants(
        &f,
        &rts(b"/P38/N/FLOW//1Day/I/", at(2003, 1, 2, 0), &v),
        &var,
        &opts(),
    )
    .unwrap();
    let t = record(&f, b"/P38/N/FLOW/01Jan2003/1Day/I/");
    assert_eq!(t.meta.inotes_element_size, 3);
    assert_eq!(&t.meta.inotes.as_ref().unwrap()[..120], &inotes[..]);
    assert_eq!(&t.ts.quality.as_ref().unwrap()[..40], &q[..]);
}

#[test]
fn stored_character_notes_stay_on_a_write_without_notes() {
    let d = tmp("keep");
    let f = d.join("k.dss");
    create_dss7(&f).unwrap();
    let v = [1.0f32, 2.0, 3.0, 4.0];
    let notes: [&[u8]; 4] = [b"a", b"", b"c", b"d"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&notes)),
    };
    let p = b"/P38/K/FLOW//1Hour/A/";
    write_rts_variants(&f, &rts(p, at(2001, 1, 1, 1), &v), &var, &opts()).unwrap();
    write_rts(&f, &rts(p, at(2001, 1, 1, 2), &[9.0]), &opts()).unwrap();
    let t = record(&f, b"/P38/K/FLOW/01Jan2001/1Hour/A/");
    assert_eq!(floats(&t)[..4], [1.0, 9.0, 3.0, 4.0]);
    let c = t.meta.cnotes.as_ref().unwrap();
    assert_eq!(&c[..4], ["a", "", "c", "d"]);
}

/// Damage one stored area of `path`'s record: its user-header address and length.
fn damage_user_header(f: &Path, path: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let info = open(f).lookup(path).unwrap().unwrap().info_address;
    let mut h = std::fs::OpenOptions::new().write(true).open(f).unwrap();
    let k = dss_core::dss7::info::key::USER_HEAD_ADDRESS as i64;
    h.seek(SeekFrom::Start(((info + k) * 8) as u64)).unwrap();
    h.write_all(&999_999_999i64.to_le_bytes()).unwrap();
    h.write_all(&1i64.to_le_bytes()).unwrap();
}

/// P38 review 1: a stored area the commit would read is checked before the first store, so
/// damage in a later block refuses with the file byte-identical.
#[test]
fn later_block_damage_refuses_before_the_first_store() {
    let d = tmp("late");
    let q = [1, 2, 3, 4];
    let var = TsVariants {
        quality_words: Some((&q, 2)),
        notes: None,
    };
    let t = [at(2001, 1, 2, 1), at(2002, 1, 2, 1)];
    let f = d.join("i.dss");
    create_dss7(&f).unwrap();
    let p = b"/P38/L/FLOW//IR-Year/A/";
    write_its_variants(&f, &its(p, &t, &[1.0, 2.0]), &var, &opts()).unwrap();
    damage_user_header(&f, b"/P38/L/FLOW/01Jan2002/IR-Year/A/");
    let before = std::fs::read(&f).unwrap();
    assert!(matches!(
        write_its_variants(&f, &its(p, &t, &[9.0, 2.0]), &var, &opts()),
        Err(WriteError::Corrupt(_))
    ));
    assert_eq!(before, std::fs::read(&f).unwrap(), "irregular");
    // The plain P21 path goes through the same check.
    assert!(matches!(
        write_its(&f, &its(p, &t, &[9.0, 2.0]), &opts()),
        Err(WriteError::Corrupt(_))
    ));
    assert_eq!(before, std::fs::read(&f).unwrap(), "plain irregular");

    // Regular: daily values across two year blocks.
    let f = d.join("r.dss");
    create_dss7(&f).unwrap();
    let v = [1.0f32, 2.0];
    let p = b"/P38/L/FLOW//1Day/A/";
    let start = at(2001, 12, 31, 24);
    write_rts_variants(&f, &rts(p, start, &v), &var, &opts()).unwrap();
    damage_user_header(&f, b"/P38/L/FLOW/01Jan2002/1Day/A/");
    let before = std::fs::read(&f).unwrap();
    // The merge read of this block reports it (WriteError::Ts); either way, no store.
    assert!(write_rts_variants(&f, &rts(p, start, &[9.0, 2.0]), &var, &opts()).is_err());
    assert_eq!(before, std::fs::read(&f).unwrap(), "regular");
    assert!(write_rts(&f, &rts(p, start, &[9.0, 2.0]), &opts()).is_err());
    assert_eq!(before, std::fs::read(&f).unwrap(), "plain regular");
}

const NOTE_LIMIT: usize = 1 << 28;

/// P38 review 2: a note the reader would refuse is refused by the writer.
#[test]
fn note_over_the_readers_limit_is_refused() {
    let d = tmp("big");
    let f = d.join("b.dss");
    create_dss7(&f).unwrap();
    let before = std::fs::read(&f).unwrap();
    // With its NUL this is one byte over the limit.
    let note = vec![b'x'; NOTE_LIMIT];
    let n: [&[u8]; 1] = [&note];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&n)),
    };
    let t = [at(2001, 1, 2, 1)];
    let e = write_its_variants(
        &f,
        &its(b"/P38/B/FLOW//IR-Year/A/", &t, &[1.0]),
        &var,
        &opts(),
    );
    assert!(matches!(e, Err(WriteError::BadInput(_))));
    assert_eq!(before, std::fs::read(&f).unwrap());
}

/// The largest accepted note reads back. About 1 GB of memory, so run on request:
/// `cargo test --release --test ts_variants -- --ignored`.
#[test]
#[ignore]
fn note_at_the_readers_limit_reads_back() {
    let d = tmp("edge");
    let f = d.join("e.dss");
    create_dss7(&f).unwrap();
    let note = vec![b'x'; NOTE_LIMIT - 1];
    let n: [&[u8]; 1] = [&note];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&n)),
    };
    let t = [at(2001, 1, 2, 1)];
    write_its_variants(
        &f,
        &its(b"/P38/B/FLOW//IR-Year/A/", &t, &[1.0]),
        &var,
        &opts(),
    )
    .unwrap();
    let r = record(&f, b"/P38/B/FLOW/01Jan2001/IR-Year/A/");
    assert_eq!(r.meta.cnotes.as_ref().unwrap()[0].len(), NOTE_LIMIT - 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn integer_notes_over_stored_character_notes_are_refused() {
    let d = tmp("mix");
    let f = d.join("m.dss");
    create_dss7(&f).unwrap();
    let v = [1.0f32, 2.0];
    let notes: [&[u8]; 2] = [b"a", b"b"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&notes)),
    };
    let p = b"/P38/M/FLOW//1Hour/A/";
    write_rts_variants(&f, &rts(p, at(2001, 1, 1, 1), &v), &var, &opts()).unwrap();
    let before = std::fs::read(&f).unwrap();
    let ine = [1, 2];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Int {
            words: &ine,
            element_size: 1,
        }),
    };
    assert!(matches!(
        write_rts_variants(&f, &rts(p, at(2001, 1, 1, 1), &v), &var, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(before, std::fs::read(&f).unwrap());
}

/// Stored notes with empty leading notes, then a write that makes the leading values missing:
/// the first valid position is not 0, and every note must stay with its own position.
#[test]
fn kept_notes_stay_aligned_when_leading_values_go_missing() {
    let d = tmp("gap");
    let f = d.join("g.dss");
    create_dss7(&f).unwrap();
    let v = [1.0f32, 2.0, 3.0, 4.0];
    let notes: [&[u8]; 4] = [b"", b"", b"c", b"d"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&notes)),
    };
    let p = b"/P38/G/FLOW//1Hour/A/";
    write_rts_variants(&f, &rts(p, at(2001, 1, 1, 1), &v), &var, &opts()).unwrap();
    let m = -f32::MAX;
    write_rts(&f, &rts(p, at(2001, 1, 1, 1), &[m, m]), &opts()).unwrap();
    let t = record(&f, b"/P38/G/FLOW/01Jan2001/1Hour/A/");
    assert_eq!(floats(&t)[2..4], [3.0, 4.0]);
    assert_eq!(&t.meta.cnotes.as_ref().unwrap()[..4], ["", "", "c", "d"]);
}

#[test]
fn irregular_notes_merge_keeps_values_outside_the_window() {
    let d = tmp("irr");
    let f = d.join("i.dss");
    create_dss7(&f).unwrap();
    let t: Vec<i64> = (0..6).map(|i| at(2001, 1, 2 + i, 0)).collect();
    let v: Vec<f32> = (0..6).map(|i| i as f32).collect();
    let notes: [&[u8]; 6] = [b"r0", b"r1", b"r2", b"r3", b"r4", b"r5"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&notes)),
    };
    let p = b"/P38/I/FLOW//IR-Month/A/";
    write_its_variants(&f, &its(p, &t, &v), &var, &opts()).unwrap();
    // Replace positions 1..=3: r4 and r5 keep their own notes (the tested heclib build
    // associates them differently here).
    let new: [&[u8]; 3] = [b"u", b"v", b"w"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&new)),
    };
    let v2 = [10.0f32, 11.0, 12.0];
    write_its_variants(&f, &its(p, &t[1..4], &v2), &var, &opts()).unwrap();
    let r = record(&f, b"/P38/I/FLOW/01Jan2001/IR-Month/A/");
    assert_eq!(floats(&r), [0.0, 10.0, 11.0, 12.0, 4.0, 5.0]);
    let c = r.meta.cnotes.as_ref().unwrap();
    assert_eq!(c, &["r0", "u", "v", "w", "r4", "r5"]);

    // 2-word quality, then a plain write: quality dropped, notes kept with empty new ones.
    let q: Vec<i32> = (0..12).collect();
    let var = TsVariants {
        quality_words: Some((&q, 2)),
        notes: None,
    };
    let p = b"/P38/I/FLOW//IR-Month/B/";
    write_its_variants(&f, &its(p, &t, &v), &var, &opts()).unwrap();
    let r = record(&f, b"/P38/I/FLOW/01Jan2001/IR-Month/B/");
    assert_eq!(r.meta.quality_words.as_ref().unwrap(), &q);
    write_its(&f, &its(p, &t[..1], &[50.0]), &opts()).unwrap();
    let r = record(&f, b"/P38/I/FLOW/01Jan2001/IR-Month/B/");
    assert_eq!(r.meta.quality_element_size, 0);
    assert_eq!(floats(&r)[0], 50.0);
    assert_eq!(floats(&r).len(), 6);
}

#[test]
fn patterns_round_trip_and_refuse_other_types() {
    let d = tmp("pat");
    let f = d.join("p.dss");
    create_dss7(&f).unwrap();
    let v: Vec<f32> = (0..12).map(|i| i as f32 / 2.0).collect();
    let p = b"/P38/P/FLOW/TS-PATTERN/1MON/R/";
    let input = PatternInput::new(p, RtsValues::F32(&v), b"CFS", b"PER-AVER");
    let r = write_pattern(&f, &input, &opts()).unwrap();
    assert_eq!(r.blocks[0].record_type, 101);
    let path = b"/P38/P/FLOW/TS-PATTERN/1Month/R/";
    let t = record(&f, path);
    assert!(t.meta.pattern);
    assert_eq!(floats(&t), v);
    assert_eq!(t.meta.time_offset_seconds, 0);
    assert_eq!(t.ts.times[0], ymd_to_julian(3000, 1, 1) * DAY);
    let o = t.pattern_origin().unwrap();
    assert_eq!(o.stored_offset_seconds, Some(0));

    // A shorter rewrite replaces the whole record; equal values are a no-op.
    assert_eq!(
        write_pattern(&f, &input, &opts()).unwrap().blocks[0].outcome,
        BlockOutcome::Unchanged
    );
    let input2 = PatternInput::new(p, RtsValues::F32(&v[..3]), b"CFS", b"PER-AVER");
    write_pattern(&f, &input2, &opts()).unwrap();
    assert_eq!(floats(&record(&f, path)).len(), 3);

    // Doubles into a float pattern: HEC DIFFERENT_RECORD_TYPE, file unchanged.
    let before = std::fs::read(&f).unwrap();
    let dv = [1.0f64, 2.0];
    let input3 = PatternInput::new(p, RtsValues::F64(&dv), b"CFS", b"PER-AVER");
    assert!(matches!(
        write_pattern(&f, &input3, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(before, std::fs::read(&f).unwrap());

    // Irregular: the reader's times come back exactly.
    let times = [1800i64, 7200, 12_600];
    let mut input = PatternInput::new(
        b"/P38/P/FLOW/TS-PATTERN/IR-Day/I/",
        RtsValues::F64(&dv[..1]),
        b"CFS",
        b"INST-VAL",
    );
    input.times = Some(&times);
    assert!(matches!(
        write_pattern(&f, &input, &opts()),
        Err(WriteError::BadInput(_))
    ));
    let dv3 = [1.0f64, 2.0, 3.0];
    input.values = RtsValues::F64(&dv3);
    assert_eq!(
        write_pattern(&f, &input, &opts()).unwrap().blocks[0].record_type,
        116
    );
    let t = record(&f, b"/P38/P/FLOW/TS-PATTERN/IR-Day/I/");
    assert_eq!(t.ts.times, times);

    // Refusals before any file is touched.
    for bad in [
        &b"/P38/P/FLOW/TS-P/1Month/R/"[..],
        b"/P38/P/FLOW/TS-PATTERN/~1Day/R/",
        b"/P38/P/FLOW/01JAN2000/1Day/R/",
        b"/P38/P/FLOW/TS-PATTERN/1Da/R/",
    ] {
        let i = PatternInput::new(bad, RtsValues::F32(&v), b"CFS", b"PER-AVER");
        assert!(write_pattern(&f, &i, &opts()).is_err(), "{bad:?}");
    }
}

#[test]
fn pseudo_regular_stores_and_reads_as_irregular() {
    let d = tmp("ps");
    let f = d.join("p.dss");
    create_dss7(&f).unwrap();
    let t: Vec<i64> = (0..10).map(|i| at(2001, 12, 26, 0) + i * DAY).collect();
    let v: Vec<f32> = (0..10).map(|i| i as f32).collect();
    let r = write_its(&f, &its(b"/P38/S/FLOW//~1DAY/A/", &t, &v), &opts()).unwrap();
    assert_eq!(r.blocks.len(), 2);
    assert_eq!(r.blocks[0].record_type, 110);
    assert_eq!(
        r.blocks[0].pathname,
        b"/P38/S/FLOW/01Jan2001/~1Day/A/".to_vec()
    );
    let a = record(&f, b"/P38/S/FLOW/01Jan2001/~1Day/A/");
    let b = record(&f, b"/P38/S/FLOW/01Jan2002/~1Day/A/");
    assert_eq!(floats(&a).len() + floats(&b).len(), 10);

    // DSS6: stored as IR-YEAR, interval in the bin, read through the ~ name.
    let f6 = d.join("p6.dss");
    w6::create_dss6(&f6).unwrap();
    let input = w6::ItsInput {
        pathname: b"/P38/S/FLOW//~1DAY/A/",
        times: &t,
        values: RtsValues::F32(&v),
        quality: None,
        units: b"CFS",
        data_type: b"INST-VAL",
        mode: w6::ItsMode::Replace,
    };
    let r = w6::write_its6(&f6, &input, &opts()).unwrap();
    assert_eq!(
        r.blocks[0].pathname,
        b"/P38/S/FLOW/01JAN2001/IR-YEAR/A/".to_vec()
    );
    assert!(w6::check_file6(&f6).unwrap().problems.is_empty());
    // A rewrite keeps the interval (bin word KBPINTL); a plain IR- rewrite clears it.
    w6::write_its6(&f6, &input, &opts()).unwrap();
    let plain = w6::ItsInput {
        pathname: b"/P38/S/FLOW//IR-YEAR/A/",
        ..input.clone()
    };
    w6::write_its6(&f6, &plain, &opts()).unwrap();
    assert!(w6::check_file6(&f6).unwrap().problems.is_empty());
}

#[test]
fn dss6_patterns_and_refusals() {
    let d = tmp("p6");
    let f = d.join("p.dss");
    w6::create_dss6(&f).unwrap();
    let v: Vec<f32> = (0..12).map(|i| i as f32).collect();
    let mut i = PatternInput::new(
        b"/P38/P/FLOW/TS-PATTERN/1MON/R/",
        RtsValues::F32(&v),
        b"CFS",
        b"PER-AVER",
    );
    assert_eq!(
        w6::write_pattern6(&f, &i, &opts()).unwrap().blocks[0].record_type,
        101
    );
    assert_eq!(
        w6::write_pattern6(&f, &i, &opts()).unwrap().blocks[0].outcome,
        BlockOutcome::Unchanged
    );
    i.pathname = b"/P38/P/FLOW/TS-PATTERN/IR-YEAR/I/";
    let times: Vec<i64> = (0..12).map(|k| (k + 1) * 3600).collect();
    i.times = Some(&times);
    assert_eq!(
        w6::write_pattern6(&f, &i, &opts()).unwrap().blocks[0].record_type,
        111
    );
    assert!(w6::check_file6(&f).unwrap().problems.is_empty());
    let before = std::fs::read(&f).unwrap();
    let dv = [1.0f64];
    let bad = PatternInput::new(
        b"/P38/P/FLOW/TS-PATTERN/1MON/D/",
        RtsValues::F64(&dv),
        b"CFS",
        b"PER-AVER",
    );
    assert!(matches!(
        w6::write_pattern6(&f, &bad, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(before, std::fs::read(&f).unwrap());
}

#[test]
fn refused_write_changes_nothing() {
    let d = tmp("ref");
    let f = d.join("p.dss");
    std::fs::copy(fixture("synthetic_profile7.dss"), &f).unwrap();
    let mut rdr = open(&f);
    let cat = rdr.catalog().unwrap();
    let prof = cat
        .catalog
        .entries
        .iter()
        .find(|e| matches!(e.record_type, 102 | 107))
        .map(|e| e.pathname.clone())
        .unwrap();
    drop(rdr);
    let before = std::fs::read(&f).unwrap();
    let v = [1.0f32];
    let n: [&[u8]; 1] = [b"x"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&n)),
    };
    let start = {
        // Any time inside the stored block: the type check fires first.
        let parts: Vec<&[u8]> = prof.split(|&b| b == b'/').collect();
        let j = dss_core::dss7::interval::parse_date(parts[4]).unwrap();
        j * DAY + 3600
    };
    let e = write_rts_variants(&f, &rts(&prof, start, &v), &var, &opts());
    assert!(e.is_err());
    assert_eq!(before, std::fs::read(&f).unwrap());
    // Array checks run before the file is opened.
    let bad_q = [1, 2, 3];
    let var = TsVariants {
        quality_words: Some((&bad_q, 2)),
        notes: None,
    };
    let g = d.join("absent.dss");
    assert!(matches!(
        write_rts_variants(
            &g,
            &rts(b"/A/B/C//1Hour/F/", at(2001, 1, 1, 1), &v),
            &var,
            &opts()
        ),
        Err(WriteError::BadInput(_))
    ));
    assert!(!g.exists());
}

/// Every area of every record, decoded through the public catalog and raw reads.
fn all_records(p: &Path) -> Vec<(Vec<u8>, i32, Vec<u8>)> {
    let bytes = std::fs::read(p).unwrap();
    let mut f = open(p);
    let cat = f.catalog().unwrap();
    let mut out = Vec::new();
    for e in &cat.catalog.entries {
        let r: RecordRef = match f.lookup(&e.pathname).unwrap() {
            Some(r) => r,
            None => continue,
        };
        let mut b = Vec::new();
        for a in [
            &r.info.internal_header,
            &r.info.header2,
            &r.info.user_header,
            &r.info.values1,
            &r.info.values2,
            &r.info.values3,
        ] {
            let s = a.address as usize * 8;
            b.extend_from_slice(&bytes[s..s + a.number_ints.max(0) as usize * 4]);
        }
        b.extend_from_slice(&r.info.number_data.to_le_bytes());
        b.extend_from_slice(&r.info.logical_number.to_le_bytes());
        // In the tested sequence a pattern's info block has the dates of the handle's previous
        // store (`ztsStorePattern()` never sets them); the writer stores 0 (P38 REPORT §3).
        if !matches!(r.record_type, 101 | 106 | 111 | 116) {
            b.extend_from_slice(&r.info.first_date.0.to_le_bytes());
            b.extend_from_slice(&r.info.first_date.1.to_le_bytes());
            b.extend_from_slice(&r.info.last_date.0.to_le_bytes());
            b.extend_from_slice(&r.info.last_date.1.to_le_bytes());
        }
        out.push((r.pathname.clone(), r.record_type, b));
    }
    out.sort();
    out
}

#[test]
fn rewriting_hec_notes_leaves_other_records_unchanged() {
    let d = tmp("other");
    let f = d.join("n.dss");
    std::fs::copy(fixture("synthetic_notes_quality7.dss"), &f).unwrap();
    let before = all_records(&f);
    let target = b"/P32/NOTES/FLOW/01JAN2001/1Hour/CNOTES REG/".to_vec();
    let v = [55.0f32];
    let n: [&[u8]; 1] = [b"changed"];
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&n)),
    };
    let q = [1];
    let var = TsVariants {
        quality_words: Some((&q, 1)),
        ..var
    };
    let r = write_rts_variants(&f, &rts(&target, at(2001, 1, 1, 3), &v), &var, &opts()).unwrap();
    assert_eq!(r.blocks[0].outcome, BlockOutcome::Replaced);
    let after = all_records(&f);
    assert_eq!(before.len(), after.len());
    let mut changed = 0;
    for (a, b) in before.iter().zip(&after) {
        assert_eq!(a.0, b.0);
        if a != b {
            changed += 1;
            assert!(a.0.eq_ignore_ascii_case(&target));
        }
    }
    assert_eq!(changed, 1);
    let t = record(&f, &target);
    let c = t.meta.cnotes.as_ref().unwrap();
    assert_eq!(c[2], "changed");
    assert_eq!(c[0], "n1-0");
    // Every record of the file still decodes.
    let mut rdr = open(&f);
    for e in rdr.catalog().unwrap().catalog.entries.clone() {
        let r = rdr.lookup(&e.pathname).unwrap().unwrap();
        rdr.read_ts_record(&r).unwrap();
    }
}

struct Spec<'a>(std::str::SplitWhitespace<'a>);

impl<'a> Spec<'a> {
    fn s(&mut self) -> &'a str {
        self.0.next().unwrap()
    }
    fn n<T: std::str::FromStr>(&mut self) -> T
    where
        T::Err: std::fmt::Debug,
    {
        self.s().parse().unwrap()
    }
    fn vals(&mut self, kind: &str, n: usize) -> (Vec<f32>, Vec<f64>) {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for _ in 0..n {
            if kind == "float" {
                a.push(f32::from_bits(u32::from_str_radix(self.s(), 16).unwrap()));
            } else {
                b.push(f64::from_bits(u64::from_str_radix(self.s(), 16).unwrap()));
            }
        }
        (a, b)
    }
    fn notes(&mut self, n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|_| {
                let t = self.s();
                if t == "-" {
                    return Vec::new();
                }
                (0..t.len() / 2)
                    .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).unwrap())
                    .collect()
            })
            .collect()
    }
}

/// Replay the spec behind `p38_hec_ts7.dss` (written by heclib 7-JA, P38 §4) with the
/// native writer and compare every record's areas, counts and dates word for word.
#[test]
fn replay_of_the_hec_written_fixture_is_word_identical() {
    let spec = std::fs::read_to_string(fixture("p38_hec_ts7_spec.txt")).unwrap();
    let d = tmp("replay");
    let f = d.join("n.dss");
    create_dss7(&f).unwrap();
    let mut t = Spec(spec.split_whitespace());
    while let Some(kw) = t.0.next() {
        let kind = t.s();
        let path = t.s();
        let (date, time) = if kw == "STORE" {
            (t.s(), t.s())
        } else {
            ("", "")
        };
        let units = t.s();
        let dtype = t.s();
        let n: usize = t.n();
        let gran: i64 = if kw == "STORE" { 0 } else { t.n() };
        let (qes, nes, cn): (usize, usize, usize) = if kw == "PATTERN" {
            (0, 0, 0)
        } else {
            (t.n(), t.n(), t.n())
        };
        let times: Vec<i64> = if kw == "IRREG" || gran > 0 {
            (0..n).map(|_| t.n()).collect()
        } else {
            Vec::new()
        };
        let v = t.vals(kind, n);
        let values = if kind == "float" {
            RtsValues::F32(&v.0)
        } else {
            RtsValues::F64(&v.1)
        };
        let q: Vec<i32> = (0..n * qes).map(|_| t.n()).collect();
        let inotes: Vec<i32> = (0..n * nes).map(|_| t.n()).collect();
        let notes = if cn == 1 { t.notes(n) } else { Vec::new() };
        let refs: Vec<&[u8]> = notes.iter().map(Vec::as_slice).collect();
        let var = TsVariants {
            quality_words: (qes > 0).then_some((&q[..], qes)),
            notes: if cn == 1 {
                Some(TsNotes::Char(&refs))
            } else if nes > 0 {
                Some(TsNotes::Int {
                    words: &inotes,
                    element_size: nes,
                })
            } else {
                None
            },
        };
        match kw {
            "STORE" => {
                let j = dss_core::dss7::interval::parse_date(date.as_bytes()).unwrap();
                let hhmm: i64 = time.parse().unwrap();
                let start = j * DAY + hhmm / 100 * 3600 + hhmm % 100 * 60;
                let i = RtsInput::new(
                    path.as_bytes(),
                    start,
                    values,
                    units.as_bytes(),
                    dtype.as_bytes(),
                );
                write_rts_variants(&f, &i, &var, &opts()).unwrap();
            }
            "IRREG" => {
                let mut i = ItsInput::new(
                    path.as_bytes(),
                    &times,
                    values,
                    units.as_bytes(),
                    dtype.as_bytes(),
                );
                i.time_granularity = Some(gran);
                write_its_variants(&f, &i, &var, &opts()).unwrap();
            }
            _ => {
                let mut i =
                    PatternInput::new(path.as_bytes(), values, units.as_bytes(), dtype.as_bytes());
                if gran > 0 {
                    i.times = Some(&times);
                    i.time_granularity = Some(gran);
                }
                write_pattern(&f, &i, &opts()).unwrap();
            }
        }
    }
    let a = all_records(&fixture("p38_hec_ts7.dss"));
    let b = all_records(&f);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x, y, "{}", String::from_utf8_lossy(&x.0));
    }
}

/// A faulted write leaves the old record or the new one, and the next write repairs it.
#[test]
fn crash_after_every_step() {
    let d = tmp("crash");
    let v: Vec<f32> = (0..24).map(|i| i as f32).collect();
    let n: Vec<Vec<u8>> = (0..24).map(|i| format!("n{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = n.iter().map(Vec::as_slice).collect();
    let var = TsVariants {
        quality_words: None,
        notes: Some(TsNotes::Char(&refs)),
    };
    let pn = b"/P38/C/FLOW//1Hour/A/";
    let stored = b"/P38/C/FLOW/01Jan2001/1Hour/A/";
    let start = at(2001, 1, 1, 1);
    let pat = b"/P38/C/FLOW/TS-PATTERN/1Hour/P/";
    for (tag, seed) in [("add", false), ("rewrite", true)] {
        for s in WriteStep::ALL {
            let f = d.join(format!("{tag}_{s:?}.dss"));
            create_dss7(&f).unwrap();
            let other: Vec<f32> = vec![5.0; 3];
            write_rts(
                &f,
                &rts(b"/P38/C/FLOW//1Hour/OTHER/", start, &other),
                &opts(),
            )
            .unwrap();
            if seed {
                write_rts(&f, &rts(pn, start, &other), &opts()).unwrap();
            }
            let mut o = opts();
            o.fault = Some(FaultPoint { block: 0, step: s });
            let e = write_rts_variants(&f, &rts(pn, start, &v), &var, &o);
            assert!(
                matches!(e, Err(WriteError::InjectedFault { .. })),
                "{tag} {s:?}"
            );
            // The other record is intact; the target reads as old or new, never damaged.
            assert_eq!(
                floats(&record(&f, b"/P38/C/FLOW/01Jan2001/1Hour/OTHER/"))[..3],
                [5.0; 3]
            );
            let mut rdr = open(&f);
            if let Some(r) = rdr.lookup(stored).unwrap() {
                let t = rdr.read_ts_record(&r).unwrap();
                let first = floats(&t)[0];
                assert!(first == 5.0 || first == 0.0, "{tag} {s:?} {first}");
            }
            drop(rdr);
            // The write again: repaired and complete.
            let r = write_rts_variants(&f, &rts(pn, start, &v), &var, &opts()).unwrap();
            let marked = !matches!(
                s,
                WriteStep::Data | WriteStep::EndOfFile | WriteStep::Finish
            );
            assert_eq!(r.reconciled_interrupted_write, marked, "{tag} {s:?}");
            let t = record(&f, stored);
            assert_eq!(t.meta.cnotes.as_ref().unwrap()[23], "n23");
            // Patterns use the same transaction.
            let f2 = d.join(format!("{tag}_{s:?}_p.dss"));
            create_dss7(&f2).unwrap();
            let i = PatternInput::new(pat, RtsValues::F32(&v), b"CFS", b"PER-AVER");
            if seed {
                let i0 = PatternInput::new(pat, RtsValues::F32(&other), b"CFS", b"PER-AVER");
                write_pattern(&f2, &i0, &opts()).unwrap();
            }
            let e = write_pattern(&f2, &i, &o);
            assert!(matches!(e, Err(WriteError::InjectedFault { .. })));
            let r = write_pattern(&f2, &i, &opts()).unwrap();
            assert_eq!(r.reconciled_interrupted_write, marked);
            assert_eq!(floats(&record(&f2, pat)).len(), 24);
        }
    }
    // DSS6: pseudo-regular and pattern.
    let t: Vec<i64> = (0..10).map(|i| at(2001, 1, 2, 0) + i * DAY).collect();
    let v10 = &v[..10];
    for s in w6::DSS6_STEP_ORDER {
        let f = d.join(format!("six_{s:?}.dss"));
        w6::create_dss6(&f).unwrap();
        let input = w6::ItsInput {
            pathname: b"/P38/C/FLOW//~1DAY/A/",
            times: &t,
            values: RtsValues::F32(v10),
            quality: None,
            units: b"CFS",
            data_type: b"INST-VAL",
            mode: w6::ItsMode::Replace,
        };
        let mut o = opts();
        o.fault = Some(FaultPoint { block: 0, step: s });
        assert!(matches!(
            w6::write_its6(&f, &input, &o),
            Err(WriteError::InjectedFault { .. })
        ));
        w6::write_its6(&f, &input, &opts()).unwrap();
        assert!(w6::check_file6(&f).unwrap().problems.is_empty(), "{s:?}");
        let p = PatternInput::new(
            b"/P38/C/FLOW/TS-PATTERN/1DAY/P/",
            RtsValues::F32(v10),
            b"CFS",
            b"PER-AVER",
        );
        assert!(matches!(
            w6::write_pattern6(&f, &p, &o),
            Err(WriteError::InjectedFault { .. })
        ));
        w6::write_pattern6(&f, &p, &opts()).unwrap();
        assert!(w6::check_file6(&f).unwrap().problems.is_empty(), "{s:?}");
    }
}

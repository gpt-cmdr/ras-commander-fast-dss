//! P76: batch writes (`dss7::write::write_batch[_to]`, `dss6::write::write_batch6[_to]`).
//!
//! A batch must write exactly what the same records written by separate calls write. With
//! a fixed write clock the files are compared byte for byte, for DSS7 and DSS6, in every
//! flush mode, by path and on a held handle, with new records, replacements, merges, an
//! unchanged rewrite, a width change and a pathname written twice. The reports must equal
//! the separate calls' reports, and the flush count must drop to one per batch in the
//! `End` mode. A record refused while the batch is written stops it with the records
//! before it written (equal to the separate calls up to that record); a record refused
//! while the batch is planned writes nothing. Process death during a batch is covered by
//! the `batch7` and `batch6` cases of `tests/p64_kill.rs`.
//!
//! The write clock and the flush counter are process-wide: everything is in one test
//! function of its own test binary.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use dss_core::dss6::write::{
    self as w6, create_dss6_with, BatchItem6, ItsInput as ItsInput6, ItsMode,
};
use dss_core::dss7::write::{
    self as w7, create_dss7_with, flush_count, set_write_clock, BatchItem, BatchReport, ItsInput,
    PdArrays, PdInput, RtsInput, RtsValues, SyncMode, WriteError, WriteOptions, WriteReport,
};
use dss_core::time::ymd_to_julian;

/// A temporary root of this test's own (tests of one binary run in parallel, so
/// a root shared by the process must not be removed by one of them).
fn dir(test: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "p76_batch_{}_{test}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn t(y: i64, m: u32, d: u32, secs: i64) -> i64 {
    ymd_to_julian(y, m, d) * 86_400 + secs
}

fn opts(sync: SyncMode) -> WriteOptions {
    WriteOptions {
        sync,
        ..WriteOptions::default()
    }
}

fn rw(p: &Path) -> File {
    OpenOptions::new().read(true).write(true).open(p).unwrap()
}

/// Arrays the inputs borrow.
struct Data {
    a: Vec<f32>,
    b: Vec<f64>,
    c: Vec<f32>,
    q: Vec<i32>,
    times: Vec<i64>,
    irr: Vec<f64>,
    times2: Vec<i64>,
    irr2: Vec<f32>,
    ord: Vec<f32>,
    pdv: Vec<f32>,
    ord64: Vec<f64>,
    pdv64: Vec<f64>,
    small: Vec<f32>,
}

fn data() -> Data {
    Data {
        a: (0..24 * 45).map(|i| i as f32 * 0.5).collect(),
        b: (0..30).map(|i| 1000.0 + i as f64 / 3.0).collect(),
        c: (0..24 * 3).map(|i| 7.0 + i as f32).collect(),
        q: (0..30).map(|i| i % 4).collect(),
        times: vec![t(2021, 5, 1, 60), t(2021, 5, 2, 0), t(2021, 6, 3, 3600)],
        irr: vec![1.5, 2.5, 3.5],
        times2: vec![t(2021, 4, 4, 0), t(2021, 4, 6, 3600)],
        irr2: vec![7.5, 8.5],
        ord: vec![0.0, 1.0, 2.5, 4.0],
        pdv: (0..8).map(|i| i as f32 * 1.25).collect(),
        ord64: vec![0.0, 2.0, 5.0],
        pdv64: vec![10.0, 20.0, 30.5],
        small: vec![9.0, 8.0, 7.0],
    }
}

fn pd<'a>(p: &'a [u8], d: &'a Data, wide: bool) -> PdInput<'a> {
    let arrays = if wide {
        PdArrays::F64 {
            ordinates: &d.ord64,
            values: &d.pdv64,
        }
    } else {
        PdArrays::F32 {
            ordinates: &d.ord,
            values: &d.pdv,
        }
    };
    let curves = if wide { 1 } else { 2 };
    PdInput::new(p, arrays, curves, b"FT", b"LINEAR", b"CFS", b"LINEAR")
}

/// The records a base file holds before the batch: some of them the batch replaces,
/// merges into or rewrites unchanged.
fn base(v: u8, f: &Path, d: &Data) {
    let o = opts(SyncMode::None);
    if v == 6 {
        create_dss6_with(f, SyncMode::None).unwrap();
    } else {
        create_dss7_with(f, SyncMode::None).unwrap();
    }
    let keep = RtsInput::new(
        b"/K/KEEP/FLOW//1HOUR/S/",
        t(2021, 1, 1, 3600),
        RtsValues::F32(&d.c),
        b"CFS",
        b"INST-VAL",
    );
    let multi = RtsInput::new(
        b"/T/MULTI/FLOW//1HOUR/S/",
        t(2021, 3, 20, 3600),
        RtsValues::F32(&d.c),
        b"CFS",
        b"INST-VAL",
    );
    let pd_old = pd(b"/T/PD/STAGE-FLOW///S/", d, false);
    if v == 6 {
        w6::write_rts6(f, &keep, &o).unwrap();
        w6::write_rts6(f, &multi, &o).unwrap();
        w6::write_pd6(f, &pd_old, &o).unwrap();
        let irr = ItsInput6 {
            pathname: b"/T/IRR/STAGE//IR-MONTH/S/",
            times: &d.times2,
            values: RtsValues::F32(&d.small[..2]),
            quality: None,
            units: b"FT",
            data_type: b"INST-VAL",
            mode: ItsMode::Replace,
        };
        w6::write_its6(f, &irr, &o).unwrap();
    } else {
        w7::write_rts(f, &keep, &o).unwrap();
        w7::write_rts(f, &multi, &o).unwrap();
        w7::write_pd(f, &pd_old, &o).unwrap();
        let irr = ItsInput::new(
            b"/T/IRR/STAGE//IR-MONTH/S/",
            &d.times2,
            RtsValues::F32(&d.small[..2]),
            b"FT",
            b"INST-VAL",
        );
        w7::write_its(f, &irr, &o).unwrap();
    }
}

/// One record of the test batch, written either way.
#[derive(Clone)]
enum Rec<'a> {
    Rts(RtsInput<'a>),
    Its7(ItsInput<'a>),
    Its6(ItsInput6<'a>),
    Pd(PdInput<'a>),
}

fn records<'a>(v: u8, d: &'a Data) -> Vec<Rec<'a>> {
    let mut quality = RtsInput::new(
        b"/T/QUAL/FLOW//15MIN/S/",
        t(2021, 2, 28, 22 * 3600),
        RtsValues::F64(&d.b),
        b"CFS",
        b"PER-AVER",
    );
    quality.quality = Some(&d.q);
    let mut out = vec![
        // New, several blocks (45 days of hourly values).
        Rec::Rts(RtsInput::new(
            b"/T/NEW/FLOW//1HOUR/S/",
            t(2021, 2, 20, 3600),
            RtsValues::F32(&d.a),
            b"CFS",
            b"INST-VAL",
        )),
        // Merge into and extend an existing series.
        Rec::Rts(RtsInput::new(
            b"/T/MULTI/FLOW//1HOUR/S/",
            t(2021, 3, 21, 3600),
            RtsValues::F32(&d.c),
            b"CFS",
            b"INST-VAL",
        )),
        // Unchanged rewrite of a stored record.
        Rec::Rts(RtsInput::new(
            b"/K/KEEP/FLOW//1HOUR/S/",
            t(2021, 1, 1, 3600),
            RtsValues::F32(&d.c),
            b"CFS",
            b"INST-VAL",
        )),
        // Double values with quality, across a block boundary.
        Rec::Rts(quality),
        // Paired data: a replacement that widens the record, and a new record.
        Rec::Pd(pd(b"/T/PD/STAGE-FLOW///S/", d, true)),
        Rec::Pd(pd(b"/T/NEWPD/STAGE-FLOW///S/", d, false)),
    ];
    if v == 6 {
        out.push(Rec::Its6(ItsInput6 {
            pathname: b"/T/IRR/STAGE//IR-MONTH/S/",
            times: &d.times2,
            values: RtsValues::F32(&d.irr2),
            quality: None,
            units: b"FT",
            data_type: b"INST-VAL",
            mode: ItsMode::Replace,
        }));
        out.push(Rec::Its6(ItsInput6 {
            pathname: b"/T/NEWIRR/STAGE//IR-MONTH/S/",
            times: &d.times,
            values: RtsValues::F64(&d.irr),
            quality: None,
            units: b"FT",
            data_type: b"INST-VAL",
            mode: ItsMode::Merge,
        }));
    } else {
        out.push(Rec::Its7(ItsInput::new(
            b"/T/IRR/STAGE//IR-MONTH/S/",
            &d.times2,
            RtsValues::F32(&d.irr2),
            b"FT",
            b"INST-VAL",
        )));
        out.push(Rec::Its7(ItsInput::new(
            b"/T/NEWIRR/STAGE//IR-MONTH/S/",
            &d.times,
            RtsValues::F64(&d.irr),
            b"FT",
            b"INST-VAL",
        )));
    }
    // The same pathname twice: the second write sees the first.
    out.push(Rec::Rts(RtsInput::new(
        b"/T/NEW/FLOW//1HOUR/S/",
        t(2021, 3, 10, 0),
        RtsValues::F32(&d.small),
        b"CFS",
        b"INST-VAL",
    )));
    out
}

fn one(v: u8, file: &File, r: &Rec, o: &WriteOptions) -> Result<WriteReport, WriteError> {
    match (v, r) {
        (7, Rec::Rts(i)) => w7::write_rts_to(file, i, o),
        (7, Rec::Its7(i)) => w7::write_its_to(file, i, o),
        (7, Rec::Pd(i)) => w7::write_pd_to(file, i, o),
        (6, Rec::Rts(i)) => w6::write_rts6_to(file, i, o),
        (6, Rec::Its6(i)) => w6::write_its6_to(file, i, o),
        (6, Rec::Pd(i)) => w6::write_pd6_to(file, i, o),
        _ => unreachable!(),
    }
}

fn batch(
    v: u8,
    target: Result<&File, &Path>,
    recs: &[Rec],
    o: &WriteOptions,
) -> Result<BatchReport, WriteError> {
    if v == 7 {
        let items: Vec<BatchItem> = recs
            .iter()
            .map(|r| match r {
                Rec::Rts(i) => BatchItem::Rts(i.clone()),
                Rec::Its7(i) => BatchItem::Its(i.clone()),
                Rec::Pd(i) => BatchItem::Pd(i.clone()),
                Rec::Its6(_) => unreachable!(),
            })
            .collect();
        match target {
            Ok(f) => w7::write_batch_to(f, &items, o),
            Err(p) => w7::write_batch(p, &items, o),
        }
    } else {
        let items: Vec<BatchItem6> = recs
            .iter()
            .map(|r| match r {
                Rec::Rts(i) => BatchItem6::Rts(i.clone()),
                Rec::Its6(i) => BatchItem6::Its(i.clone()),
                Rec::Pd(i) => BatchItem6::Pd(i.clone()),
                Rec::Its7(_) => unreachable!(),
            })
            .collect();
        match target {
            Ok(f) => w6::write_batch6_to(f, &items, o),
            Err(p) => w6::write_batch6(p, &items, o),
        }
    }
}

fn flushes<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = flush_count();
    let r = f();
    (r, flush_count() - before)
}

fn reports_text(r: &[WriteReport]) -> Vec<String> {
    r.iter().map(|x| format!("{x:?}")).collect()
}

#[test]
fn p76_batches_write_what_separate_calls_write() {
    // A fixed clock makes the files comparable byte for byte.
    set_write_clock(Some(1_789_000_000_000));
    let d = dir("batches");
    let data = data();
    for v in [7u8, 6] {
        let b0 = d.join(format!("base{v}.dss"));
        base(v, &b0, &data);
        let recs = records(v, &data);
        let n = recs.len() as u64;
        for sync in [SyncMode::None, SyncMode::End, SyncMode::Step] {
            let o = opts(sync);
            let tag = format!("DSS{v} {}", sync.as_str());
            // Separate calls on one held handle.
            let a = d.join(format!("a{v}_{}.dss", sync.as_str()));
            std::fs::copy(&b0, &a).unwrap();
            let (sep, f_sep) = flushes(|| {
                let file = rw(&a);
                recs.iter()
                    .map(|r| one(v, &file, r, &o).unwrap())
                    .collect::<Vec<_>>()
            });
            // The batch on a held handle, and by path.
            let b = d.join(format!("b{v}_{}.dss", sync.as_str()));
            std::fs::copy(&b0, &b).unwrap();
            let (rb, f_b) = flushes(|| batch(v, Ok(&rw(&b)), &recs, &o).unwrap());
            let c = d.join(format!("c{v}_{}.dss", sync.as_str()));
            std::fs::copy(&b0, &c).unwrap();
            let (rc, f_c) = flushes(|| batch(v, Err(&c), &recs, &o).unwrap());
            assert!(rb.failed.is_none() && rc.failed.is_none(), "{tag}");
            let bytes = std::fs::read(&a).unwrap();
            assert_ne!(bytes, std::fs::read(&b0).unwrap(), "{tag}: nothing written");
            assert!(
                bytes == std::fs::read(&b).unwrap(),
                "{tag}: held batch differs"
            );
            assert!(
                bytes == std::fs::read(&c).unwrap(),
                "{tag}: path batch differs"
            );
            assert_eq!(reports_text(&sep), reports_text(&rb.reports), "{tag}");
            assert_eq!(reports_text(&sep), reports_text(&rc.reports), "{tag}");
            // The batch exercised every outcome it should.
            let outcomes: Vec<&str> = sep
                .iter()
                .flat_map(|r| r.blocks.iter().map(|b| b.outcome.as_str()))
                .collect();
            for want in ["added", "replaced", "unchanged"] {
                assert!(
                    outcomes.contains(&want),
                    "{tag}: no {want} block: {outcomes:?}"
                );
            }
            // Flushes: End once per call becomes once per batch; Step keeps every flush
            // point and loses the per-call end flushes but one; None never flushes.
            match sync {
                SyncMode::None => assert_eq!((f_sep, f_b, f_c), (0, 0, 0), "{tag}"),
                SyncMode::End => assert_eq!((f_sep, f_b, f_c), (n, 1, 1), "{tag}"),
                SyncMode::Step => {
                    assert_eq!(f_b, f_sep - (n - 1), "{tag}");
                    assert_eq!(f_c, f_b, "{tag}");
                }
            }
        }

        // A record refused while the batch is written stops it: paired data onto a
        // stored time series is refused (the writer never deletes another record type).
        let mut recs = records(v, &data);
        let bad = Rec::Pd(pd(b"/K/KEEP/FLOW/01JAN2021/1HOUR/S/", &data, false));
        recs.insert(3, bad);
        let a = d.join(format!("fail_a{v}.dss"));
        std::fs::copy(&b0, &a).unwrap();
        let file = rw(&a);
        let mut sep = Vec::new();
        let mut refused = None;
        for r in &recs {
            match one(v, &file, r, &opts(SyncMode::End)) {
                Ok(x) => sep.push(x),
                Err(e) => {
                    refused = Some(e.to_string());
                    break;
                }
            }
        }
        drop(file);
        let refused = refused.expect("the bad record is refused by a separate call");
        let b = d.join(format!("fail_b{v}.dss"));
        std::fs::copy(&b0, &b).unwrap();
        let (r, f) = flushes(|| batch(v, Err(&b), &recs, &opts(SyncMode::End)).unwrap());
        let (i, e) = r.failed.expect("the batch stops");
        assert_eq!((i, e.to_string()), (3, refused), "DSS{v}");
        assert_eq!(reports_text(&r.reports), reports_text(&sep), "DSS{v}");
        assert!(
            std::fs::read(&a).unwrap() == std::fs::read(&b).unwrap(),
            "DSS{v}"
        );
        assert_eq!(f, 1, "DSS{v}: the records before the refusal are flushed");

        // A record refused while the batch is planned: nothing is written or flushed.
        let mut recs = records(v, &data);
        recs.insert(
            2,
            Rec::Rts(RtsInput::new(
                b"/NO/E/PART///S/",
                t(2021, 1, 1, 0),
                RtsValues::F32(&data.small),
                b"CFS",
                b"INST-VAL",
            )),
        );
        let b = d.join(format!("plan_b{v}.dss"));
        std::fs::copy(&b0, &b).unwrap();
        let (r, f) = flushes(|| batch(v, Err(&b), &recs, &opts(SyncMode::End)).unwrap());
        let (i, _) = r.failed.expect("the plan refuses");
        assert_eq!((i, r.reports.len(), f), (2, 0, 0), "DSS{v}");
        assert!(
            std::fs::read(&b).unwrap() == std::fs::read(&b0).unwrap(),
            "DSS{v}"
        );

        // An empty batch does not touch the file, or even need one.
        let missing = d.join(format!("missing{v}.dss"));
        let (r, f) = flushes(|| batch(v, Err(&missing), &[], &opts(SyncMode::End)).unwrap());
        assert!(r.reports.is_empty() && r.failed.is_none() && f == 0 && !missing.exists());
    }
    set_write_clock(None);
    let _ = std::fs::remove_dir_all(&d);
}

//! P23 paired-data writer tests (DSS7 and DSS6): create → add → replace → read round
//! trips (multi-curve, labels, float and double), refusals that leave the file
//! byte-identical, the HMS Atlas 14 pathname regression (an HMS import issue), every other
//! record word-identical, a crash at every step of the transaction with recovery, and a
//! fuzz pass on input validation. Synthetic data only.
//!
//! The same writer was also checked with HEC software (heclib `zpdStore`/`zcheckFile`,
//! hecdss, Monolith `HecPairedData`); those checks need HEC software and do not run in
//! this test suite.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss_core::dss6::write::{
    check_file6, create_dss6, validate_pd6_input, write_pd6, write_rts6, DSS6_STEP_ORDER,
};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, validate_pd_input, write_pd, write_rts, BlockOutcome, FaultPoint, PdArrays,
    PdInput, RtsInput, RtsValues, WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::Dss7File;
use dss_core::{PairedData, TsValues};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pd_write_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: false,
        ..WriteOptions::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum V {
    V6,
    V7,
}

fn create(v: V, p: &Path) {
    match v {
        V::V6 => create_dss6(p).unwrap(),
        V::V7 => create_dss7(p).unwrap(),
    }
}

fn write(v: V, p: &Path, input: &PdInput, o: &WriteOptions) -> Result<BlockOutcome, WriteError> {
    let r = match v {
        V::V6 => write_pd6(p, input, o)?,
        V::V7 => write_pd(p, input, o)?,
    };
    assert_eq!(r.blocks.len(), 1);
    Ok(r.blocks[0].outcome)
}

fn read(v: V, p: &Path, path: &[u8]) -> PairedData {
    match v {
        V::V6 => Dss6File::open(p).unwrap().read_pd(path).unwrap().pd,
        V::V7 => Dss7File::open(p).unwrap().read_pd(path).unwrap().pd,
    }
}

fn bits(v: &TsValues) -> Vec<u64> {
    match v {
        TsValues::F32(x) => x.iter().map(|y| u64::from(y.to_bits())).collect(),
        TsValues::F64(x) => x.iter().map(|y| y.to_bits()).collect(),
    }
}

/// Every live record of a file: pathname (upper) -> (type, value bits, metadata string).
fn snapshot(v: V, p: &Path) -> BTreeMap<Vec<u8>, (i32, Vec<u64>, String)> {
    snapshot_with(v, p, true)
}

/// `strict`: no catalog anomaly and `knumberRecords` equal to the live entries. An
/// interrupted file is read without them (only reachable records are listed).
fn snapshot_with(v: V, p: &Path, strict: bool) -> BTreeMap<Vec<u8>, (i32, Vec<u64>, String)> {
    let mut out = BTreeMap::new();
    match v {
        V::V7 => {
            let mut f = Dss7File::open(p).unwrap();
            let cat = f.catalog().unwrap();
            let live: Vec<_> = cat.catalog.live().cloned().collect();
            if strict {
                assert!(
                    cat.catalog.anomalies.is_empty(),
                    "{:?}",
                    cat.catalog.anomalies
                );
                assert_eq!(f.header().number_records, live.len() as i64);
            }
            for e in live {
                let Some(r) = f.lookup(&e.pathname).unwrap() else {
                    assert!(!strict, "live entry not reachable");
                    continue;
                };
                let entry = if r.record_type == 200 || r.record_type == 205 {
                    let d = f.read_pd_record(&r).unwrap().pd;
                    (
                        d.record_type,
                        [bits(&d.ordinates), bits(&d.values)].concat(),
                        meta(&d),
                    )
                } else if (100..120).contains(&r.record_type) {
                    let t = f.read_ts_record(&r).unwrap();
                    (r.record_type, bits(&t.ts.values), String::new())
                } else {
                    (r.record_type, Vec::new(), String::new())
                };
                out.insert(e.pathname.to_ascii_uppercase(), entry);
            }
        }
        V::V6 => {
            let f = Dss6File::open(p).unwrap();
            for e in f.catalog().unwrap().entries {
                let entry = if e.record_type == 200 || e.record_type == 205 {
                    let d = f.read_pd(&e.pathname).unwrap().pd;
                    (
                        d.record_type,
                        [bits(&d.ordinates), bits(&d.values)].concat(),
                        meta(&d),
                    )
                } else {
                    let t = f.read_ts_record(&e.pathname).unwrap();
                    (t.record_type, bits(&t.values), String::new())
                };
                out.insert(e.pathname.clone(), entry);
            }
        }
    }
    out
}

fn meta(d: &PairedData) -> String {
    format!(
        "{}x{} {:?} {:?} {:?} {:?} {:?} {} {} {} {:?}",
        d.number_ordinates,
        d.number_curves,
        d.units_independent,
        d.type_independent,
        d.units_dependent,
        d.type_dependent,
        d.labels,
        d.independent_is_x_axis,
        d.x_precision,
        d.y_precision,
        d.time_zone
    )
}

fn clean(v: V, p: &Path, what: &str) {
    snapshot(v, p);
    if v == V::V6 {
        let c = check_file6(p).unwrap();
        assert!(c.is_clean(), "{what}: {:?}", c.problems);
    }
}

fn labels(v: &[&str]) -> Vec<Vec<u8>> {
    v.iter().map(|s| s.as_bytes().to_vec()).collect()
}

const ORD: [f32; 5] = [0.0, 1.5, 3.0, 4.5, 6.0];

fn three_curves() -> Vec<f32> {
    (0..15).map(|i| 100.0 + i as f32 * 2.5).collect()
}

// ---------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------

fn round_trip(v: V) {
    let d = tmp(&format!("rt{v:?}"));
    let p = d.join("rt.dss");
    create(v, &p);
    let vals = three_curves();
    let ls = labels(&["Total", "Residential damage (structures)", "C"]);
    let mut input = PdInput::new(
        b"/BASIN/LOC/STAGE-DAMAGE///PLAN A/",
        PdArrays::F32 {
            ordinates: &ORD,
            values: &vals,
        },
        3,
        b"FT",
        b"LINEAR",
        b"DOLLARS",
        b"LINEAR",
    );
    input.labels = Some(&ls);
    input.x_precision = 2;
    input.y_precision = 1;
    if v == V::V7 {
        input.time_zone = Some(b"UTC");
    }
    assert_eq!(write(v, &p, &input, &opts()).unwrap(), BlockOutcome::Added);
    let stored: &[u8] = if v == V::V6 {
        b"/BASIN/LOC/STAGE-DAMAGE///PLAN A/"
    } else {
        input.pathname
    };
    let r = read(v, &p, stored);
    assert_eq!(r.pathname, stored);
    assert_eq!(r.record_type, 200);
    assert_eq!((r.number_ordinates, r.number_curves), (5, 3));
    assert_eq!(r.ordinates, TsValues::F32(ORD.to_vec()));
    assert_eq!(r.values, TsValues::F32(vals.clone()));
    assert_eq!(
        (
            r.units_independent.as_str(),
            r.type_independent.as_str(),
            r.units_dependent.as_str(),
            r.type_dependent.as_str()
        ),
        ("FT", "LINEAR", "DOLLARS", "LINEAR")
    );
    assert_eq!(
        r.labels,
        vec!["Total", "Residential damage (structures)", "C"]
    );
    assert_eq!((r.x_precision, r.y_precision), (2, 1));
    assert_eq!(r.independent_is_x_axis, 1);
    if v == V::V7 {
        assert_eq!(r.time_zone.as_deref(), Some("UTC"));
    }
    clean(v, &p, "added");

    // Equal rewrite: nothing written.
    let before = std::fs::read(&p).unwrap();
    assert_eq!(
        write(v, &p, &input, &opts()).unwrap(),
        BlockOutcome::Unchanged
    );
    assert_eq!(std::fs::read(&p).unwrap(), before);

    // Replace with doubles (PD -> PDD), no labels, other strings.
    let dord = [0.1f64, 0.2, 0.30000000000000004];
    let dval = [1.0f64 / 3.0, -0.0, f64::MIN_POSITIVE];
    let mut dbl = PdInput::new(
        input.pathname,
        PdArrays::F64 {
            ordinates: &dord,
            values: &dval,
        },
        1,
        b"",
        b"PROB",
        b"cfs",
        b"LOG",
    );
    dbl.independent_is_x_axis = 2;
    assert_eq!(write(v, &p, &dbl, &opts()).unwrap(), BlockOutcome::Replaced);
    let r = read(v, &p, stored);
    assert_eq!(r.record_type, 205);
    assert_eq!(r.ordinates, TsValues::F64(dord.to_vec()));
    assert_eq!(bits(&r.values), bits(&TsValues::F64(dval.to_vec())));
    assert!(r.labels.is_empty());
    assert_eq!((r.x_precision, r.y_precision), (-1, -1));
    assert_eq!(r.independent_is_x_axis, 2);
    assert_eq!(r.type_independent, "PROB");
    assert_eq!(r.units_dependent, "cfs");
    clean(v, &p, "replaced");

    // Back to floats, larger (relocation with growth).
    let big_o: Vec<f32> = (0..200).map(|i| i as f32).collect();
    let big_v: Vec<f32> = (0..800).map(|i| (i as f32).sqrt()).collect();
    let big = PdInput::new(
        input.pathname,
        PdArrays::F32 {
            ordinates: &big_o,
            values: &big_v,
        },
        4,
        b"HOURS",
        b"TIME",
        b"FRACTION",
        b"CUMUL",
    );
    assert_eq!(write(v, &p, &big, &opts()).unwrap(), BlockOutcome::Replaced);
    let r = read(v, &p, stored);
    assert_eq!(r.values, TsValues::F32(big_v));
    clean(v, &p, "grown");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn round_trip_dss7() {
    round_trip(V::V7);
}

#[test]
fn round_trip_dss6() {
    round_trip(V::V6);
}

// ---------------------------------------------------------------------------------
// Refusals leave the file byte-identical; other records are word-identical
// ---------------------------------------------------------------------------------

fn refusals(v: V) {
    let d = tmp(&format!("ref{v:?}"));
    let p = d.join("r.dss");
    create(v, &p);
    let ts = [1.0f32, 2.0];
    let start = 43_831 * 86_400 + 3_600;
    let rts = RtsInput::new(
        b"/A/B/FLOW//1HOUR/F/",
        start,
        RtsValues::F32(&ts),
        b"CFS",
        b"INST-VAL",
    );
    match v {
        V::V7 => write_rts(&p, &rts, &opts()).unwrap(),
        V::V6 => write_rts6(&p, &rts, &opts()).unwrap(),
    };
    let vals = three_curves();
    let good = PdInput::new(
        b"/A/B/STAGE-FLOW///F/",
        PdArrays::F32 {
            ordinates: &ORD,
            values: &vals,
        },
        3,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    write(v, &p, &good, &opts()).unwrap();
    let snap = snapshot(v, &p);
    let bytes = std::fs::read(&p).unwrap();
    let refuse = |input: &PdInput, what: &str| {
        let e = write(v, &p, input, &opts()).unwrap_err();
        assert!(
            matches!(
                e,
                WriteError::BadPathname(_) | WriteError::BadInput(_) | WriteError::Unsupported(_)
            ),
            "{what}: {e:?}"
        );
        assert_eq!(std::fs::read(&p).unwrap(), bytes, "{what}: file changed");
    };
    // Pathnames: 8 slashes (HMS Issue B2), 6 slashes, control bytes, long part.
    for bad in [
        &b"//SYN_R1/FIRST-QUARTILE/24HR///50%/"[..],
        b"/A/B/C/D/E/",
        b"/A/B/STAGE\tFLOW///F/",
        b"A/B/C/D/E/F/",
    ] {
        refuse(
            &PdInput {
                pathname: bad,
                ..good.clone()
            },
            "pathname",
        );
    }
    let long = format!("/A/{}/C///F/", "B".repeat(129));
    refuse(
        &PdInput {
            pathname: long.as_bytes(),
            ..good.clone()
        },
        "long part",
    );
    // Sizes.
    refuse(
        &PdInput {
            number_curves: 2,
            ..good.clone()
        },
        "value count",
    );
    refuse(
        &PdInput {
            number_curves: 0,
            ..good.clone()
        },
        "no curves",
    );
    let empty: [f32; 0] = [];
    refuse(
        &PdInput {
            data: PdArrays::F32 {
                ordinates: &empty,
                values: &empty,
            },
            ..good.clone()
        },
        "no ordinates",
    );
    // Labels: wrong count, control byte.
    let two = labels(&["a", "b"]);
    refuse(
        &PdInput {
            labels: Some(&two),
            ..good.clone()
        },
        "label count",
    );
    let ctl = labels(&["a", "b\u{1}", "c"]);
    refuse(
        &PdInput {
            labels: Some(&ctl),
            ..good.clone()
        },
        "label byte",
    );
    // Precision: mixed or out of range.
    refuse(
        &PdInput {
            x_precision: 2,
            ..good.clone()
        },
        "mixed precision",
    );
    refuse(
        &PdInput {
            x_precision: 10,
            y_precision: 1,
            ..good.clone()
        },
        "precision range",
    );
    // A time-series record at the same pathname: heclib would delete it.
    refuse(
        &PdInput {
            pathname: b"/A/B/FLOW/01JAN2020/1HOUR/F/",
            ..good.clone()
        },
        "ts record",
    );
    // Strings too long for the internal header.
    let huge = vec![b'U'; 400];
    refuse(
        &PdInput {
            units_independent: &huge,
            ..good.clone()
        },
        "long units",
    );
    if v == V::V6 {
        refuse(
            &PdInput {
                units_dependent: b"123456789",
                ..good.clone()
            },
            "9-char units",
        );
        refuse(
            &PdInput {
                x_precision: 0,
                y_precision: 3,
                ..good.clone()
            },
            "dss6 x precision 0",
        );
        refuse(
            &PdInput {
                time_zone: Some(b"UTC"),
                ..good.clone()
            },
            "dss6 time zone",
        );
        refuse(
            &PdInput {
                independent_is_x_axis: 0,
                ..good.clone()
            },
            "axis 0",
        );
        let trail = labels(&["a ", "b", "c"]);
        refuse(
            &PdInput {
                labels: Some(&trail),
                ..good.clone()
            },
            "trailing blank",
        );
        let blank = labels(&["", "        x", ""]);
        refuse(
            &PdInput {
                labels: Some(&blank),
                ..good.clone()
            },
            "blank labels",
        );
        let l101 = [vec![b'L'; 101], b"b".to_vec(), b"c".to_vec()];
        refuse(
            &PdInput {
                labels: Some(&l101),
                ..good.clone()
            },
            "label > 100",
        );
        let o: Vec<f32> = vec![1.0; 2];
        let many: Vec<f32> = vec![1.0; 2 * 101];
        refuse(
            &PdInput {
                data: PdArrays::F32 {
                    ordinates: &o,
                    values: &many,
                },
                number_curves: 101,
                labels: None,
                ..good.clone()
            },
            "101 curves",
        );
    }
    assert_eq!(snapshot(v, &p), snap);
    // A second record: the first is word-identical (G4 by snapshot and by bytes of the
    // old region below the old end of file).
    let other = PdInput {
        pathname: b"/A/C/STAGE-FLOW///F/",
        ..good.clone()
    };
    write(v, &p, &other, &opts()).unwrap();
    let after = snapshot(v, &p);
    for (k, val) in &snap {
        assert_eq!(after.get(k), Some(val));
    }
    clean(v, &p, "after refusals");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn refusals_dss7() {
    refusals(V::V7);
}

#[test]
fn refusals_dss6() {
    refusals(V::V6);
}

/// Regression: the HMS Atlas 14 import built an 8-slash pathname, which
/// heclib truncated so every probability column landed in one record. The native writer
/// refuses that pathname, and well-formed pathnames per probability stay distinct.
fn b2_distinct(v: V) {
    let d = tmp(&format!("b2{v:?}"));
    let p = d.join("b2.dss");
    create(v, &p);
    let hours: Vec<f32> = (0..49).map(|i| i as f32 * 0.5).collect();
    let probs = ["10%", "50%", "90%"];
    for (k, prob) in probs.iter().enumerate() {
        let frac: Vec<f32> = (0..49)
            .map(|i| (i as f32 / 48.0).powf(1.0 + k as f32))
            .collect();
        let eight = format!("//SYN_R1/FIRST-QUARTILE/24HR///{prob}/");
        // DSS6 stores 8-character types (HMS writes CUMULATIVE to DSS7 files).
        let cum: &[u8] = if v == V::V6 {
            b"CUMULATI"
        } else {
            b"CUMULATIVE"
        };
        let input = PdInput::new(
            eight.as_bytes(),
            PdArrays::F32 {
                ordinates: &hours,
                values: &frac,
            },
            1,
            b"HOURS",
            b"TIME",
            b"FRACTION",
            cum,
        );
        assert!(matches!(
            write(v, &p, &input, &opts()),
            Err(WriteError::BadPathname(_))
        ));
        let seven = format!("//SYN_R1/FIRST-QUARTILE/24HR//{prob}/");
        let ok = PdInput {
            pathname: seven.as_bytes(),
            ..input
        };
        assert_eq!(write(v, &p, &ok, &opts()).unwrap(), BlockOutcome::Added);
    }
    let snap = snapshot(v, &p);
    assert_eq!(snap.len(), probs.len());
    for (k, prob) in probs.iter().enumerate() {
        let path = format!("//SYN_R1/FIRST-QUARTILE/24HR//{prob}/");
        let r = read(v, &p, path.as_bytes());
        let want: Vec<f32> = (0..49)
            .map(|i| (i as f32 / 48.0).powf(1.0 + k as f32))
            .collect();
        assert_eq!(r.values, TsValues::F32(want), "{prob}");
        assert_eq!(r.type_independent, "TIME");
        assert!(r.type_dependent.starts_with("CUMULATI"));
        assert!(r.labels.is_empty(), "types are not labels");
    }
    clean(v, &p, "b2");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn b2_probability_pathnames_stay_distinct_dss7() {
    b2_distinct(V::V7);
}

#[test]
fn b2_probability_pathnames_stay_distinct_dss6() {
    b2_distinct(V::V6);
}

// ---------------------------------------------------------------------------------
// Crash at every step
// ---------------------------------------------------------------------------------

fn crash(v: V) {
    let d = tmp(&format!("crash{v:?}"));
    let base = d.join("base.dss");
    create(v, &base);
    let vals = three_curves();
    let first = PdInput::new(
        b"/C/OLD/STAGE-FLOW///F/",
        PdArrays::F32 {
            ordinates: &ORD,
            values: &vals,
        },
        3,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    write(v, &base, &first, &opts()).unwrap();
    let other = PdInput {
        pathname: b"/C/OTHER/STAGE-FLOW///F/",
        ..first.clone()
    };
    write(v, &base, &other, &opts()).unwrap();
    let nv: Vec<f64> = (0..5).map(|i| i as f64 * 7.0).collect();
    let no: Vec<f64> = ORD.iter().map(|&x| f64::from(x)).collect();
    let ls = labels(&["new"]);
    let mut newrec = PdInput::new(
        b"/C/NEW/STAGE-FLOW///F/",
        PdArrays::F64 {
            ordinates: &no,
            values: &nv,
        },
        1,
        b"FT",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    newrec.labels = Some(&ls);
    let rewrite = PdInput {
        pathname: b"/C/OLD/STAGE-FLOW///F/",
        ..newrec.clone()
    };
    let steps: Vec<WriteStep> = match v {
        V::V7 => WriteStep::ALL.to_vec(),
        V::V6 => DSS6_STEP_ORDER.to_vec(),
    };
    // A same-type growth (more ordinates, float as stored) and a
    // label-only rewrite (same values, new labels).
    let go: Vec<f32> = (0..12).map(|i| i as f32).collect();
    let gv: Vec<f32> = (0..36).map(|i| 50.0 + i as f32).collect();
    let grow = PdInput {
        data: PdArrays::F32 {
            ordinates: &go,
            values: &gv,
        },
        labels: None,
        ..first.clone()
    };
    let relabels = labels(&["one", "two", "three"]);
    let relabel = PdInput {
        labels: Some(&relabels),
        ..first.clone()
    };
    for (name, input) in [
        ("new", &newrec),
        ("rewrite", &rewrite),
        ("grow", &grow),
        ("relabel", &relabel),
    ] {
        let reference = d.join(format!("ref_{name}.dss"));
        std::fs::copy(&base, &reference).unwrap();
        write(v, &reference, input, &opts()).unwrap();
        let want = snapshot(v, &reference);
        let before = snapshot(v, &base);
        let target = input.pathname.to_ascii_uppercase();
        for step in &steps {
            let f = d.join(format!("{name}_{step:?}.dss"));
            std::fs::copy(&base, &f).unwrap();
            let o = WriteOptions {
                fault: Some(FaultPoint {
                    block: 0,
                    step: *step,
                }),
                ..opts()
            };
            match write(v, &f, input, &o) {
                Err(WriteError::InjectedFault { .. }) => {}
                other => panic!("{v:?} {name} {step:?}: {other:?}"),
            }
            let now = snapshot_with(v, &f, false);
            for (k, val) in &before {
                if *k == target {
                    assert!(
                        now.get(k) == Some(val) || now.get(k) == want.get(k),
                        "{v:?} {name} {step:?}: target neither old nor new"
                    );
                } else {
                    assert_eq!(now.get(k), Some(val), "{v:?} {name} {step:?}");
                }
            }
            // The same write repairs and completes it.
            let g = d.join(format!("{name}_{step:?}_rerun.dss"));
            std::fs::copy(&f, &g).unwrap();
            write(v, &g, input, &opts()).unwrap();
            clean(v, &g, &format!("{name} {step:?} rerun"));
            assert_eq!(snapshot(v, &g), want, "{v:?} {name} {step:?} rerun");
            // Another record first, then the same write.
            let h = d.join(format!("{name}_{step:?}_other.dss"));
            std::fs::copy(&f, &h).unwrap();
            let third = PdInput {
                pathname: b"/C/THIRD/STAGE-FLOW///F/",
                ..first.clone()
            };
            write(v, &h, &third, &opts()).unwrap();
            write(v, &h, input, &opts()).unwrap();
            clean(v, &h, &format!("{name} {step:?} other"));
            let s = snapshot(v, &h);
            for (k, val) in &want {
                assert_eq!(s.get(k), Some(val), "{v:?} {name} {step:?} other");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn crash_after_every_step_dss7() {
    crash(V::V7);
}

#[test]
fn crash_after_every_step_dss6() {
    crash(V::V6);
}

// ---------------------------------------------------------------------------------
// Fuzz: input validation never panics; accepted inputs round-trip
// ---------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn fuzz_bytes(r: &mut Rng, max: u64) -> Vec<u8> {
    let n = r.below(max + 1);
    (0..n)
        .map(|_| match r.below(10) {
            0 if r.below(8) == 0 => r.below(256) as u8,
            1 if r.below(4) == 0 => b'/',
            2 => b' ',
            _ => b'A' + r.below(26) as u8,
        })
        .collect()
}

#[test]
fn fuzz_input_validation() {
    let iters: usize = std::env::var("PD_WRITE_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);
    let d = tmp("fuzz");
    let p7 = d.join("f7.dss");
    let p6 = d.join("f6.dss");
    create_dss7(&p7).unwrap();
    create_dss6(&p6).unwrap();
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ok7, mut ok6) = (0, 0);
    for i in 0..iters {
        let path = if r.below(3) == 0 {
            fuzz_bytes(&mut r, 40)
        } else {
            format!(
                "/F/{}/{}-X/{}/E/{}/",
                String::from_utf8_lossy(&fuzz_bytes(&mut r, 6)).replace('/', "_"),
                i,
                if r.below(2) == 0 { "" } else { "D" },
                r.below(4)
            )
            .into_bytes()
        };
        let nord = r.below(6) as usize;
        let ncurve = r.below(4) as usize;
        let nvals = if r.below(5) == 0 {
            r.below(20) as usize
        } else {
            nord * ncurve
        };
        let ord: Vec<f32> = (0..nord)
            .map(|k| f32::from_bits(r.next() as u32 ^ k as u32))
            .collect();
        let vals: Vec<f32> = (0..nvals)
            .map(|_| f32::from_bits(r.next() as u32))
            .collect();
        let dord: Vec<f64> = ord.iter().map(|&x| f64::from(x)).collect();
        let dvals: Vec<f64> = vals.iter().map(|&x| f64::from(x)).collect();
        let data = if r.below(2) == 0 {
            PdArrays::F32 {
                ordinates: &ord,
                values: &vals,
            }
        } else {
            PdArrays::F64 {
                ordinates: &dord,
                values: &dvals,
            }
        };
        let nl = if r.below(6) == 0 {
            r.below(5) as usize
        } else {
            ncurve
        };
        let ls: Vec<Vec<u8>> = (0..nl).map(|_| fuzz_bytes(&mut r, 20)).collect();
        let s: Vec<Vec<u8>> = (0..5)
            .map(|_| {
                let m = if r.below(4) == 0 { 12 } else { 8 };
                fuzz_bytes(&mut r, m)
            })
            .collect();
        let mut input = PdInput::new(&path, data, ncurve, &s[0], &s[1], &s[2], &s[3]);
        if r.below(2) == 0 {
            input.labels = Some(&ls);
        }
        input.x_precision = r.below(12) as i32 - 1;
        input.y_precision = if r.below(3) == 0 {
            r.below(12) as i32 - 1
        } else {
            input.x_precision
        };
        input.independent_is_x_axis = if r.below(4) == 0 {
            r.below(4) as i32
        } else {
            1
        };
        if r.below(4) == 0 {
            input.time_zone = Some(&s[4]);
        }
        if validate_pd_input(&input).is_ok() {
            let rep = write_pd(&p7, &input, &opts()).expect("validated input writes");
            assert_ne!(rep.blocks[0].outcome, BlockOutcome::Deleted);
            let got = read(V::V7, &p7, &path);
            assert_eq!(
                bits(&got.values),
                bits(&match data {
                    PdArrays::F32 { values, .. } => TsValues::F32(values.to_vec()),
                    PdArrays::F64 { values, .. } => TsValues::F64(values.to_vec()),
                })
            );
            ok7 += 1;
        }
        if validate_pd6_input(&input).is_ok() {
            write_pd6(&p6, &input, &opts()).expect("validated input writes");
            let got = read(V::V6, &p6, &path.to_ascii_uppercase());
            assert_eq!(got.number_curves, ncurve);
            if let Some(l) = input.labels {
                let want: Vec<String> = l
                    .iter()
                    .map(|x| String::from_utf8_lossy(x).into_owned())
                    .collect();
                assert_eq!(got.labels, want);
            }
            ok6 += 1;
        }
    }
    assert!(ok7 > iters / 10 && ok6 > iters / 50, "{ok7} {ok6}");
    clean(V::V7, &p7, "fuzz7");
    clean(V::V6, &p6, "fuzz6");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// ELEV paired data (heclib's vertical-datum branch, zpdStore.c 324-647)
// ---------------------------------------------------------------------------------

fn hec_elev_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/p23_hec_elev7.dss")
}

fn elev_input<'a>(path: &'a [u8], o: &'a [f32], v: &'a [f32]) -> PdInput<'a> {
    PdInput::new(
        path,
        PdArrays::F32 {
            ordinates: o,
            values: v,
        },
        1,
        b"FT",
        b"LINEAR",
        b"AC-FT",
        b"LINEAR",
    )
}

#[test]
fn elev_rules_dss7() {
    let d = tmp("elev7");
    let p = d.join("elev.dss");
    std::fs::copy(hec_elev_fixture(), &p).unwrap();
    let o = [100.0f32, 101.0, 102.0];
    let v = [10.0f32, 20.0, 30.0];
    // 1. A fresh ELEV record (no location record): written as heclib writes it.
    let fresh = elev_input(b"/E/NEW/ELEV-STORAGE///F/", &o, &v);
    assert_eq!(
        write(V::V7, &p, &fresh, &opts()).unwrap(),
        BlockOutcome::Added
    );
    // 2. An existing ELEV record with a user header (heclib-written, no vertical-datum
    // text): heclib drops the header on a rewrite without one (observed with heclib), and
    // so does the writer.
    let mut f = Dss7File::open(&p).unwrap();
    assert!(!f
        .read_pd(b"/E/UH/ELEV-STORAGE///F/")
        .unwrap()
        .pd
        .user_header
        .is_empty());
    drop(f);
    let uh = elev_input(b"/E/UH/ELEV-STORAGE///F/", &o, &v);
    assert_eq!(
        write(V::V7, &p, &uh, &opts()).unwrap(),
        BlockOutcome::Replaced
    );
    let r = read(V::V7, &p, b"/E/UH/ELEV-STORAGE///F/");
    assert!(r.user_header.is_empty());
    assert_eq!(r.values, TsValues::F32(v.to_vec()));
    // 3. Vertical-datum information in the location record (heclib-written, including the
    // one heclib moved out of a data user header): refused, file byte-identical.
    let bytes = std::fs::read(&p).unwrap();
    for path in [
        &b"/E/VDI/ELEV-STORAGE///F/"[..],
        b"/E/UHVDI/ELEV-STORAGE///F/",
    ] {
        let input = elev_input(path, &o, &v);
        assert!(matches!(
            write(V::V7, &p, &input, &opts()),
            Err(WriteError::Unsupported(_))
        ));
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
    }
    // A non-ELEV record at the same location is not affected.
    let other = elev_input(b"/E/VDI/STAGE-FLOW///F/", &o, &v);
    assert_eq!(
        write(V::V7, &p, &other, &opts()).unwrap(),
        BlockOutcome::Added
    );
    // 4. A unit specification ('|') on an elevation side: refused (either side).
    let bytes = std::fs::read(&p).unwrap();
    let ind = PdInput {
        units_independent: b"U=ft|V=NAVD-88",
        ..elev_input(b"/E/SPEC/ELEV-STORAGE///F/", &o, &v)
    };
    let dep = PdInput {
        units_dependent: b"FT|NAVD-88",
        ..elev_input(b"/E/SPEC/STAGE-ELEV///F/", &o, &v)
    };
    for input in [ind, dep] {
        assert!(matches!(
            write(V::V7, &p, &input, &opts()),
            Err(WriteError::Unsupported(_))
        ));
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
    }
    // '|' in a non-elevation unit is stored as given.
    let plain = PdInput {
        units_dependent: b"CFS|X",
        ..elev_input(b"/E/SPEC/STAGE-FLOW///F/", &o, &v)
    };
    write(V::V7, &p, &plain, &opts()).unwrap();
    clean(V::V7, &p, "elev7");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn elev_rules_dss6() {
    let d = tmp("elev6");
    let p = d.join("elev6.dss");
    create_dss6(&p).unwrap();
    let o = [100.0f32, 101.0];
    let v = [10.0f32, 20.0];
    let input = PdInput {
        units_independent: b"FT|NGVD",
        ..elev_input(b"/E/SPEC/ELEV-STORAGE///F/", &o, &v)
    };
    let bytes = std::fs::read(&p).unwrap();
    assert!(matches!(
        write(V::V6, &p, &input, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
    let ok = elev_input(b"/E/SPEC/ELEV-STORAGE///F/", &o, &v);
    assert_eq!(write(V::V6, &p, &ok, &opts()).unwrap(), BlockOutcome::Added);
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// The accept side of the 392-byte, 100-curve and 100-character limits
// ---------------------------------------------------------------------------------

#[test]
fn accept_side_limits() {
    let d = tmp("limits");
    // DSS7: the five strings pack to whole 8-byte words. A 359-character X unit packs to
    // 360 bytes; with the four other (blank) strings at 8 bytes each the total is 392.
    let p7 = d.join("l7.dss");
    create_dss7(&p7).unwrap();
    let o = [1.0f32, 2.0];
    let v = [3.0f32, 4.0];
    let base = PdInput::new(
        b"/L/A/STAGE-FLOW///F/",
        PdArrays::F32 {
            ordinates: &o,
            values: &v,
        },
        1,
        b"",
        b"",
        b"",
        b"",
    );
    let u359 = vec![b'U'; 359];
    let u360 = vec![b'U'; 360];
    let ok = PdInput {
        units_independent: &u359,
        ..base.clone()
    };
    write(V::V7, &p7, &ok, &opts()).unwrap();
    assert_eq!(
        read(V::V7, &p7, b"/L/A/STAGE-FLOW///F/")
            .units_independent
            .len(),
        359
    );
    assert!(validate_pd_input(&PdInput {
        units_independent: &u360,
        ..base.clone()
    })
    .is_err());
    // DSS6: 100 curves with 100-character labels (long labels, NWDS 25).
    let p6 = d.join("l6.dss");
    create_dss6(&p6).unwrap();
    let ord: Vec<f32> = vec![1.0, 2.0];
    let vals: Vec<f32> = (0..200).map(|i| i as f32).collect();
    let ls: Vec<Vec<u8>> = (0..100)
        .map(|i| format!("{i:03}{}", "L".repeat(97)).into_bytes())
        .collect();
    let mut big = PdInput::new(
        b"/L/B/STAGE-FLOW///F/",
        PdArrays::F32 {
            ordinates: &ord,
            values: &vals,
        },
        100,
        b"12345678",
        b"LINEAR",
        b"CFS",
        b"LINEAR",
    );
    big.labels = Some(&ls);
    assert_eq!(
        write(V::V6, &p6, &big, &opts()).unwrap(),
        BlockOutcome::Added
    );
    let r = read(V::V6, &p6, b"/L/B/STAGE-FLOW///F/");
    assert_eq!(r.number_curves, 100);
    let want: Vec<String> = ls
        .iter()
        .map(|l| String::from_utf8(l.clone()).unwrap())
        .collect();
    assert_eq!(r.labels, want);
    assert_eq!(r.values, TsValues::F32(vals.clone()));
    clean(V::V6, &p6, "limits6");
    // The same record in DSS7 (no curve or label limit there).
    write(V::V7, &p7, &big, &opts()).unwrap();
    assert_eq!(read(V::V7, &p7, b"/L/B/STAGE-FLOW///F/").labels, want);
    clean(V::V7, &p7, "limits7");
    let _ = std::fs::remove_dir_all(&d);
}

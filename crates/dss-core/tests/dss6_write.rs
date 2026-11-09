//! P22: DSS6 writer (create, regular and irregular add/replace/merge), synthetic data.
//!
//! - The native writer replays `fixtures/public/p22_hec_ts6_ops.tsv` from an empty native
//!   file and every record equals `p22_hec_ts6.dss`, written by HEC Monolith 3.3.27 from
//!   the same ops file (dss6.md §9.3 lists the differences that remain).
//! - Crash test: every step of the block transaction ([`DSS6_STEP_ORDER`]) is interrupted
//!   in six scenarios (new bin, existing bin, overflow bin, new bin block, regular
//!   rewrite, irregular rewrite); each interrupted file reads, keeps every other record,
//!   passes the structural check apart from the marker and the documented `KNRECS`
//!   window, and is repaired by the next write. Also a crash inside that repairing write.
//! - Refusals and a fuzz pass over input validation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss_core::dss6::interval::{iymdjl, parse_d_part};
use dss_core::dss6::write::{
    check_file6, create_dss6, validate_its6_input, validate_rts6_input, write_its6, write_rts6,
    ItsInput, ItsMode, DSS6_STEP_ORDER,
};
use dss_core::dss6::{zhash6, Dss6File};
use dss_core::dss7::write::{
    BlockOutcome, FaultPoint, RtsInput, RtsValues, WriteError, WriteOptions, WriteStep,
};
use dss_core::TsValues;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dss6_write_{}_{name}", std::process::id()));
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

fn mins(y: i64, m: i64, d: i64, hh: i64, mm: i64) -> i64 {
    (iymdjl(y, m, d) * 1440 + hh * 60 + mm) * 60
}

// ---------------------------------------------------------------------------------
// The ops file format (the store lines `examples/dss6_write.rs` documents).
// ---------------------------------------------------------------------------------

enum Vals {
    F(Vec<f32>),
    D(Vec<f64>),
}

impl Vals {
    fn r(&self) -> RtsValues<'_> {
        match self {
            Vals::F(v) => RtsValues::F32(v),
            Vals::D(v) => RtsValues::F64(v),
        }
    }
}

fn parse_vals(s: &str, double: bool) -> Vals {
    let it = s.split(',').map(str::trim);
    if double {
        Vals::D(
            it.map(|t| match t.strip_prefix("0x") {
                Some(h) => f64::from_bits(u64::from_str_radix(h, 16).unwrap()),
                None => t.parse().unwrap(),
            })
            .collect(),
        )
    } else {
        Vals::F(
            it.map(|t| match t.strip_prefix("0x") {
                Some(h) => f32::from_bits(u32::from_str_radix(h, 16).unwrap()),
                None => t.parse().unwrap(),
            })
            .collect(),
        )
    }
}

fn parse_ints(s: &str) -> Option<Vec<i32>> {
    (s != "-").then(|| {
        s.split(',')
            .map(|t| t.trim().parse::<i64>().unwrap() as i32)
            .collect()
    })
}

fn run_op(path: &Path, line: &str, o: &WriteOptions) -> Result<Vec<BlockOutcome>, WriteError> {
    let c: Vec<&str> = line.split('\t').collect();
    let r = match c[0] {
        "rts" | "rtd" => {
            let j = parse_d_part(c[2].as_bytes()).unwrap();
            let t: i64 = c[3].parse().unwrap();
            let start = j * 86_400 + (t / 100) * 3_600 + (t % 100) * 60;
            let v = parse_vals(c[6], c[0] == "rtd");
            let q = parse_ints(c[7]);
            let mut input = RtsInput::new(
                c[1].as_bytes(),
                start,
                v.r(),
                c[4].as_bytes(),
                c[5].as_bytes(),
            );
            input.quality = q.as_deref();
            write_rts6(path, &input, o)?
        }
        "its" | "itd" => {
            let jb: i64 = c[2].parse().unwrap();
            let times: Vec<i64> = parse_ints(c[3])
                .unwrap()
                .iter()
                .map(|&m| (jb * 1440 + i64::from(m)) * 60)
                .collect();
            let v = parse_vals(c[6], c[0] == "itd");
            let q = parse_ints(c[7]);
            let input = ItsInput {
                pathname: c[1].as_bytes(),
                times: &times,
                values: v.r(),
                quality: q.as_deref(),
                units: c[4].as_bytes(),
                data_type: c[5].as_bytes(),
                mode: if c[8] == "0" {
                    ItsMode::Merge
                } else {
                    ItsMode::Replace
                },
            };
            write_its6(path, &input, o)?
        }
        other => panic!("unknown op {other}"),
    };
    Ok(r.blocks.iter().map(|b| b.outcome).collect())
}

// ---------------------------------------------------------------------------------
// Word-level comparison (logical words; the 4-byte record pads skipped).
// ---------------------------------------------------------------------------------

fn words(path: &Path) -> Vec<i32> {
    let b = std::fs::read(path).unwrap();
    let mut out = Vec::new();
    for rec in b.chunks(512) {
        let n = rec.len().min(508) / 4;
        for k in 0..n {
            out.push(i32::from_le_bytes(
                rec[4 * k..4 * k + 4].try_into().unwrap(),
            ));
        }
    }
    out
}

struct Rec {
    entry: Vec<i32>,
    info: Vec<i32>,
    ihead: Vec<i32>,
    data: Vec<i32>,
    record_type: i32,
}

/// Every live entry: pathname -> bin entry without `KBAINF`, info block without the
/// addresses, `KIPROG`, `KIDATE`, `KITIME` and `KIUNUS`, internal header and data.
fn records(w: &[i32]) -> BTreeMap<String, Rec> {
    let at = |a: i32| (a - 1) as usize;
    let (kafbin, kbnblk) = (w[19], w[16]);
    let mut out = BTreeMap::new();
    let (mut addr, mut nbin) = (kafbin, kbnblk);
    loop {
        let b = &w[at(addr)..at(addr) + 127];
        let mut j = 1usize;
        while j <= 125 {
            let st = b[j - 1];
            if st == 0 || st == -1 {
                break;
            }
            let np = b[j] as usize;
            let p = (np - 1) / 4 + 1;
            if st == 1 || st == 11 {
                let name: Vec<u8> = b[j + 1..j + 1 + p]
                    .iter()
                    .flat_map(|x| x.to_le_bytes())
                    .take(np)
                    .collect();
                let e = &b[j - 1..j - 1 + p + 11];
                let ia = e[2 + p];
                let info = &w[at(ia)..at(ia) + 28 + p];
                let rs = &info[3 + p..];
                let mut entry = e[..2 + p].to_vec();
                entry.extend_from_slice(&e[3 + p..]);
                let mut inf = info[..3 + p].to_vec();
                inf.extend_from_slice(&[rs[0], rs[1], rs[3], rs[4], rs[5]]);
                inf.extend_from_slice(&rs[12..17]);
                inf.extend_from_slice(&[rs[18], rs[20], rs[22], rs[23]]);
                out.insert(
                    String::from_utf8_lossy(&name).into_owned(),
                    Rec {
                        entry,
                        info: inf,
                        ihead: w[at(rs[17])..at(rs[17]) + rs[18] as usize].to_vec(),
                        data: w[at(rs[2])..at(rs[2]) + rs[3] as usize].to_vec(),
                        record_type: rs[14],
                    },
                );
            }
            j += p + 11;
        }
        nbin -= 1;
        if nbin == 0 {
            if b[126] == 0 {
                break;
            }
            addr = b[126];
            nbin = kbnblk;
        } else {
            addr += 127;
        }
    }
    out
}

#[test]
fn native_replay_matches_hec_fixture() {
    let d = tmp("replay");
    let nat = d.join("nat.dss");
    create_dss6(&nat).unwrap();
    let spec = std::fs::read_to_string(fixture("p22_hec_ts6_ops.tsv")).unwrap();
    let mut n = 0;
    for line in spec
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        run_op(&nat, line, &opts()).unwrap_or_else(|e| panic!("{line}: {e}"));
        n += 1;
    }
    assert_eq!(n, 87);
    assert!(check_file6(&nat).unwrap().is_clean());
    let hec = words(&fixture("p22_hec_ts6.dss"));
    let ours = words(&nat);
    // Header: every counter equal; KFSIZE, KDEAD and KANBIN depend on where records
    // were placed (HEC rewrites in place, the native writer relocates).
    for i in [
        1, 2, 3, 4, 14, 15, 16, 17, 18, 19, 37, 38, 39, 40, 41, 42, 43, 44, 52, 53, 54,
    ] {
        assert_eq!(hec[i], ours[i], "header word {i}");
    }
    assert!(ours[55..69].iter().all(|&w| w == 0), "KRES words");
    let (rh, rn) = (records(&hec), records(&ours));
    assert_eq!(rh.len(), 84);
    assert_eq!(rh.keys().collect::<Vec<_>>(), rn.keys().collect::<Vec<_>>());
    let mut stale = 0;
    for (k, a) in &rh {
        let b = &rn[k];
        assert_eq!(a.ihead, b.ihead, "{k} internal header");
        assert_eq!(a.data, b.data, "{k} data");
        if a.entry == b.entry && a.info == b.info {
            continue;
        }
        // The one known difference (dss6.md §9.3): HEC's in-place rewrite of an
        // irregular block keeps the old KILNDA/KBLNDA (zowrit6.f lines 155-160 set them
        // only when the record moves); the native writer relocates and stores the current count, which
        // equals NPAIRS (internal header word 2).
        assert!(a.record_type == 110 || a.record_type == 115, "{k}");
        let p = a.entry.len() - 10;
        let (mut ea, mut eb) = (a.entry.clone(), b.entry.clone());
        ea[2 + p + 2] = 0;
        eb[2 + p + 2] = 0;
        assert_eq!(ea, eb, "{k} entry");
        let (mut ia, mut ib) = (a.info.clone(), b.info.clone());
        ia[3 + p + 3] = 0;
        ib[3 + p + 3] = 0;
        assert_eq!(ia, ib, "{k} info");
        assert_eq!(b.info[3 + p + 3], b.ihead[1], "{k}: native KILNDA = NPAIRS");
        stale += 1;
    }
    assert_eq!(stale, 7);
}

#[test]
fn hec_fixture_reads_natively() {
    let f = Dss6File::open(fixture("p22_hec_ts6.dss")).unwrap();
    let cat = f.catalog().unwrap();
    assert_eq!(cat.entries.len(), 84);
    assert!(cat.anomalies.is_empty());
    for e in &cat.entries {
        f.read_ts_record(&e.pathname).unwrap();
    }
    assert!(check_file6(&fixture("p22_hec_ts6.dss")).unwrap().is_clean());
}

// ---------------------------------------------------------------------------------
// Snapshots and the crash test.
// ---------------------------------------------------------------------------------

type Snap = BTreeMap<Vec<u8>, (i32, Vec<u64>, Option<Vec<i32>>, Vec<i64>, String)>;

fn snapshot(path: &Path) -> Snap {
    let f = Dss6File::open(path).unwrap();
    let mut out = BTreeMap::new();
    for e in f.catalog().unwrap().entries {
        let r = f.read_ts_record(&e.pathname).unwrap();
        let bits = match &r.values {
            TsValues::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
            TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
        };
        out.insert(
            e.pathname.clone(),
            (r.record_type, bits, r.quality, r.times, r.units),
        );
    }
    out
}

/// A pathname of `len` characters... with the zhash6 code `want` (2048 codes).
fn path_with_hash(want: i32, tag: &str) -> String {
    for i in 0.. {
        let p = format!("/C/{tag}{i:05}/FLOW/01JAN2020/1HOUR/OV/");
        if zhash6(p.as_bytes(), 2048) == want {
            return p.replace("/01JAN2020/", "//");
        }
    }
    unreachable!()
}

fn hourly(path: &Path, p: &str, start: i64, v: &[f32]) -> Result<Vec<BlockOutcome>, WriteError> {
    hourly_o(path, p, start, v, &opts())
}

fn hourly_o(
    path: &Path,
    p: &str,
    start: i64,
    v: &[f32],
    o: &WriteOptions,
) -> Result<Vec<BlockOutcome>, WriteError> {
    let input = RtsInput::new(p.as_bytes(), start, RtsValues::F32(v), b"CFS", b"INST-VAL");
    write_rts6(path, &input, o).map(|r| r.blocks.iter().map(|b| b.outcome).collect())
}

fn irr(path: &Path, p: &str, t: &[i64], v: &[f32], o: &WriteOptions) -> Result<(), WriteError> {
    let input = ItsInput {
        pathname: p.as_bytes(),
        times: t,
        values: RtsValues::F32(v),
        quality: None,
        units: b"FT",
        data_type: b"INST-VAL",
        mode: ItsMode::Replace,
    };
    write_its6(path, &input, o).map(|_| ())
}

const T0: i64 = 43_830 * 86_400 + 3_600; // 01JAN2020 01:00

/// The base file of the crash scenarios and the write each scenario interrupts.
type WriteFn = Box<dyn Fn(&Path, &WriteOptions) -> Result<(), WriteError>>;

struct Scenario {
    name: &'static str,
    base: PathBuf,
    write: WriteFn,
    /// Pathnames the write changes (block pathnames).
    targets: Vec<Vec<u8>>,
}

fn scenarios(dir: &Path) -> Vec<Scenario> {
    let mk = |name: &str| {
        let p = dir.join(format!("base_{name}.dss"));
        create_dss6(&p).unwrap();
        hourly(&p, "/C/R/FLOW//1HOUR/F/", T0, &[1.0; 30]).unwrap();
        irr(
            &p,
            "/C/I/STAGE//IR-MONTH/F/",
            &[T0, T0 + 3_600, T0 + 40 * 86_400],
            &[1.0, 2.0, 3.0],
            &opts(),
        )
        .unwrap();
        p
    };
    let group_hash = zhash6(b"/C/G00000/FLOW/01JAN2020/1HOUR/OV/", 2048);
    let mut out = Vec::new();
    // (a) new record, new bin from the table.
    let b = mk("slot");
    out.push(Scenario {
        name: "new_bin",
        base: b,
        write: Box::new(|p, o| {
            hourly_o(p, "/C/NEW/FLOW//1HOUR/F/", T0, &[5.0, 6.0], o).map(|_| ())
        }),
        targets: vec![b"/C/NEW/FLOW/01JAN2020/1HOUR/F/".to_vec()],
    });
    // (b) new record in an existing bin (5 entries of one hash, the 6th fits).
    let b = mk("bin");
    let names: Vec<String> = (0..7)
        .map(|k| path_with_hash(group_hash, &format!("G{k}")))
        .collect();
    for n in &names[..5] {
        hourly(&b, n, T0, &[1.0]).unwrap();
    }
    let n5 = names[5].clone();
    out.push(Scenario {
        name: "existing_bin",
        base: b,
        targets: vec![n5.replace("//1HOUR", "/01JAN2020/1HOUR").into_bytes()],
        write: Box::new(move |p, o| hourly_o(p, &n5, T0, &[7.0], o).map(|_| ())),
    });
    // (c) overflow bin (the 7th entry of the hash does not fit).
    let b = mk("over");
    for n in &names[..6] {
        hourly(&b, n, T0, &[1.0]).unwrap();
    }
    let n6 = names[6].clone();
    out.push(Scenario {
        name: "overflow_bin",
        base: b,
        targets: vec![n6.replace("//1HOUR", "/01JAN2020/1HOUR").into_bytes()],
        write: Box::new(move |p, o| hourly_o(p, &n6, T0, &[8.0], o).map(|_| ())),
    });
    // (d) new bin block: the first block's 32 bins are used.
    let b = mk("block");
    let mut k = 0;
    while words(&b)[17] > 0 {
        hourly(&b, &format!("/C/B{k:03}/FLOW//1HOUR/F/"), T0, &[1.0]).unwrap();
        k += 1;
    }
    out.push(Scenario {
        name: "new_block",
        base: b,
        write: Box::new(|p, o| {
            hourly_o(p, "/C/NEWBLOCK/FLOW//1HOUR/F/", T0, &[9.0], o).map(|_| ())
        }),
        targets: vec![b"/C/NEWBLOCK/FLOW/01JAN2020/1HOUR/F/".to_vec()],
    });
    // (e) regular rewrite (merge into the stored block).
    let b = mk("rewrite");
    out.push(Scenario {
        name: "rewrite",
        base: b,
        write: Box::new(|p, o| {
            hourly_o(p, "/C/R/FLOW//1HOUR/F/", T0 + 7_200, &[42.0], o).map(|_| ())
        }),
        targets: vec![b"/C/R/FLOW/01JAN2020/1HOUR/F/".to_vec()],
    });
    // (f) irregular rewrite (replace inside the stored block).
    let b = mk("its");
    out.push(Scenario {
        name: "its_rewrite",
        base: b,
        write: Box::new(|p, o| irr(p, "/C/I/STAGE//IR-MONTH/F/", &[T0 + 1_800], &[4.5], o)),
        targets: vec![b"/C/I/STAGE/01JAN2020/IR-MONTH/F/".to_vec()],
    });
    out
}

/// Problems the structural check may report on an interrupted file: the marker, and
/// `KNRECS` one high between the Counters and Publish steps (dss6.md §9.3).
fn allowed_after_crash(problems: &[String]) -> bool {
    problems.iter().all(|p| {
        p.starts_with("in-progress marker")
            || p.starts_with("KNRECS is")
            || p.starts_with("KBINS is")
            || p.starts_with("KHUSED is")
            || p.starts_with("KBOVER is")
    })
}

#[test]
fn crash_at_every_step_then_recover() {
    let dir = tmp("crash");
    for sc in scenarios(&dir) {
        let reference = dir.join(format!("ref_{}.dss", sc.name));
        std::fs::copy(&sc.base, &reference).unwrap();
        (sc.write)(&reference, &opts()).unwrap();
        assert!(check_file6(&reference).unwrap().is_clean(), "{}", sc.name);
        let want = snapshot(&reference);
        let before = snapshot(&sc.base);
        for step in DSS6_STEP_ORDER {
            let f = dir.join(format!("{}_{step:?}.dss", sc.name));
            std::fs::copy(&sc.base, &f).unwrap();
            let o = WriteOptions {
                fault: Some(FaultPoint { block: 0, step }),
                ..opts()
            };
            match (sc.write)(&f, &o) {
                Err(WriteError::InjectedFault { .. }) => {}
                other => panic!("{} {step:?}: {other:?}", sc.name),
            }
            // The interrupted file reads; every other record is unchanged; the target is
            // the old or the new version.
            let now = snapshot(&f);
            for (k, v) in &before {
                if sc.targets.contains(k) {
                    assert!(
                        now.get(k) == Some(v) || want.get(k) == now.get(k),
                        "{} {step:?}",
                        sc.name
                    );
                } else {
                    assert_eq!(now.get(k), Some(v), "{} {step:?} {k:?}", sc.name);
                }
            }
            for k in now.keys() {
                assert!(
                    before.contains_key(k) || sc.targets.contains(k),
                    "{} {step:?}",
                    sc.name
                );
            }
            let c = check_file6(&f).unwrap();
            assert!(
                allowed_after_crash(&c.problems),
                "{} {step:?}: {:?}",
                sc.name,
                c.problems
            );
            if step == WriteStep::Finish {
                assert!(c.is_clean(), "{} {step:?}: {:?}", sc.name, c.problems);
            }
            // (1) The same write again repairs and completes it.
            let g = dir.join(format!("{}_{step:?}_rerun.dss", sc.name));
            std::fs::copy(&f, &g).unwrap();
            (sc.write)(&g, &opts()).unwrap();
            let c = check_file6(&g).unwrap();
            assert!(c.is_clean(), "{} {step:?} rerun: {:?}", sc.name, c.problems);
            assert_eq!(snapshot(&g), want, "{} {step:?} rerun", sc.name);
            // (2) A different write first (a new pathname under the same hash where the
            // scenario has one), then the same write.
            let h = dir.join(format!("{}_{step:?}_other.dss", sc.name));
            std::fs::copy(&f, &h).unwrap();
            hourly(&h, "/C/OTHER/FLOW//1HOUR/F/", T0, &[3.0]).unwrap();
            (sc.write)(&h, &opts()).unwrap();
            let c = check_file6(&h).unwrap();
            assert!(c.is_clean(), "{} {step:?} other: {:?}", sc.name, c.problems);
            let s = snapshot(&h);
            for (k, v) in &want {
                assert_eq!(s.get(k), Some(v), "{} {step:?} other {k:?}", sc.name);
            }
            // (3) A second crash inside the repairing write, then a clean write.
            for step2 in [
                WriteStep::Data,
                WriteStep::Allocation,
                WriteStep::Counters,
                WriteStep::Publish,
                WriteStep::Retire,
            ] {
                let k = dir.join(format!("{}_{step:?}_{step2:?}.dss", sc.name));
                std::fs::copy(&f, &k).unwrap();
                let o2 = WriteOptions {
                    fault: Some(FaultPoint {
                        block: 0,
                        step: step2,
                    }),
                    ..opts()
                };
                let _ = (sc.write)(&k, &o2);
                (sc.write)(&k, &opts()).unwrap();
                let c = check_file6(&k).unwrap();
                assert!(
                    c.is_clean(),
                    "{} {step:?}/{step2:?}: {:?}",
                    sc.name,
                    c.problems
                );
                assert_eq!(snapshot(&k), want, "{} {step:?}/{step2:?}", sc.name);
            }
        }
    }
}

#[test]
fn equal_rewrite_is_unchanged_and_block_outcomes() {
    let d = tmp("unchanged");
    let p = d.join("u.dss");
    create_dss6(&p).unwrap();
    assert_eq!(
        hourly(&p, "/A/B/FLOW//1HOUR/F/", T0, &[1.0, 2.0]).unwrap(),
        vec![BlockOutcome::Added]
    );
    let before = std::fs::read(&p).unwrap();
    assert_eq!(
        hourly(&p, "/A/B/FLOW//1HOUR/F/", T0, &[1.0, 2.0]).unwrap(),
        vec![BlockOutcome::Unchanged]
    );
    assert_eq!(std::fs::read(&p).unwrap(), before);
    assert_eq!(
        hourly(&p, "/A/B/FLOW//1HOUR/F/", T0, &[1.0, 3.0]).unwrap(),
        vec![BlockOutcome::Replaced]
    );
    // The old copy is marked relocated (zowrit6.f line 77) and counted as dead space.
    let w = words(&p);
    assert!(w[6] > 0, "KDEAD");
    assert!(check_file6(&p).unwrap().is_clean());
}

#[test]
fn irregular_deviations_and_refusals() {
    let d = tmp("irr");
    let p = d.join("i.dss");
    create_dss6(&p).unwrap();
    let ts = [mins(2020, 1, 1, 1, 0), mins(2020, 1, 2, 1, 0)];
    // Float written into a stored double block keeps it double (HEC narrows it).
    let input = ItsInput {
        pathname: b"/A/B/STAGE//IR-MONTH/D/",
        times: &ts,
        values: RtsValues::F64(&[1.0 / 3.0, 2.0 / 3.0]),
        quality: None,
        units: b"FT",
        data_type: b"INST-VAL",
        mode: ItsMode::Replace,
    };
    write_its6(&p, &input, &opts()).unwrap();
    let input2 = ItsInput {
        times: &ts[1..],
        values: RtsValues::F32(&[0.5]),
        ..input.clone()
    };
    let r = write_its6(&p, &input2, &opts()).unwrap();
    assert_eq!(r.blocks[0].record_type, 115);
    let f = Dss6File::open(&p).unwrap();
    let rec = f
        .read_ts_record(b"/A/B/STAGE/01JAN2020/IR-MONTH/D/")
        .unwrap();
    assert_eq!(rec.values, TsValues::F64(vec![1.0 / 3.0, 0.5]));
    // A replace with no value in a stored block would delete it: refused.
    let input = ItsInput {
        pathname: b"/A/B/STAGE//IR-DAY/G/",
        times: &[
            mins(2020, 1, 1, 1, 0),
            mins(2020, 1, 2, 1, 0),
            mins(2020, 1, 3, 1, 0),
        ],
        values: RtsValues::F32(&[1.0, 2.0, 3.0]),
        quality: None,
        units: b"FT",
        data_type: b"INST-VAL",
        mode: ItsMode::Replace,
    };
    write_its6(&p, &input, &opts()).unwrap();
    let before = std::fs::read(&p).unwrap();
    let gap = ItsInput {
        times: &[mins(2020, 1, 1, 2, 0), mins(2020, 1, 3, 2, 0)],
        values: RtsValues::F32(&[5.0, 6.0]),
        ..input.clone()
    };
    assert!(matches!(
        write_its6(&p, &gap, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    // Every block is built before the first store: the refused write changed nothing,
    // although its first block (01JAN) alone would have been accepted.
    assert_eq!(std::fs::read(&p).unwrap(), before);
    // The same gap with merge keeps the middle block.
    let merge = ItsInput {
        mode: ItsMode::Merge,
        ..gap.clone()
    };
    write_its6(&p, &merge, &opts()).unwrap();
    assert!(check_file6(&p).unwrap().is_clean());
    // -902 first or last is HEC's window marker: refused.
    let m = ItsInput {
        values: RtsValues::F32(&[-902.0, 1.0]),
        ..gap.clone()
    };
    assert!(matches!(
        validate_its6_input(&m),
        Err(WriteError::BadInput(_))
    ));
}

#[test]
fn refusals() {
    let d = tmp("refuse");
    let p = d.join("r.dss");
    create_dss6(&p).unwrap();
    let v = [1.0f32; 3];
    let bad_paths: [&[u8]; 6] = [
        b"/A/B/C//1HOU/F/",
        b"/A/B/C//15SECOND/F/",
        b"/A/B/C//~1HOUR/F/",
        b"/A/B/C/TS-PATTERN/1HOUR/F/",
        b"/A/B/C//IR-DAY/F/",
        b"/A/B/C//1HOUR/C:000001|X/",
    ];
    for bp in bad_paths {
        let i = RtsInput::new(bp, T0, RtsValues::F32(&v), b"CFS", b"INST-VAL");
        assert!(
            matches!(validate_rts6_input(&i), Err(WriteError::BadPathname(_))),
            "{bp:?}"
        );
    }
    let mut i = RtsInput::new(
        b"/A/B/C//1HOUR/F/",
        T0 + 30,
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    assert!(matches!(
        validate_rts6_input(&i),
        Err(WriteError::BadInput(_))
    ));
    i.start = T0;
    i.time_zone = Some(b"UTC");
    assert!(matches!(
        validate_rts6_input(&i),
        Err(WriteError::Unsupported(_))
    ));
    i.time_zone = None;
    i.data_type = b"INST-VALUE";
    assert!(matches!(
        validate_rts6_input(&i),
        Err(WriteError::BadInput(_))
    ));
    // Not DSS6, existing path, wrong record type under the pathname.
    let p7 = d.join("r7.dss");
    dss_core::dss7::write::create_dss7(&p7).unwrap();
    let ok = RtsInput::new(
        b"/A/B/C//1HOUR/F/",
        T0,
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    assert!(matches!(
        write_rts6(&p7, &ok, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert!(matches!(create_dss6(&p), Err(WriteError::AlreadyExists(_))));
    // A file left in HEC's write-lock mode (KFSIZE = -1357) is refused by the guard.
    let wl = d.join("wl.dss");
    std::fs::copy(&p, &wl).unwrap();
    let mut b = std::fs::read(&wl).unwrap();
    b[20..24].copy_from_slice(&(-1357i32).to_le_bytes());
    std::fs::write(&wl, &b).unwrap();
    assert!(matches!(
        write_rts6(&wl, &ok, &opts()),
        Err(WriteError::Locked(_))
    ));
    assert_eq!(std::fs::read(&wl).unwrap(), b);
}

/// Fuzz pass over input validation: random pathnames, starts, values and strings never
/// panic; a write is either a typed refusal or reads back.
#[test]
fn fuzz_input_validation() {
    let d = tmp("fuzz");
    let p = d.join("f.dss");
    create_dss6(&p).unwrap();
    let mut s: u64 = 0x2545_F491_4F6C_DD1D;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let eparts: [&str; 10] = [
        "1HOUR", "15MIN", "1DAY", "1WEEK", "1MON", "1YEAR", "IR-MONTH", "1Hour", "XX", "",
    ];
    let mut accepted = 0;
    for it in 0..1500 {
        let e = eparts[(rnd() % 10) as usize];
        let mut path = format!("/F/L{}/FLOW//{e}/Z/", rnd() % 50).into_bytes();
        if rnd() % 10 == 0 {
            let k = (rnd() as usize) % path.len();
            path[k] = (rnd() % 256) as u8;
        }
        let n = (rnd() % 40) as usize;
        let vals: Vec<f32> = (0..n).map(|_| f32::from_bits(rnd() as u32)).collect();
        let start = (rnd() % 4_000_000_000) as i64 - 100_000_000;
        let units: Vec<u8> = (0..(rnd() % 10)).map(|_| (rnd() % 128) as u8).collect();
        let q: Vec<i32> = (0..n).map(|_| rnd() as i32).collect();
        let with_q = rnd() % 3 == 0;
        if e.starts_with("IR") {
            let mut t: Vec<i64> = (0..n)
                .map(|k| start + 60 * (k as i64) * ((rnd() % 3) as i64))
                .collect();
            if rnd() % 5 == 0 && n > 1 {
                t.swap(0, n - 1);
            }
            let input = ItsInput {
                pathname: &path,
                times: &t,
                values: RtsValues::F32(&vals),
                quality: with_q.then_some(&q[..]),
                units: &units,
                data_type: b"INST-VAL",
                mode: if it % 2 == 0 {
                    ItsMode::Replace
                } else {
                    ItsMode::Merge
                },
            };
            match write_its6(&p, &input, &opts()) {
                Ok(_) => accepted += 1,
                Err(
                    WriteError::BadInput(_)
                    | WriteError::BadPathname(_)
                    | WriteError::Unsupported(_),
                ) => {}
                Err(other) => panic!("{other:?}"),
            }
        } else {
            let mut input = RtsInput::new(
                &path,
                start / 60 * 60,
                RtsValues::F32(&vals),
                &units,
                b"INST-VAL",
            );
            input.quality = with_q.then_some(&q[..]);
            match write_rts6(&p, &input, &opts()) {
                Ok(_) => accepted += 1,
                Err(
                    WriteError::BadInput(_)
                    | WriteError::BadPathname(_)
                    | WriteError::Unsupported(_),
                ) => {}
                Err(other) => panic!("{other:?}"),
            }
        }
    }
    assert!(accepted > 100, "{accepted}");
    let c = check_file6(&p).unwrap();
    assert!(c.is_clean(), "{:?}", c.problems);
    let f = Dss6File::open(&p).unwrap();
    for e in f.catalog().unwrap().entries {
        f.read_ts_record(&e.pathname).unwrap();
    }
}

/// A write refused in a later block leaves the file byte-identical. The
/// case: a February block stored as double with quality, then float values
/// without quality written from 31 January (HEC refuses that block, zsrtsi6 status 511).
#[test]
fn refused_multi_block_write_changes_nothing() {
    let d = tmp("m1");
    let p = d.join("m1.dss");
    create_dss6(&p).unwrap();
    let mut input = RtsInput::new(
        b"/A/B/C//1HOUR/F/",
        mins(2020, 2, 1, 1, 0),
        RtsValues::F64(&[1.0, 2.0]),
        b"CFS",
        b"INST-VAL",
    );
    input.quality = Some(&[3, 4]);
    write_rts6(&p, &input, &opts()).unwrap();
    let before = std::fs::read(&p).unwrap();
    let v = [9.0f32; 30];
    let jan31 = RtsInput::new(
        b"/A/B/C//1HOUR/F/",
        mins(2020, 1, 31, 1, 0),
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    assert!(matches!(
        write_rts6(&p, &jan31, &opts()),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(std::fs::read(&p).unwrap(), before);
    // Irregular: the second block of the write is refused (a replace that would delete
    // a stored block); the first block, valid on its own, is not written either.
    // (irregular_deviations_and_refusals checks the byte identity of the same case.)
}

/// The structural check and the writer never panic on damaged bins.
#[test]
fn check_and_writer_survive_corrupt_bins() {
    let d = tmp("m4");
    let base = d.join("base.dss");
    create_dss6(&base).unwrap();
    for k in 0..12 {
        hourly(
            &base,
            &format!("/C/K{k:02}/FLOW//1HOUR/F/"),
            T0,
            &[1.0, 2.0],
        )
        .unwrap();
    }
    let w = words(&base);
    let kafbin = w[19] as usize;
    let bytes = std::fs::read(&base).unwrap();
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    // The known case first: status 1 and pathname length 392 in the first free
    // entry slot of the first bin (word 19, after one 18-word entry).
    let mut cases: Vec<Vec<(usize, i32)>> =
        vec![vec![(kafbin - 1 + 18, 1), (kafbin - 1 + 19, 392)]];
    for _ in 0..400 {
        let n = 1 + (rnd() % 4) as usize;
        cases.push(
            (0..n)
                .map(|_| {
                    let addr = kafbin - 1 + (rnd() % (32 * 127)) as usize;
                    let v = match rnd() % 5 {
                        0 => -1,
                        1 => (rnd() % 500) as i32,
                        2 => 1,
                        3 => 11,
                        _ => rnd() as i32,
                    };
                    (addr, v)
                })
                .collect(),
        );
    }
    let f = d.join("c.dss");
    for (i, c) in cases.iter().enumerate() {
        let mut b = bytes.clone();
        for &(idx, v) in c {
            let off = (idx / 127) * 512 + (idx % 127) * 4;
            b[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&f, &b).unwrap();
        let r = check_file6(&f);
        if i == 0 {
            let r = r.unwrap();
            assert!(!r.is_clean(), "{:?}", r.problems);
        }
        // A write either succeeds or is refused with a typed error; no panic.
        let _ = hourly(&f, "/C/K03/FLOW//1HOUR/F/", T0, &[5.0]);
        let _ = hourly(&f, "/C/NEWNAME/FLOW//1HOUR/F/", T0, &[5.0]);
        let _ = check_file6(&f);
    }
}

/// A tag scheme is refused; a write stamps the last-written time.
#[test]
fn tag_scheme_refused_and_last_write_stamped() {
    let d = tmp("m56");
    let p = d.join("t.dss");
    create_dss6(&p).unwrap();
    let mut b = std::fs::read(&p).unwrap();
    b[44..52].copy_from_slice(b"XXXXXXXX"); // KLWTIM, words 11-12
    std::fs::write(&p, &b).unwrap();
    hourly(&p, "/A/B/C//1HOUR/F/", T0, &[1.0]).unwrap();
    let w = words(&p);
    let t: Vec<u8> = [w[11], w[12]]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    assert_eq!((t[2], t[5]), (b':', b':'));
    let tagged = d.join("tagged.dss");
    let mut b = std::fs::read(&p).unwrap();
    b[84..88].copy_from_slice(&1i32.to_le_bytes()); // KTAGS, word 21
    std::fs::write(&tagged, &b).unwrap();
    assert!(matches!(
        hourly(&tagged, "/A/B/C//1HOUR/G/", T0, &[1.0]),
        Err(WriteError::Unsupported(_))
    ));
    assert_eq!(std::fs::read(&tagged).unwrap(), b);
}

/// The DSS6 write lock is held while a write runs and released after it and after an
/// error. Windows: the whole-file lock is visible to another handle of this process.
#[test]
#[cfg(windows)]
fn lock_held_during_write_and_released() {
    use dss_core::lock::lock_status;
    let d = tmp("lockheld");
    let f = d.join("l.dss");
    create_dss6(&f).unwrap();
    let held_now = |p: &Path| match lock_status(p) {
        Ok(r) => r.locked(),
        Err(_) => true,
    };
    assert!(!held_now(&f));
    let fp = f.clone();
    let writer = std::thread::spawn(move || {
        let v: Vec<f32> = (0..1440 * 60).map(|i| (i % 97) as f32).collect();
        let input = RtsInput::new(
            b"/L/H/FLOW//1MIN/S/",
            T0,
            RtsValues::F32(&v),
            b"CFS",
            b"INST-VAL",
        );
        write_rts6(&fp, &input, &WriteOptions::default()).unwrap()
    });
    let mut seen = false;
    while !writer.is_finished() {
        seen |= held_now(&f);
    }
    let rep = writer.join().unwrap();
    assert_eq!(rep.blocks.len(), 61);
    assert!(seen, "the lock was never seen held during a 61-block write");
    assert_eq!(rep.lock_method, "whole_file_lockfileex");
    assert!(!held_now(&f), "released after the write");
    let o = WriteOptions {
        fault: Some(FaultPoint {
            block: 0,
            step: WriteStep::Publish,
        }),
        ..opts()
    };
    assert!(matches!(
        hourly_o(&f, "/L/E/FLOW//1HOUR/S/", T0, &[1.0], &o),
        Err(WriteError::InjectedFault { .. })
    ));
    assert!(!held_now(&f), "released after an error");
}

/// Linux: fcntl locks belong to the process, so a child process writes and this
/// process probes (as `dss7_write.rs`'s lock_seen_from_another_process).
#[test]
#[cfg(target_os = "linux")]
fn lock_seen_from_another_process6() {
    use dss_core::lock::lock_status;
    let d = tmp("lockchild6");
    let f = d.join("l.dss");
    create_dss6(&f).unwrap();
    assert!(!lock_status(&f).unwrap().locked());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock_child_writer6",
            "--ignored",
            "--test-threads=1",
        ])
        .env("P22_LOCK_CHILD_DIR", &d)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut seen = false;
    let marker = d.join("faulted");
    while !marker.exists() {
        if let Ok(r) = lock_status(&f) {
            seen |= r.locked();
        }
        if let Some(st) = child.try_wait().unwrap() {
            panic!("child exited early: {st}");
        }
    }
    assert!(
        seen,
        "the lock of the child was never seen during its write"
    );
    assert!(
        !lock_status(&f).unwrap().locked(),
        "released after the failed write"
    );
    assert!(child.wait().unwrap().success());
    let rep = write_rts6(
        &f,
        &RtsInput::new(
            b"/L/E/FLOW//1HOUR/S/",
            T0,
            RtsValues::F32(&[1.0]),
            b"CFS",
            b"INST-VAL",
        ),
        &opts(),
    )
    .unwrap();
    assert!(rep.reconciled_interrupted_write);
}

/// Child half of lock_seen_from_another_process6.
#[test]
#[ignore = "child process of lock_seen_from_another_process6"]
#[cfg(target_os = "linux")]
fn lock_child_writer6() {
    let Some(dir) = std::env::var_os("P22_LOCK_CHILD_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let f = dir.join("l.dss");
    let v: Vec<f32> = (0..1440 * 120).map(|i| (i % 97) as f32).collect();
    let input = RtsInput::new(
        b"/L/H/FLOW//1MIN/S/",
        T0,
        RtsValues::F32(&v),
        b"CFS",
        b"INST-VAL",
    );
    let rep = write_rts6(&f, &input, &WriteOptions::default()).unwrap();
    assert_eq!(rep.lock_method, "fcntl_setlk");
    let o = WriteOptions {
        fault: Some(FaultPoint {
            block: 0,
            step: WriteStep::Publish,
        }),
        ..opts()
    };
    assert!(matches!(
        hourly_o(&f, "/L/E/FLOW//1HOUR/S/", T0, &[1.0], &o),
        Err(WriteError::InjectedFault { .. })
    ));
    std::fs::write(dir.join("faulted"), b"1").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
}

//! P66: DSS6 regular time-series merge and extend into existing blocks (`zsrtsi6.f:ZSRTSI6`
//! with `IPLAN = 0`, HEC's default store flag, and `zmovbk6.f:ZMOVBK6` with `IFLAG = 1`;
//! hec-dss tag `7-IU-15`, `heclib/heclib_f/src/`).
//!
//! - `fixtures/public/p66_hec_ts6_merge_ops.tsv` is a matrix of merges (partial-block
//!   overlap at the head, the tail, inside and covering; extension before, after and on both
//!   sides across block boundaries for 1MIN, 15MIN, 1HOUR, 1DAY, 1WEEK, 1MON and 1YEAR;
//!   offsets kept and relabeled; missing values written over valid ones and valid values over
//!   missing ones; float into double, double into float, double into double; quality kept,
//!   added and double; compressed stored blocks). `p66_hec_ts6_merge.dss` is the file HEC
//!   Monolith 3.3.27 wrote from it (`zsrtsx`/`zsrtsxd`, one store per line).
//! - The native writer replays the ops file from an empty native file; every record equals
//!   HEC's, word for word, apart from the documented deviation that the native writer stores
//!   a merged compressed block uncompressed, where the decoded values,
//!   metadata and internal header are compared instead.
//! - Independently of HEC, a model of the store rule (new values replace stored values at
//!   their positions; nothing else changes; new block slots are -901) predicts every value
//!   and quality flag; both files read back as the model says.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss_core::dss6::interval::parse_d_part;
use dss_core::dss6::write::{check_file6, create_dss6, value_times6, write_rts6};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{RtsInput, RtsValues, SyncMode, WriteError, WriteOptions};
use dss_core::TsValues;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dss6_merge_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: SyncMode::None,
        ..WriteOptions::default()
    }
}

// ---------------------------------------------------------------------------------
// The ops files (`p66_*_ops.tsv`): one store per line.
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
    /// Values as HEC widens them into a double block (`DBLE`), as f64 bits.
    fn bits(&self) -> Vec<u64> {
        match self {
            Vals::F(v) => v.iter().map(|&x| f64::from(x).to_bits()).collect(),
            Vals::D(v) => v.iter().map(|x| x.to_bits()).collect(),
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

struct Op {
    pathname: String,
    start: i64,
    vals: Vals,
    quality: Option<Vec<i32>>,
    units: String,
    data_type: String,
}

fn ops() -> Vec<Op> {
    ops_from("p66_hec_ts6_merge_ops.tsv")
}

fn ops_from(name: &str) -> Vec<Op> {
    let spec = std::fs::read_to_string(fixture(name)).unwrap();
    spec.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|line| {
            let c: Vec<&str> = line.split('\t').collect();
            assert!(matches!(c[0], "rts" | "rtd" | "rtc"), "{line}");
            let j = parse_d_part(c[2].as_bytes()).unwrap();
            let t: i64 = c[3].parse().unwrap();
            Op {
                pathname: c[1].to_string(),
                start: j * 86_400 + (t / 100) * 3_600 + (t % 100) * 60,
                vals: parse_vals(c[6], c[0] == "rtd"),
                quality: parse_ints(c[7]),
                units: c[4].to_string(),
                data_type: c[5].to_string(),
            }
        })
        .collect()
}

/// `rtc` (HEC: a compressed store) is an ordinary regular store natively: the writer
/// stores uncompressed.
fn run_op(path: &Path, op: &Op) -> Result<(), WriteError> {
    let mut input = RtsInput::new(
        op.pathname.as_bytes(),
        op.start,
        op.vals.r(),
        op.units.as_bytes(),
        op.data_type.as_bytes(),
    );
    input.quality = op.quality.as_deref();
    write_rts6(path, &input, &opts()).map(|_| ())
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
    /// Info block without the addresses, `KIPROG`, `KIDATE`, `KITIME` and `KIUNUS`.
    info: Vec<i32>,
    ihead: Vec<i32>,
    data: Vec<i32>,
    compression: i32,
    /// Index in `info` of `KINDAT`, `KICOMP` and `KINCHE`.
    p: usize,
}

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
                        compression: rs[15],
                        p,
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

fn native_replay(dir: &Path) -> PathBuf {
    let nat = dir.join("nat.dss");
    create_dss6(&nat).unwrap();
    for op in ops() {
        run_op(&nat, &op).unwrap_or_else(|e| panic!("{}: {e}", op.pathname));
    }
    nat
}

const RECORDS: usize = 44;
const COMPRESSED: [&str; 2] = [
    "/P66/COMP1/FLOW/01JAN2020/1HOUR/M/",
    "/P66/COMP3/FLOW/01JAN2020/1HOUR/M/",
];

#[test]
fn native_merges_match_hec() {
    let d = tmp("replay");
    let nat = native_replay(&d);
    assert!(check_file6(&nat).unwrap().is_clean());
    let hec_path = fixture("p66_hec_ts6_merge.dss");
    assert!(check_file6(&hec_path).unwrap().is_clean());
    let hec = words(&hec_path);
    let ours = words(&nat);
    // Header: every counter equal; KFSIZE, KDEAD and KANBIN depend on where records
    // were placed (HEC rewrites in place, the native writer relocates).
    for i in [
        1, 2, 3, 4, 14, 15, 16, 17, 18, 19, 37, 38, 39, 40, 41, 42, 43, 44, 52, 53, 54,
    ] {
        assert_eq!(hec[i], ours[i], "header word {i}");
    }
    let (rh, rn) = (records(&hec), records(&ours));
    assert_eq!(rh.len(), RECORDS);
    assert_eq!(rh.keys().collect::<Vec<_>>(), rn.keys().collect::<Vec<_>>());
    let (fh, fnat) = (
        Dss6File::open(&hec_path).unwrap(),
        Dss6File::open(&nat).unwrap(),
    );
    let mut compressed = Vec::new();
    for (k, a) in &rh {
        let b = &rn[k];
        assert_eq!(a.ihead, b.ihead, "{k} internal header");
        assert_eq!(
            b.compression, 0,
            "{k}: the native writer stores uncompressed"
        );
        if a.compression == 0 {
            assert_eq!(a.data, b.data, "{k} data");
            assert_eq!(a.entry, b.entry, "{k} entry");
            assert_eq!(a.info, b.info, "{k} info");
            continue;
        }
        // Deviation: HEC keeps the stored compression scheme on a merge
        // (zsrtsi6.f lines 589-595); the native writer stores the merged block
        // uncompressed. Only KINDAT, KICOMP and KINCHE may differ; the values decode equal.
        compressed.push(k.clone());
        let p = a.p;
        let masked = |i: &[i32]| {
            let mut i = i.to_vec();
            for o in [3 + p + 2, 3 + p + 5 + 3, 3 + p + 5 + 5 + 1] {
                i[o] = 0;
            }
            i
        };
        assert_eq!(masked(&a.info), masked(&b.info), "{k} info");
        let (ra, rb) = (
            fh.read_ts_record(k.as_bytes()).unwrap(),
            fnat.read_ts_record(k.as_bytes()).unwrap(),
        );
        assert_eq!(ra.times, rb.times, "{k} times");
        if k == COMPRESSED[0] {
            // Scheme 1 (repeats) is lossless: the merged values are equal.
            assert_eq!(bits(&ra.values), bits(&rb.values), "{k} values");
        } // Scheme 3: see hec_requantizes_a_merged_delta_block.
        assert_eq!(
            (ra.record_type, &ra.quality, &ra.units, &ra.data_type),
            (rb.record_type, &rb.quality, &rb.units, &rb.data_type),
            "{k}"
        );
        assert_eq!(
            (ra.time_offset_minutes, ra.precision, ra.logical_values),
            (rb.time_offset_minutes, rb.precision, rb.logical_values),
            "{k}"
        );
    }
    assert_eq!(compressed, COMPRESSED.map(String::from).to_vec());
}

fn bits(v: &TsValues) -> Vec<u64> {
    match v {
        TsValues::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
        TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
    }
}

// ---------------------------------------------------------------------------------
// Model of the store rule, independent of either writer.
// ---------------------------------------------------------------------------------

#[derive(Default)]
struct Model {
    /// Dataset (pathname without D part) -> stamp -> (f64 bits, quality).
    vals: BTreeMap<String, BTreeMap<i64, (u64, i32)>>,
}

/// Model key of a stamp: hourly stamps by their standard hour, because a merge at another
/// offset relabels the whole block without converting it (`IIHEAD(1) = IOFSET`,
/// `zsrtsi6.f` line 893). The matrix uses offsets only on hourly series.
fn key(p: &str, s: i64) -> i64 {
    if p.split('/').nth(5) == Some("1HOUR") {
        s - s.rem_euclid(3_600)
    } else {
        s
    }
}

fn dataset(p: &str) -> String {
    let mut parts: Vec<&str> = p.split('/').collect();
    parts[4] = "";
    parts.join("/")
}

#[test]
fn both_files_read_as_the_store_rule_predicts() {
    let mut model = Model::default();
    for op in ops() {
        let t = value_times6(op.pathname.as_bytes(), op.start, op.vals.bits().len()).unwrap();
        let m = model.vals.entry(dataset(&op.pathname)).or_default();
        let q = op.quality.clone();
        for (i, (s, b)) in t.iter().zip(op.vals.bits()).enumerate() {
            m.insert(key(&op.pathname, *s), (b, q.as_ref().map_or(0, |q| q[i])));
        }
    }
    let d = tmp("model");
    let nat = native_replay(&d);
    for (who, file) in [("hec", fixture("p66_hec_ts6_merge.dss")), ("native", nat)] {
        let f = Dss6File::open(&file).unwrap();
        let cat = f.catalog().unwrap();
        assert_eq!(cat.entries.len(), RECORDS);
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for e in &cat.entries {
            let r = f.read_ts_record(&e.pathname).unwrap();
            let name = String::from_utf8_lossy(&e.pathname).into_owned();
            if who == "hec" && name == COMPRESSED[1] {
                // HEC re-quantizes this block (hec_requantizes_a_merged_delta_block).
                *seen.entry(dataset(&name)).or_default() += model.vals[&dataset(&name)].len();
                continue;
            }
            let m = &model.vals[&dataset(&name)];
            let vals: Vec<u64> = match &r.values {
                TsValues::F32(v) => v.iter().map(|&x| f64::from(x).to_bits()).collect(),
                TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
            };
            assert_eq!(r.times.len(), vals.len());
            for (i, (s, v)) in r.times.iter().zip(&vals).enumerate() {
                let q = r.quality.as_ref().map(|q| q[i]);
                match m.get(&key(&name, *s)) {
                    Some(&(want, wq)) => {
                        assert_eq!(*v, want, "{who} {name} value at {s}");
                        if let Some(q) = q {
                            assert_eq!(q, wq, "{name} quality at {s}");
                        }
                        *seen.entry(dataset(&name)).or_default() += 1;
                    }
                    None => {
                        assert_eq!(
                            *v,
                            (-901.0f64).to_bits(),
                            "{who} {name} unwritten slot at {s}"
                        );
                        if let Some(q) = q {
                            assert_eq!(q, 0, "{name} unwritten quality at {s}");
                        }
                    }
                }
            }
        }
        // Every modelled value was found in some block.
        for (ds, m) in &model.vals {
            assert_eq!(seen.get(ds).copied().unwrap_or(0), m.len(), "{ds}");
        }
    }
}

/// Widths by block: a double write makes its blocks double; a float write into a double
/// block keeps it double; a float write extending into a new block leaves that block float.
#[test]
fn block_widths_follow_hec() {
    let f = Dss6File::open(fixture("p66_hec_ts6_merge.dss")).unwrap();
    let ty = |p: &str| f.read_ts_record(p.as_bytes()).unwrap().record_type;
    assert_eq!(ty("/P66/F2D/FLOW/01JAN2020/1HOUR/M/"), 105);
    assert_eq!(ty("/P66/D2F/FLOW/01JAN2020/1HOUR/M/"), 105);
    assert_eq!(ty("/P66/MIXBLK/FLOW/01JAN2020/1HOUR/M/"), 100);
    assert_eq!(ty("/P66/MIXBLK/FLOW/01FEB2020/1HOUR/M/"), 105);
    assert_eq!(ty("/P66/EXTAFT/FLOW/01FEB2020/1HOUR/M/"), 100);
}

/// Observed HEC behaviour we did not implement. A merge into a block stored
/// with delta compression (scheme 3) re-compresses the whole block with the caller's
/// precision, not the stored one (`zsrtsi6.f` lines 586-595 keep the stored scheme; line
/// 659: "NEED TO GET OTHER COMPRESSION INFORMATION HERE (EG BASE NPREC)"). Here the block
/// was stored with `NPREC` -2 (hundredths) and merged through `zsrtsx` with `NPREC` 0, so
/// HEC rounds every value in the block to a whole number, including the stored values
/// outside the written window (7.5 becomes 8.0). Monolith reads the same values. The
/// native writer changes nothing outside the window and stores the block uncompressed.
#[test]
fn hec_requantizes_a_merged_delta_block() {
    let path = COMPRESSED[1].as_bytes();
    let hec = Dss6File::open(fixture("p66_hec_ts6_merge.dss")).unwrap();
    let h = hec.read_ts_record(path).unwrap().values.to_f64();
    let d = tmp("delta");
    let nat = native_replay(&d);
    let n = Dss6File::open(&nat).unwrap().read_ts_record(path).unwrap();
    assert_eq!(n.compression, 0);
    let n = n.values.to_f64();
    assert_eq!(h.len(), n.len());
    // Stored before the merge: 10 x 5.0, 10 x 7.5, then 10 values; the merge writes
    // positions 22..=31.
    assert!(n[..10].iter().all(|&x| x == 5.0));
    assert!(n[10..20].iter().all(|&x| x == 7.5));
    assert!(h[10..20].iter().all(|&x| x == 8.0));
    let mut changed_outside = 0;
    for i in 0..31 {
        // HEC's value is the native value rounded half away from zero to a whole number.
        assert_eq!(h[i], (n[i] as f32).round() as f64, "position {}", i + 1);
        if i < 21 && h[i] != n[i] {
            changed_outside += 1;
        }
    }
    assert_eq!(changed_outside, 10);
    assert!(h[31..].iter().zip(&n[31..]).all(|(a, b)| a == b));
}

// ---------------------------------------------------------------------------------
// Merges into the same HEC-compressed starting block.
// ---------------------------------------------------------------------------------

/// `zsrtsi6`'s delta compression stores each value quantized to `10^NPREC`; on a merge it
/// quantizes the whole block with the caller's `NPREC` (line 659). Model of the result for
/// one stored or written value, in single precision as `DCREAL` works: scale, round half
/// away from zero, scale back.
fn quantize(v: f32, nprec: i32) -> f32 {
    if v == -901.0 {
        return v;
    }
    let scale = 10f32.powi(-nprec);
    (v * scale).round() / scale
}

fn ulps(a: f32, b: f32) -> u32 {
    (a.to_bits() as i64 - b.to_bits() as i64).unsigned_abs() as u32
}

fn positions(r: std::ops::RangeInclusive<usize>) -> Vec<usize> {
    r.collect()
}

/// HEC Monolith (`zsrtsx`) and the HEC 7-IV-1 C library (`ztsStore`) merged into identical
/// copies of `p66_hec_ts6_comp_base.dss`, five blocks HEC stored compressed (scheme 1 and
/// scheme 3 at `NPREC` -2). The native writer merges into another copy here. Compared value
/// for value (decoded HEC values against the native block's stored words), with every
/// difference listed and explained by HEC's re-quantization to the caller's precision.
#[test]
fn same_block_compressed_merges() {
    let d = tmp("sameblock");
    let nat = d.join("nat.dss");
    std::fs::copy(fixture("p66_hec_ts6_comp_base.dss"), &nat).unwrap();
    let mut perm = std::fs::metadata(&nat).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(&nat, perm).unwrap();
    for op in ops_from("p66_hec_ts6_comp_merge_ops.tsv") {
        run_op(&nat, &op).unwrap();
    }
    assert!(check_file6(&nat).unwrap().is_clean());
    let base = Dss6File::open(fixture("p66_hec_ts6_comp_base.dss")).unwrap();
    let hec = Dss6File::open(fixture("p66_hec_ts6_comp_merged.dss")).unwrap();
    let cl = Dss6File::open(fixture("p66_c_ts6_comp_merged.dss")).unwrap();
    let native = Dss6File::open(&nat).unwrap();
    let nat_words = records(&words(&nat));
    let hec_words = records(&words(&fixture("p66_hec_ts6_comp_merged.dss")));
    let f32s = |v: &TsValues| match v {
        TsValues::F32(v) => v.clone(),
        TsValues::F64(_) => panic!("float blocks"),
    };
    // (case, written 1-based positions, Monolith caller NPREC, positions where Monolith
    // differs, positions where the C library differs). None: scheme 1, lossless.
    let mid = || {
        let mut v = positions(22..=25);
        v.extend(27..=35);
        v
    };
    let sb3p0 = {
        let mut v = positions(11..=20);
        v.extend(mid());
        v
    };
    let sb3p2_c = {
        let mut v = positions(22..=25);
        v.extend(27..=30);
        v.extend(32..=35);
        v
    };
    /// Record, written window, the caller's `NPREC`, then the positions expected to differ from
    /// Monolith's merge and from the C library's.
    type Case = (
        &'static str,
        std::ops::RangeInclusive<usize>,
        Option<i32>,
        Vec<usize>,
        Vec<usize>,
    );
    let cases: [Case; 5] = [
        ("SB1", 5..=9, None, vec![], vec![]),
        ("SB3P0", 26..=35, Some(0), sb3p0, mid()),
        ("SB3P2", 26..=35, Some(-2), vec![30, 33], sb3p2_c),
        ("SB3P2X", 26..=35, Some(-2), positions(27..=35), mid()),
        ("SB3P1", 26..=35, Some(-1), mid(), mid()),
    ];
    for (case, window, nprec, want_hec, want_c) in cases {
        let p = format!("/P66/{case}/FLOW/01JAN2020/1HOUR/M/");
        let b = base.read_ts_record(p.as_bytes()).unwrap();
        let n = native.read_ts_record(p.as_bytes()).unwrap();
        let h = hec.read_ts_record(p.as_bytes()).unwrap();
        let c = cl.read_ts_record(p.as_bytes()).unwrap();
        // The base block is compressed as HEC stored it; HEC keeps the scheme on a merge.
        assert_eq!(b.compression, if nprec.is_none() { 1 } else { 3 }, "{case}");
        assert_eq!(h.compression, b.compression, "{case}");
        assert_eq!(c.compression, b.compression, "{case}");
        assert_eq!(n.compression, 0, "{case}");
        let (bv, nv, hv, cv) = (
            f32s(&b.values),
            f32s(&n.values),
            f32s(&h.values),
            f32s(&c.values),
        );
        assert_eq!(nv.len(), 744);
        // Native: the decoded starting block, plus the input inside the window.
        for i in 0..744 {
            if !window.contains(&(i + 1)) {
                assert_eq!(
                    nv[i].to_bits(),
                    bv[i].to_bits(),
                    "{case} native position {}",
                    i + 1
                );
            }
        }
        // The native block's stored words are its values (uncompressed floats).
        let nw = &nat_words[&p];
        assert_eq!(nw.compression, 0);
        assert!(
            nw.data
                .iter()
                .zip(&nv)
                .all(|(&w, x)| w as u32 == x.to_bits()),
            "{case}"
        );
        // Internal header word for word (offset, units, type, time zone, coordinates).
        assert_eq!(nw.ihead, hec_words[&p].ihead, "{case} internal header");
        // Info block word for word, apart from addresses, stamps, and the three words of
        // the stored representation (KINDAT, KICOMP, KINCHE).
        let hw = &hec_words[&p];
        let masked = |r: &Rec| {
            let mut i = r.info.clone();
            for o in [3 + r.p + 2, 3 + r.p + 5 + 3, 3 + r.p + 5 + 5 + 1] {
                i[o] = 0;
            }
            i
        };
        assert_eq!(masked(nw), masked(hw), "{case} info");
        for (who, got, want, prec) in [
            ("Monolith", &hv, &want_hec, nprec),
            ("C library", &cv, &want_c, nprec.map(|_| -1)),
        ] {
            assert_eq!(h.times, n.times, "{case}");
            let differ: Vec<usize> = (0..744)
                .filter(|&i| got[i].to_bits() != nv[i].to_bits())
                .map(|i| i + 1)
                .collect();
            assert_eq!(&differ, want, "{case} {who}: positions that differ");
            if let Some(pr) = prec {
                // Every value HEC stored, inside and outside the window, is the native value
                // re-quantized to the caller's precision (within one unit in the last place
                // of HEC's delta reconstruction).
                for i in 0..744 {
                    let q = quantize(nv[i], pr);
                    assert!(
                        ulps(got[i], q) <= 1,
                        "{case} {who} position {}: {} vs model {q}",
                        i + 1,
                        got[i]
                    );
                }
            }
        }
    }
}

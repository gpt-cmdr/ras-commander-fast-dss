//! Synthetic paired-data files for the paired-data tests (no external files).
//!
//! DSS7: starts from the catalog test builder (`dss7_common::build`), appends each record's internal
//! header, header2 (labels), user header, values1 (ordinates) and values2 (curves) after
//! the end of file, points the info block at them (`zdssInfoKeys` 13–24, dss7.md §5.1)
//! and moves the end-of-file word. DSS6: the time-series payload builder (`dss6_ts_support`),
//! with the internal header of `zspdi6.f` / `zrpdi6.f` (dss6.md §5.2).

#![allow(dead_code)]

#[path = "../dss7_common/mod.rs"]
pub mod dss7_common;

#[path = "../dss6_ts_support/mod.rs"]
pub mod dss6_ts_support;

use std::path::PathBuf;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

/// Pack bytes into little-endian int32s, NUL padded.
pub fn ints(b: &[u8]) -> Vec<i32> {
    let mut b = b.to_vec();
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// A DSS7 paired-data payload. Areas are given as int32s; `*_n` override the stored
/// counts (to build inconsistent records).
#[derive(Clone, Debug, Default)]
pub struct Pd7 {
    pub ih: Vec<i32>,
    pub header2: Vec<i32>,
    pub user_header: Vec<i32>,
    pub values1: Vec<i32>,
    pub values2: Vec<i32>,
    pub ih_n: Option<i64>,
    pub v1_n: Option<i64>,
    pub v2_n: Option<i64>,
    pub h2_n: Option<i64>,
}

/// Internal header as `zpdUnitsToHead()` writes it (7-HA and later: int 5 blank, units
/// from int 6, strings on 8-byte boundaries).
pub fn pd7_header(no: i32, nc: i32, labels_len: i32, precision: i32, strings: &[&str]) -> Vec<i32> {
    let mut ih = vec![
        no,
        nc,
        1,
        labels_len,
        precision,
        i32::from_le_bytes(*b"    "),
    ];
    let mut c = Vec::new();
    for s in strings {
        let mut b = s.as_bytes().to_vec();
        // zcharTo8Byte(): blank pad so the NUL ends an 8-byte word.
        while (b.len() + 1) % 8 != 0 {
            b.push(b' ');
        }
        b.push(0);
        c.extend_from_slice(&b);
    }
    ih.extend(ints(&c));
    if ih.len() % 2 == 1 {
        ih.push(0);
    }
    ih
}

pub fn f32_ints(v: &[f32]) -> Vec<i32> {
    v.iter().map(|x| x.to_bits() as i32).collect()
}

pub fn f64_ints(v: &[f64]) -> Vec<i32> {
    v.iter()
        .flat_map(|x| {
            let b = x.to_bits();
            [b as u32 as i32, (b >> 32) as u32 as i32]
        })
        .collect()
}

fn word_get(buf: &[u8], w: i64) -> i64 {
    i64::from_le_bytes(buf[w as usize * 8..w as usize * 8 + 8].try_into().unwrap())
}

fn word_put(buf: &mut Vec<u8>, w: i64, v: i64) {
    let o = w as usize * 8;
    if buf.len() < o + 8 {
        buf.resize(o + 8, 0);
    }
    buf[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Append `ints` at word `*eof` (word aligned); return its address.
fn append(buf: &mut Vec<u8>, eof: &mut i64, v: &[i32]) -> i64 {
    let a = *eof;
    let words = (v.len() as i64 + 1) / 2;
    let o = a as usize * 8;
    buf.resize(o + words as usize * 8, 0);
    for (i, x) in v.iter().enumerate() {
        buf[o + i * 4..o + i * 4 + 4].copy_from_slice(&x.to_le_bytes());
    }
    *eof += words;
    a
}

/// Build a DSS7 file with these PD (or other) records and payloads.
pub fn dss7_file(recs: &[(&str, i32, Option<Pd7>)]) -> Vec<u8> {
    use dss7_common::{build, Rec, Spec};
    let spec = Spec {
        recs: recs.iter().map(|(p, t, _)| Rec::live(p, *t)).collect(),
        ..Spec::default()
    };
    let built = build(&spec);
    let mut buf = built.bytes;
    let mut eof = word_get(&buf, 5);
    buf.truncate(eof as usize * 8);
    for (i, (_, _, pl)) in recs.iter().enumerate() {
        let Some(p) = pl else { continue };
        let info = built.info_addr[i];
        let ih = append(&mut buf, &mut eof, &p.ih);
        let h2 = append(&mut buf, &mut eof, &p.header2);
        let uh = append(&mut buf, &mut eof, &p.user_header);
        let v1 = append(&mut buf, &mut eof, &p.values1);
        let v2 = append(&mut buf, &mut eof, &p.values2);
        let set = |buf: &mut Vec<u8>, k: i64, a: i64, n: i64| {
            word_put(buf, info + k, a);
            word_put(buf, info + k + 1, n);
        };
        set(&mut buf, 13, ih, p.ih_n.unwrap_or(p.ih.len() as i64));
        set(&mut buf, 15, h2, p.h2_n.unwrap_or(p.header2.len() as i64));
        set(&mut buf, 17, uh, p.user_header.len() as i64);
        set(&mut buf, 19, v1, p.v1_n.unwrap_or(p.values1.len() as i64));
        set(&mut buf, 21, v2, p.v2_n.unwrap_or(p.values2.len() as i64));
    }
    word_put(&mut buf, eof, -97532);
    word_put(&mut buf, 5, eof);
    buf
}

/// DSS6 internal header (`zspdi6.f`): NORD, NCURVE, IHORIZ, 4 × 8-character strings,
/// then optional extra words (labels).
pub fn pd6_header(nord: i32, ncurve: i32, units: [&str; 4], extra: &[i32]) -> Vec<i32> {
    use dss6_ts_support::text;
    let mut ih = vec![nord, ncurve, 1];
    for u in units {
        ih.extend(text(u, 2));
    }
    ih.extend_from_slice(extra);
    ih
}

/// A standard synthetic DSS7 PD file used by several tests.
pub fn standard_dss7() -> Vec<u8> {
    let labels = b"Total\0Residential\0Commercial\0";
    let mut h2 = labels.to_vec();
    h2.resize(32, 0);
    let pd = Pd7 {
        ih: pd7_header(
            3,
            3,
            labels.len() as i32,
            21,
            &["FEET", "Linear", "DOLLARS", "Log", "UTC"],
        ),
        header2: ints(&h2),
        user_header: vec![7, 8],
        values1: f32_ints(&[1.0, 2.0, 3.5]),
        values2: f32_ints(&[10.0, 20.0, 30.0, 11.0, 21.0, 31.0, -901.0, 22.0, 32.0]),
        ..Pd7::default()
    };
    let pdd = Pd7 {
        ih: pd7_header(2, 1, 0, -1, &["CFS", "UNT", "FEET", "UNT"]),
        values1: f64_ints(&[0.1, 1e300]),
        values2: f64_ints(&[f64::MIN_POSITIVE, -0.0]),
        ..Pd7::default()
    };
    dss7_file(&[
        ("/BASIN/LOC/STAGE-DAMAGE///F/", 200, Some(pd)),
        ("/BASIN/LOC/FLOW-STAGE///DOUBLE/", 205, Some(pdd)),
        ("/BASIN/LOC/FLOW/01Jan2020/1Hour/TS/", 100, None),
    ])
}

/// A standard synthetic DSS6 PD file: float with short labels, double with long labels,
/// plus a time-series record.
pub fn standard_dss6() -> Vec<u8> {
    use dss6_ts_support::{f32w, f64w, text, ts_file, Payload, Rec};
    let recs = [
        Rec::live("/A/B/STAGE-FLOW///SHORT/", 200),
        Rec::live("/A/B/ELEV-STOR///LONG/", 205),
        Rec::live("/A/B/FLOW/01JAN2000/1HOUR/TS/", 100),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    let mut short = text("  5%LIMIT   ", 3);
    short.extend(text("MEDIAN", 3));
    let ih = pd6_header(3, 2, ["FEET", "UNT", "CFS", "UNT"], &short);
    let mut data: Vec<i32> = [1.0f32, 2.0, 3.0, 10.0, 20.0, 30.0, 11.0, 21.0, 31.0]
        .iter()
        .map(|&v| f32w(v))
        .collect();
    data.push(f32w(99.0)); // extra stored word, ignored
    f.attach(
        "/A/B/STAGE-FLOW///SHORT/",
        &Payload {
            internal_header: ih,
            data,
            kilnda: 365, // stale, as in dss6.md §12 O9
            ..Payload::default()
        },
    );
    let mut long = text("", 3 * 2); // blank short labels
    long.push(4); // NWDS
    long.extend(text("A first long label", 4));
    long.extend(text("Second", 4));
    let ih = pd6_header(2, 2, ["FEET", "UNT", "ACRE-FT", "UNT"], &long);
    let data: Vec<i32> = [100.0f64, 200.5, 1.0, 2.0, 3.0, 4.0]
        .iter()
        .flat_map(|&v| f64w(v))
        .collect();
    f.attach(
        "/A/B/ELEV-STOR///LONG/",
        &Payload {
            internal_header: ih,
            data,
            kilnda: 6,
            ..Payload::default()
        },
    );
    f.finish()
}

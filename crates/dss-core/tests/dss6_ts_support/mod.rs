//! Synthetic DSS6 time-series files for P08b tests.
//!
//! Starts from P07's catalog builder (`dss6_support::build`), then appends each record's
//! internal header, compression header and data at end of file and points the info block
//! at them, following the layout HEC's `znwrit6` writes (dss6.md §5.1: info | internal
//! header | compression header | user header | data). Pad bytes are garbage.

#![allow(dead_code)]

#[path = "../dss6_support/mod.rs"]
mod dss6_support;

pub use dss6_support::{build, Opts, Rec};
use dss_core::dss6::word_byte_offset;

/// A time-series payload for one record.
#[derive(Clone, Debug, Default)]
pub struct Payload {
    pub internal_header: Vec<i32>,
    pub compression_header: Vec<i32>,
    pub user_header: Vec<i32>,
    pub data: Vec<i32>,
    pub kilnda: i32,
    pub kicomp: i32,
    pub kiqual: i32,
    pub password: i32,
}

pub struct TsFile {
    pub bytes: Vec<u8>,
    pub kfsize: i32,
    pub info_addrs: Vec<(Vec<u8>, i32)>,
}

impl TsFile {
    fn set(&mut self, addr: i32, v: i32) {
        let off = word_byte_offset(addr.into()).unwrap() as usize;
        if self.bytes.len() < off + 4 {
            // Grow by whole 512-byte records with garbage pads.
            while self.bytes.len() < off + 4 {
                let mut rec = vec![0u8; 508];
                rec.extend_from_slice(&[0xBA, 0xAD, 0xF0, 0x0D]);
                self.bytes.extend_from_slice(&rec);
            }
        }
        self.bytes[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn append(&mut self, words: &[i32]) -> i32 {
        let a = self.kfsize;
        for (i, &w) in words.iter().enumerate() {
            self.set(a + i as i32, w);
        }
        self.kfsize += words.len() as i32;
        a
    }

    /// Attach `p` to the record with pathname `path`.
    pub fn attach(&mut self, path: &str, p: &Payload) {
        self.attach_bytes(path.as_bytes(), p)
    }

    /// [`TsFile::attach`] for a pathname given as bytes (for example Latin-1).
    pub fn attach_bytes(&mut self, path: &[u8], p: &Payload) {
        let info = self
            .info_addrs
            .iter()
            .find(|(n, _)| n == path)
            .map(|(_, a)| *a)
            .expect("record in catalog");
        let pw = ((path.len() as i32) - 1) / 4 + 1;
        let q = info + pw;
        let ih = self.append(&p.internal_header);
        let ch = self.append(&p.compression_header);
        let uh = self.append(&p.user_header);
        let da = self.append(&p.data);
        self.set(q + 3, p.password);
        self.set(q + 5, da);
        self.set(q + 6, p.data.len() as i32);
        self.set(q + 7, p.kilnda);
        self.set(q + 18, p.kicomp);
        self.set(q + 19, p.kiqual);
        self.set(q + 20, ih);
        self.set(q + 21, p.internal_header.len() as i32);
        self.set(
            q + 22,
            if p.compression_header.is_empty() {
                0
            } else {
                ch
            },
        );
        self.set(q + 23, p.compression_header.len() as i32);
        self.set(q + 24, uh);
        self.set(q + 25, p.user_header.len() as i32);
    }

    /// Write the header's `KFSIZE` and the end-of-file flag; return the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let k = self.kfsize;
        self.set(k, -9630);
        self.set(6, k);
        // Whole records only.
        let n = (k as usize).div_ceil(127) * 512;
        while self.bytes.len() < n {
            self.bytes.extend_from_slice(&[0u8; 508]);
            self.bytes.extend_from_slice(&[0xBA, 0xAD, 0xF0, 0x0D]);
        }
        self.bytes
    }
}

/// Build a catalog for `recs` (version `6-YN` unless overridden) ready for payloads.
pub fn ts_file(recs: &[Rec], version: [u8; 4]) -> TsFile {
    let o = Opts {
        version,
        ..Opts::default()
    };
    let b = build(recs, &o);
    TsFile {
        bytes: b.bytes,
        kfsize: b.kfsize,
        info_addrs: b.info_addrs,
    }
}

/// Pack text into blank-padded words (`CH2HOL`).
pub fn text(s: &str, nwords: usize) -> Vec<i32> {
    let mut b = s.as_bytes().to_vec();
    b.resize(nwords * 4, b' ');
    b.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// RTS internal header (dss6.md §5.2): offset, units, type, time-zone offset, name.
pub fn rts_header(offset: i32, units: &str, dtype: &str, nwords: usize) -> Vec<i32> {
    let mut h = vec![offset];
    h.extend(text(units, 2));
    h.extend(text(dtype, 2));
    if nwords > 5 {
        h.push(-1);
    }
    // Words 7-12: blank time-zone name. Words 13-24: coordinates and descriptors,
    // zeroed when none are given (heclib_f/src/zsrtsi6.f lines 878-881).
    while h.len() < nwords {
        h.push(if h.len() < 12 { 0x2020_2020 } else { 0 });
    }
    h.truncate(nwords);
    h
}

/// ITS internal header: capacity, count, block julian, last time, pseudo interval,
/// units, type, time-zone offset.
pub fn its_header(cap: i32, n: i32, jul: i32, pseudo: i32, units: &str, dtype: &str) -> Vec<i32> {
    let mut h = vec![cap, n, jul, 0, pseudo];
    h.extend(text(units, 2));
    h.extend(text(dtype, 2));
    h.push(-1);
    h
}

pub fn f32w(v: f32) -> i32 {
    v.to_bits() as i32
}

pub fn f64w(v: f64) -> [i32; 2] {
    let b = v.to_bits();
    [(b & 0xffff_ffff) as u32 as i32, (b >> 32) as u32 as i32]
}

/// Compression header like `DHINIT`/`DCREAL` write (dss6.md §8).
pub fn compression_header(
    scheme: u8,
    nelms: i32,
    delta: Option<(u8, i8, f32)>,
    flags: &[bool],
) -> Vec<i32> {
    let nw = if scheme & 1 != 0 {
        (nelms as usize).div_ceil(32)
    } else {
        0
    };
    let mut b = vec![0u8; 36 + 4 * nw];
    b[0] = b'C';
    b[1] = 16;
    b[2] = 8;
    b[3] = scheme;
    b[4..8].copy_from_slice(&nelms.to_le_bytes());
    b[8] = b'D';
    b[9] = 16;
    b[10] = 13;
    if let Some((size, prec, base)) = delta {
        b[11] = size;
        b[12] = (i16::from(prec) + 10) as u8;
        b[14] = 4;
        b[15..19].copy_from_slice(&base.to_le_bytes());
    }
    b[21] = b'S';
    b[22] = 16;
    b[23] = 4;
    b[24] = 2;
    b[25] = b'R';
    b[26] = 16;
    let rlen = 8 + 4 * nw;
    b[27] = (rlen % 256) as u8;
    b[28] = (nelms % 32) as u8;
    b[29] = (rlen / 256) as u8;
    for (i, &f) in flags.iter().enumerate() {
        if f {
            b[32 + i / 8] |= 1 << (i % 8);
        }
    }
    b[33 + 4 * nw] = b'E';
    b.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// HEC julian day (1900-01-01 = 1).
pub fn jul(y: i64, m: u32, d: u32) -> i64 {
    dss_core::time::ymd_to_julian(y, m, d)
}

/// Seconds since the HEC epoch.
pub fn secs(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> i64 {
    (jul(y, m, d) * 1440 + hh * 60 + mm) * 60
}

/// A file with one record of each kind; returns bytes and the pathnames.
pub fn sample_file() -> Vec<u8> {
    let recs = [
        Rec::live("/A/B/FLOW/01JAN2000/1HOUR/F32/", 100),
        Rec::live("/A/B/FLOW/01MAR2000/1HOUR/F32/", 100),
        Rec::live("/A/B/FLOW/01JAN2000/1HOUR/QUAL/", 100),
        Rec::live("/A/B/FLOW/01JAN2000/1DAY/F64Q/", 105),
        Rec::live("/A/B/FLOW/01JAN2000/1DAY/OFFSET/", 100),
        Rec::live("/A/B/FLOW/01JAN2000/1HOUR/COMP3/", 100),
        Rec::live("/A/B/FLOW/01JAN2000/1HOUR/COMP4/", 100),
        Rec::live("/A/B/STAGE/01JAN2000/IR-MONTH/ITS/", 110),
        Rec::live("/A/B/STAGE/01FEB2000/IR-MONTH/ITS/", 110),
        Rec::live("/A/B/STAGE/01JAN2000/IR-YEAR/ITDQ/", 115),
        Rec::live("/A/B/FLOW/TS-PATTERN/1HOUR/PAT/", 101),
        Rec::live("/A/B/FLOW/01JAN2000/1HOUR/PASSWD/", 100),
        Rec::live("/A/B/CURVE///PD/", 200),
        Rec {
            pseudo: 1440,
            ..Rec::live("/A/B/FLOW/01JAN2000/IR-YEAR/PSEUDO/", 110)
        },
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    // 1HOUR float, January (744 values) and March (744); February absent.
    for (path, n) in [
        ("/A/B/FLOW/01JAN2000/1HOUR/F32/", 744),
        ("/A/B/FLOW/01MAR2000/1HOUR/F32/", 744),
    ] {
        let data: Vec<i32> = (0..n)
            .map(|i| {
                if i < 10 {
                    f32w(-901.0)
                } else {
                    f32w(i as f32 * 0.5)
                }
            })
            .collect();
        f.attach(
            path,
            &Payload {
                internal_header: rts_header(0, "CFS", "INST-VAL", 24),
                data,
                kilnda: n,
                ..Default::default()
            },
        );
    }
    // Quality, interleaved (value, flag).
    let mut d = Vec::new();
    for i in 0..744 {
        d.push(f32w(i as f32));
        d.push(i % 7);
    }
    f.attach(
        "/A/B/FLOW/01JAN2000/1HOUR/QUAL/",
        &Payload {
            internal_header: rts_header(0, "CFS", "PER-AVER", 9),
            data: d,
            kilnda: 744,
            kiqual: 1,
            ..Default::default()
        },
    );
    // Doubles then quality words; 366 values for leap year 2000.
    let mut d = Vec::new();
    for i in 0..366 {
        d.extend(f64w(1.0e6 + i as f64 / 3.0));
    }
    d.extend((0..366).map(|i| i * 3));
    f.attach(
        "/A/B/FLOW/01JAN2000/1DAY/F64Q/",
        &Payload {
            internal_header: rts_header(0, "FEET", "INST-VAL", 5),
            data: d,
            kilnda: 366,
            kiqual: 1,
            ..Default::default()
        },
    );
    // Daily with an 8-hour offset.
    f.attach(
        "/A/B/FLOW/01JAN2000/1DAY/OFFSET/",
        &Payload {
            internal_header: rts_header(480, "CFS", "INST-VAL", 24),
            data: (0..366).map(|i| f32w(i as f32)).collect(),
            kilnda: 366,
            ..Default::default()
        },
    );
    // Scheme 3: repeat + 1-byte delta, base 10, quantum 0.1; values 10,10,10,10.5,...
    let mut flags = vec![false; 744];
    let mut stored = Vec::new();
    for (i, fl) in flags.iter_mut().enumerate() {
        if i % 4 != 0 {
            *fl = true;
        } else {
            stored.push(((i / 4) % 200) as u8);
        }
    }
    stored[3] = 0xff; // missing
    let mut bytes = stored.clone();
    bytes.resize(bytes.len().div_ceil(4) * 4, 0);
    f.attach(
        "/A/B/FLOW/01JAN2000/1HOUR/COMP3/",
        &Payload {
            internal_header: rts_header(0, "IN", "PER-CUM", 5),
            compression_header: compression_header(3, 744, Some((1, -1, 10.0)), &flags),
            data: bytes
                .chunks(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
            kilnda: 744,
            kicomp: 3,
            ..Default::default()
        },
    );
    f.attach(
        "/A/B/FLOW/01JAN2000/1HOUR/COMP4/",
        &Payload {
            internal_header: rts_header(0, "IN", "PER-CUM", 5),
            compression_header: compression_header(4, 744, None, &[]),
            data: vec![i32::from_le_bytes([0xe9, 0x03, 0xe9, 0x03]); 372],
            kilnda: 744,
            kicomp: 4,
            ..Default::default()
        },
    );
    // ITS, January and February 2000 blocks, with spare capacity (O18).
    for (path, j0, base) in [
        (
            "/A/B/STAGE/01JAN2000/IR-MONTH/ITS/",
            jul(2000, 1, 1),
            1.0f32,
        ),
        (
            "/A/B/STAGE/01FEB2000/IR-MONTH/ITS/",
            jul(2000, 2, 1),
            100.0f32,
        ),
    ] {
        let mut d = Vec::new();
        for i in 0..5 {
            d.push(i * 1440 * 3 + 90);
            d.push(f32w(base + i as f32));
        }
        d.extend([0x7777_7777; 6]); // slack: never read
        f.attach(
            path,
            &Payload {
                internal_header: its_header(8, 5, j0 as i32, 0, "FEET", "INST-VAL"),
                data: d,
                kilnda: 5,
                ..Default::default()
            },
        );
    }
    // ITD with quality: (time, double, quality).
    let mut d = Vec::new();
    for i in 0..3 {
        d.push(i * 1440 * 40 + 1440);
        d.extend(f64w(2.5 * i as f64));
        d.push(i + 100);
    }
    f.attach(
        "/A/B/STAGE/01JAN2000/IR-YEAR/ITDQ/",
        &Payload {
            internal_header: its_header(3, 3, jul(2000, 1, 1) as i32, 0, "FT", "INST-VAL"),
            data: d,
            kilnda: 3,
            kiqual: 1,
            ..Default::default()
        },
    );
    // Pattern: 24 raw values, no dates.
    f.attach(
        "/A/B/FLOW/TS-PATTERN/1HOUR/PAT/",
        &Payload {
            internal_header: rts_header(0, "CFS", "INST-VAL", 5),
            data: (0..24).map(|i| f32w(i as f32 * 2.0)).collect(),
            kilnda: 99,
            ..Default::default()
        },
    );
    f.attach(
        "/A/B/FLOW/01JAN2000/1HOUR/PASSWD/",
        &Payload {
            internal_header: rts_header(0, "CFS", "INST-VAL", 5),
            data: vec![0; 744],
            kilnda: 744,
            password: 12345,
            ..Default::default()
        },
    );
    // Pseudo-regular daily data stored IR-YEAR.
    let mut d = Vec::new();
    for i in 0..10 {
        d.push(i * 1440 + 1440);
        d.push(f32w(i as f32));
    }
    f.attach(
        "/A/B/FLOW/01JAN2000/IR-YEAR/PSEUDO/",
        &Payload {
            internal_header: its_header(10, 10, jul(2000, 1, 1) as i32, 1440, "CFS", "INST-VAL"),
            data: d,
            kilnda: 10,
            ..Default::default()
        },
    );
    f.finish()
}

//! Synthetic DSS6 file builder for tests (packet P07).
//!
//! Lays files out the way HEC does (dss6.md §2-§5): 69-word header, hash table at word
//! 70, a lock record, bin blocks linked through word `KBNSIZ` of their last bin, overflow
//! bins linked through word `KBNSIZ-1`, info blocks with flag -9753, the -9630 EOF flag
//! at `KFSIZE`, and 127 words + 4 pad bytes per 512-byte record. Pad bytes and the lock
//! record are filled with garbage so tests notice if a reader uses them.
#![allow(dead_code)]

use dss_core::dss6::{pathname_hash, ReadAt};
use std::cell::RefCell;
use std::io;

#[derive(Clone, Debug)]
pub struct Rec {
    pub path: Vec<u8>,
    pub rtype: i32,
    pub status: i32,
    pub pseudo: i32,
}

impl Rec {
    pub fn live(path: &str, rtype: i32) -> Self {
        Self {
            path: path.as_bytes().to_vec(),
            rtype,
            status: if path.len() > 80 { 11 } else { 1 },
            pseudo: 0,
        }
    }
    pub fn with_status(mut self, status: i32) -> Self {
        self.status = status;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub khash: i32,
    pub kbnblk: i32,
    pub kbnsiz: i32,
    pub ktable: i32,
    pub version: [u8; 4],
    pub knrecs: Option<i32>,
    /// Drop the 4 pad bytes of the final record (old 508-byte writers).
    pub short_final: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            khash: 8,
            kbnblk: 4,
            kbnsiz: 127,
            ktable: 1,
            version: *b"6-YN",
            knrecs: None,
            short_final: false,
        }
    }
}

pub struct Built {
    pub bytes: Vec<u8>,
    pub kafbin: i32,
    pub kfsize: i32,
    pub lock_record: i64,
    /// (path, info address) per record, in insertion order.
    pub info_addrs: Vec<(Vec<u8>, i32)>,
    /// Word addresses of every bin block.
    pub blocks: Vec<i32>,
    /// Bins that overflowed: (bin address, overflow bin address).
    pub overflows: Vec<(i32, i32)>,
}

struct W {
    words: Vec<i32>, // index = address - 1
}

impl W {
    fn ensure(&mut self, addr: i32) {
        if self.words.len() < addr as usize {
            self.words.resize(addr as usize, 0);
        }
    }
    fn set(&mut self, addr: i32, v: i32) {
        self.ensure(addr);
        self.words[addr as usize - 1] = v;
    }
    fn get(&self, addr: i32) -> i32 {
        self.words.get(addr as usize - 1).copied().unwrap_or(0)
    }
}

fn pack(s: &[u8], nwords: usize) -> Vec<i32> {
    let mut b = s.to_vec();
    b.resize(nwords * 4, b' ');
    b.chunks(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn build(recs: &[Rec], o: &Opts) -> Built {
    let mut w = W { words: Vec::new() };
    let nperm = 69;
    let bnsiz = o.kbnsiz;
    // Lock record and first bin (zopen6Int.f lines 410-451).
    let lock_start_addr = if o.ktable == 1 {
        o.khash + nperm + 1
    } else {
        nperm + 1
    };
    let mut jrec = (lock_start_addr - 1) / 127 + 1;
    if (lock_start_addr - 1) % 127 != 0 {
        jrec += 1;
    }
    let lock_record = jrec as i64;
    let kafbin = jrec * 127 + 1;
    let first_block_bins = if o.ktable == 1 { o.kbnblk } else { o.khash };
    let mut kfsize = kafbin + first_block_bins * bnsiz;
    w.ensure(kfsize);
    let mut blocks = vec![kafbin];
    let mut next_bin = kafbin;
    let mut bins_left = first_block_bins;
    let mut overflows = Vec::new();
    // Type 2: every hash code already has its bin.
    if o.ktable == 2 {
        bins_left = 0;
    }
    let mut info_addrs = Vec::new();

    let alloc_bin = |w: &mut W,
                     kfsize: &mut i32,
                     blocks: &mut Vec<i32>,
                     next_bin: &mut i32,
                     bins_left: &mut i32|
     -> i32 {
        if *bins_left == 0 {
            // New block at EOF; link from the last bin of the previous block.
            let prev = *blocks.last().unwrap();
            let prev_bins = if blocks.len() == 1 {
                first_block_bins
            } else {
                o.kbnblk
            };
            let last_bin = prev + (prev_bins - 1) * bnsiz;
            let new = *kfsize;
            w.set(last_bin + bnsiz - 1, new);
            blocks.push(new);
            *kfsize += o.kbnblk * bnsiz;
            w.ensure(*kfsize);
            *next_bin = new;
            *bins_left = o.kbnblk;
        }
        let b = *next_bin;
        *next_bin += bnsiz;
        *bins_left -= 1;
        b
    };

    let mut live = 0;
    for r in recs {
        let npath = r.path.len() as i32;
        let p = (npath - 1) / 4 + 1;
        let h = pathname_hash(&r.path, o.khash);
        // Find the chain's bin with room.
        let mut bin = if o.ktable == 1 {
            let t = w.get(nperm + h);
            if t == 0 {
                let b = alloc_bin(
                    &mut w,
                    &mut kfsize,
                    &mut blocks,
                    &mut next_bin,
                    &mut bins_left,
                );
                w.set(nperm + h, b);
                b
            } else {
                t
            }
        } else {
            kafbin + (h - 1) * bnsiz
        };
        let j = loop {
            // Walk to the end of this bin.
            let mut j = 1;
            let mut full = false;
            loop {
                let s = w.get(bin + j - 1);
                if s == 0 {
                    break;
                }
                if s == -1 {
                    full = true;
                    break;
                }
                let np = w.get(bin + j);
                j += (np - 1) / 4 + 1 + 11;
            }
            if full {
                bin = w.get(bin + bnsiz - 2);
                continue;
            }
            // znwrit6.f lines 98-104: room for the entry plus 2 words.
            if j + p + 11 + 2 <= bnsiz {
                break j;
            }
            let nb = alloc_bin(
                &mut w,
                &mut kfsize,
                &mut blocks,
                &mut next_bin,
                &mut bins_left,
            );
            w.set(bin + j - 1, -1);
            w.set(bin + bnsiz - 2, nb);
            overflows.push((bin, nb));
            bin = nb;
        };
        // Info block + 4 data words at EOF.
        let info = kfsize;
        let ninfo = 28 + p;
        kfsize += ninfo + 4;
        w.ensure(kfsize);
        let e = bin + j - 1;
        w.set(e, r.status);
        w.set(e + 1, npath);
        for (i, v) in pack(&r.path, p as usize).into_iter().enumerate() {
            w.set(e + 2 + i as i32, v);
            w.set(info + 3 + i as i32, v);
        }
        let o_ = e + 2 + p;
        w.set(o_, info);
        w.set(o_ + 1, 0);
        w.set(o_ + 2, 4);
        w.set(o_ + 3, 4);
        w.set(o_ + 4, r.rtype);
        let tag = pack(b"T1", 2);
        w.set(o_ + 5, tag[0]);
        w.set(o_ + 6, tag[1]);
        w.set(o_ + 7, h);
        w.set(o_ + 8, r.pseudo);
        w.set(info, -9753);
        w.set(info + 1, r.status);
        w.set(info + 2, npath);
        let q = info + p; // 0-based offsets + P
        w.set(q + 5, info + ninfo);
        w.set(q + 6, 4);
        w.set(q + 7, 4);
        w.set(q + 8, 1);
        let prog = pack(b"P07TST", 2);
        w.set(q + 9, prog[0]);
        w.set(q + 10, prog[1]);
        w.set(q + 17, r.rtype);
        info_addrs.push((r.path.clone(), info));
        if r.status == 1 || r.status == 11 {
            live += 1;
        }
    }
    w.set(kfsize, -9630);

    // Header (zinit6.f lines 446-572).
    let hdr: [(usize, i32); 13] = [
        (0, i32::from_le_bytes(*b"ZDSS")),
        (1, o.knrecs.unwrap_or(live)),
        (3, 1),
        (4, i32::from_le_bytes(o.version)),
        (5, kfsize),
        (14, o.khash),
        (15, o.ktable),
        (16, o.kbnblk),
        (17, bins_left),
        (18, bnsiz),
        (19, kafbin),
        (20, next_bin),
        (37, 0),
    ];
    for (i, v) in hdr {
        w.set(i as i32 + 1, v);
    }
    let date = pack(b"19OCT26\0", 2);
    w.set(8, date[0]);
    w.set(9, date[1]);

    // Serialize: whole records, 127 words + 4 garbage pad bytes each.
    let nrec = (kfsize as usize).div_ceil(127);
    w.ensure(nrec as i32 * 127);
    let mut bytes = Vec::with_capacity(nrec * 512);
    for r in 0..nrec {
        let rec = &w.words[r * 127..r * 127 + 127];
        if r as i64 + 1 == lock_record {
            bytes.extend(std::iter::repeat(0xA5u8).take(512));
            continue;
        }
        for v in rec {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    }
    if o.short_final {
        bytes.truncate(bytes.len() - 4);
    }
    Built {
        bytes,
        kafbin,
        kfsize,
        lock_record,
        info_addrs,
        blocks,
        overflows,
    }
}

/// Byte source that records every read range, to prove the lock record is never read.
pub struct Spy<'a> {
    pub data: &'a [u8],
    pub reads: RefCell<Vec<(u64, u64)>>,
}

impl<'a> Spy<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            reads: RefCell::new(Vec::new()),
        }
    }
    pub fn touched(&self, lo: u64, hi: u64) -> bool {
        self.reads.borrow().iter().any(|&(s, e)| s < hi && e > lo)
    }
}

impl ReadAt for Spy<'_> {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.reads
            .borrow_mut()
            .push((offset, offset + buf.len() as u64));
        self.data.read_exact_at(offset, buf)
    }
}

/// A mixed set of records: long paths (status 11), several types, deleted, renamed and
/// rewritten-old entries, a Latin-1 pathname, and enough entries to overflow bins and
/// add bin blocks with `KHASH = 8`.
pub fn mixed_records() -> Vec<Rec> {
    let mut v = Vec::new();
    for i in 0..60 {
        v.push(Rec::live(
            &format!("/BASIN/LOC{i:03}/FLOW/01JAN2000/1HOUR/OBS/"),
            100,
        ));
    }
    v.push(Rec::live("//TABLE 1/STORAGE-FLOW///TABLE/", 200));
    v.push(Rec::live(
        "/SHG/MARFC/PRECIP/10SEP2018:0600/10SEP2018:1200/QPF/",
        420,
    ));
    let long = format!(
        "/A/{}/FLOW/01JAN2000/1DAY/{}/",
        "B".repeat(40),
        "F".repeat(40)
    );
    v.push(Rec::live(&long, 105));
    let latin = b"/R\xCDO/GUINEO/FLOW/01JAN2000/1HOUR/PMP/".to_vec();
    v.push(Rec {
        path: latin,
        rtype: 105,
        status: 1,
        pseudo: 0,
    });
    v.push(Rec::live("/X/DELETED/FLOW/01JAN2000/1HOUR/OBS/", 100).with_status(2));
    v.push(Rec::live("/X/RENAMED/FLOW/01JAN2000/1HOUR/OBS/", 100).with_status(3));
    v.push(Rec::live("/X/REWRITTEN/FLOW/01JAN2000/1HOUR/OBS/", 100).with_status(4));
    v.push(Rec::live("/X/DELETED-LONG/FLOW/01JAN2000/1HOUR/OBS/", 100).with_status(12));
    v.push(Rec {
        path: b"/A/B/FLOW/01JAN2000/IR-YEAR/OBS/".to_vec(),
        rtype: 110,
        status: 1,
        pseudo: 1440,
    });
    v
}

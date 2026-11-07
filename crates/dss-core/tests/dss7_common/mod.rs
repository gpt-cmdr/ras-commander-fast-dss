//! Test support for the DSS7 reader tests: a small synthetic DSS7 file builder and
//! locations of optional public fixtures.
//!
//! The builder follows `docs/format/dss7.md` §2.3 (layout of a new file), §3.4 (bin
//! entries; placement rule from `Internal/zbinUpdate.c:zbinUpdate()`), §3.5 (bin blocks),
//! and §5.1 (info blocks). It exists so that header, catalog, anomaly and fuzz tests run
//! without any external file.

#![allow(dead_code)]

use dss_core::dss7::{pathname_hash, table_hash};
use std::path::PathBuf;

pub const HT: i64 = 178;

#[derive(Clone, Debug)]
pub struct Rec {
    pub path: Vec<u8>,
    pub status: i64,
    pub rtype: i32,
    /// Leave the info block all zeros (interrupted write).
    pub zero_info: bool,
    /// For status 2: index of the primary record.
    pub alias_of: Option<usize>,
    /// Put this entry in a bin that no hash-table slot reaches.
    pub orphan: bool,
    pub last_write_ms: i64,
    pub julian: (i32, i32),
}

impl Rec {
    pub fn live(path: &str, rtype: i32) -> Self {
        Rec {
            path: path.as_bytes().to_vec(),
            status: 1,
            rtype,
            zero_info: false,
            alias_of: None,
            orphan: false,
            last_write_ms: 1_700_000_000_000,
            julian: if (100..200).contains(&rtype) {
                (43350, 43351)
            } else {
                (0, 0)
            },
        }
    }
    pub fn with_status(mut self, s: i64) -> Self {
        self.status = s;
        self
    }
    pub fn orphan(mut self) -> Self {
        self.orphan = true;
        self
    }
    pub fn zero_info(mut self) -> Self {
        self.zero_info = true;
        self
    }
}

pub struct Spec {
    pub version: [u8; 4],
    pub max_hash: i64,
    pub bin_size: i64,
    pub bins_per_block: i64,
    pub recs: Vec<Rec>,
    /// Header record count override (default: number of status 0/1 records).
    pub number_records: Option<i64>,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            version: *b"7-IU",
            max_hash: 64,
            bin_size: 40,
            bins_per_block: 4,
            recs: Vec::new(),
            number_records: None,
        }
    }
}

pub struct Built {
    pub bytes: Vec<u8>,
    /// Info word address per record (0 for aliases).
    pub info_addr: Vec<i64>,
    /// Bin entry word address per record.
    pub entry_addr: Vec<i64>,
}

fn put(buf: &mut Vec<u8>, word: i64, v: i64) {
    let o = (word as usize) * 8;
    if buf.len() < o + 8 {
        buf.resize(o + 8, 0);
    }
    buf[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_bytes(buf: &mut Vec<u8>, word: i64, b: &[u8]) {
    let o = (word as usize) * 8;
    let n = b.len().div_ceil(8) * 8;
    if buf.len() < o + n {
        buf.resize(o + n, 0);
    }
    buf[o..o + b.len()].copy_from_slice(b);
}

fn i4(a: i32, b: i32) -> i64 {
    (a as u32 as i64) | ((b as i64) << 32)
}

pub fn build(spec: &Spec) -> Built {
    let bs = spec.bin_size;
    let bpb = spec.bins_per_block;
    let mh = spec.max_hash;
    let mut buf = vec![0u8; 0];
    let block_words = bs * bpb + 1;
    let first_bin = HT + mh;

    // Bin allocation: sequential bins inside chained blocks.
    let mut blocks: Vec<i64> = vec![first_bin];
    let mut next_bin = 0i64; // index of next free bin
    let mut alloc_bin = |blocks: &mut Vec<i64>| -> i64 {
        let blk = (next_bin / bpb) as usize;
        while blocks.len() <= blk {
            let a = *blocks.last().unwrap() + block_words;
            blocks.push(a);
        }
        let a = blocks[blk] + (next_bin % bpb) * bs;
        next_bin += 1;
        a
    };

    // Group by table hash (orphans in their own group).
    let n = spec.recs.len();
    let mut entry_addr = vec![0i64; n];
    let mut chains: std::collections::BTreeMap<i64, Vec<i64>> = Default::default(); // key -> bins
    let mut used: std::collections::HashMap<i64, i64> = Default::default();
    let mut placement: Vec<(i64, i64)> = vec![(0, 0); n]; // (bin, icount)
    for (i, r) in spec.recs.iter().enumerate() {
        let key = if r.orphan {
            -1
        } else {
            table_hash(&r.path, mh)
        };
        let pw = (r.path.len() as i64 + 7) / 8;
        let bins = chains.entry(key).or_default();
        let mut placed = false;
        for &b in bins.iter() {
            let u = used[&b];
            if u + 7 + pw + 1 < bs {
                placement[i] = (b, u);
                used.insert(b, u + 7 + pw);
                placed = true;
                break;
            }
        }
        if !placed {
            let b = alloc_bin(&mut blocks);
            bins.push(b);
            used.insert(b, 7 + pw);
            placement[i] = (b, 0);
        }
    }
    let last_bin_end = blocks.last().unwrap() + block_words;
    let mut info_next = last_bin_end;
    let mut info_addr = vec![0i64; n];
    for (i, r) in spec.recs.iter().enumerate() {
        if r.alias_of.is_none() {
            info_addr[i] = info_next;
            info_next += 30 + (r.path.len() as i64 + 7) / 8 + 4;
        }
    }
    let eof = info_next;
    buf.resize(((eof + 1) * 8) as usize, 0);

    // Bin entries.
    for (i, r) in spec.recs.iter().enumerate() {
        let (b, ic) = placement[i];
        let e = b + ic;
        entry_addr[i] = e;
        let pw = (r.path.len() as i32 + 7) / 8;
        put(&mut buf, e, pathname_hash(&r.path));
        put(&mut buf, e + 1, r.status);
        put(&mut buf, e + 2, i4(r.path.len() as i32, pw));
        let ia = match r.alias_of {
            Some(p) => info_addr[p],
            None => info_addr[i],
        };
        put(&mut buf, e + 3, ia);
        put(&mut buf, e + 4, i4(r.rtype, 0));
        put(&mut buf, e + 5, r.last_write_ms);
        put(&mut buf, e + 6, i4(r.julian.0, r.julian.1));
        put_bytes(&mut buf, e + 7, &r.path);
    }
    // Overflow links and table slots.
    for (&key, bins) in &chains {
        for w in bins.windows(2) {
            put(&mut buf, w[0] + bs - 1, w[1]);
        }
        if key >= 0 {
            put(&mut buf, HT + key, bins[0]);
        }
    }
    // Block links.
    for w in blocks.windows(2) {
        put(&mut buf, w[0] + bs * bpb, w[1]);
    }
    // Info blocks.
    for (i, r) in spec.recs.iter().enumerate() {
        if r.alias_of.is_some() || r.zero_info {
            continue;
        }
        let a = info_addr[i];
        put(&mut buf, a, -97534);
        put(
            &mut buf,
            a + 1,
            if r.status == 0 {
                0
            } else if r.status == 2 {
                1
            } else {
                r.status
            },
        );
        put(&mut buf, a + 2, r.path.len() as i64);
        put(&mut buf, a + 3, pathname_hash(&r.path));
        put(&mut buf, a + 4, i4(r.rtype, 1));
        put(&mut buf, a + 6, r.last_write_ms);
        put_bytes(&mut buf, a + 7, b"RUST-TEST");
        put(&mut buf, a + 9, i4(r.julian.0, 60));
        put(&mut buf, a + 10, i4(r.julian.1, 86400));
        put_bytes(&mut buf, a + 30, &r.path);
    }

    // Header (dss7.md §2.2).
    buf[0..5].copy_from_slice(b"ZDSS\0");
    put(&mut buf, 1, 100);
    buf[16..20].copy_from_slice(&spec.version);
    let live = spec
        .recs
        .iter()
        .filter(|r| r.status == 0 || r.status == 1)
        .count() as i64;
    put(&mut buf, 3, spec.number_records.unwrap_or(live));
    put(
        &mut buf,
        4,
        spec.recs.iter().filter(|r| r.status == 2).count() as i64,
    );
    put(&mut buf, 5, eof);
    put(&mut buf, 14, 100);
    put(&mut buf, 15, mh);
    put(&mut buf, 19, HT);
    put(&mut buf, 21, bpb);
    put(&mut buf, 23, bs);
    put(&mut buf, 24, first_bin);
    put(&mut buf, 26, next_bin);
    put(&mut buf, 90, 25);
    put(&mut buf, 91, 103);
    put(&mut buf, 92, 128);
    put(&mut buf, 93, 153);
    put(&mut buf, 99, -97531);
    put(&mut buf, eof, -97532);
    Built {
        bytes: buf,
        info_addr,
        entry_addr,
    }
}

/// A representative synthetic file: live, undeleted, deleted, renamed, an alias, enough
/// records to force overflow bins and more than one bin block.
pub fn standard_spec() -> Spec {
    let mut recs = Vec::new();
    for i in 0..40 {
        recs.push(Rec::live(
            &format!("/BASIN/LOC{i}/FLOW/01Jan2020/1Hour/RUN{}/", i % 3),
            100,
        ));
    }
    recs.push(Rec::live("/BASIN/CURVE/STAGE-FLOW///RATING/", 200));
    recs.push(Rec::live("/BASIN/OLD/FLOW/01Jan2020/1Hour/RUN/", 105).with_status(11));
    recs.push(Rec::live("/BASIN/RENAMED/FLOW/01Jan2020/1Hour/RUN/", 105).with_status(12));
    recs.push(Rec::live("/BASIN/UNDELETED/FLOW/01Jan2020/1Day/RUN/", 105).with_status(0));
    let mut alias = Rec::live("/BASIN/ALIAS/STAGE-FLOW///RATING/", 200).with_status(2);
    alias.alias_of = Some(40);
    recs.push(alias);
    Spec {
        recs,
        ..Spec::default()
    }
}

pub fn set_word(buf: &mut [u8], word: usize, v: i64) {
    buf[word * 8..word * 8 + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn get_word(buf: &[u8], word: usize) -> i64 {
    i64::from_le_bytes(buf[word * 8..word * 8 + 8].try_into().unwrap())
}

/// Optional public fixtures: `fixtures/public/<name>` in the repo, else the P02 golden
/// copy under `DSS_GOLDEN_DIR` when that variable is set.
pub fn public_fixture(golden_id: &str, name: &str) -> Option<PathBuf> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name);
    if repo.is_file() {
        return Some(repo);
    }
    let base = std::env::var_os("DSS_GOLDEN_DIR")?;
    let p = PathBuf::from(base).join(golden_id).join(name);
    p.is_file().then_some(p)
}

pub fn sample7() -> Option<PathBuf> {
    public_fixture("g01_v7_sample7", "sample7.dss")
}

pub fn bald_eagle() -> Option<PathBuf> {
    public_fixture("g02_v7_BaldEagleDamBrk", "BaldEagleDamBrk.dss")
}

/// Deterministic xorshift64* generator for fuzz sweeps.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

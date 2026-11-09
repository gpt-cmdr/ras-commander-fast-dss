//! DSS6 catalog: the bin-block walk of `zcatfi6`, plus optional verification.
//!
//! Sources:
//! - Walk order and termination: `heclib_f/src/zcatfi6.f:ZCATFI6` lines 61-237 (also
//!   `heclib_f/src/zplist6.f:ZPLIST6` lines 97-237 and
//!   `heclib_c/src/Dss6/zcatalog6Internal.c` lines 164-212).
//! - Bin entry layout (`KBSTAT` .. `KBPINTL`, `NLBIN = 11`) and reserved words 126/127:
//!   `heclib_f/src/zinit6.f:ZINIT6` lines 120-145 and 654-696; `znwbin6.f:ZNWBIN6`
//!   lines 51-84.
//! - Status codes: `zinit6.f` lines 120-131; `znwrit6.f` lines 110-114; `zdelet6.f`
//!   lines 54-63; `zcheck6.f` line 301.
//! - Hash-table chains: `heclib_f/src/zcheck6.f:ZCHECK6` lines 211-322.
//!
//! Differences from HEC, all deliberate (dss6.md §6.1): the walk does not stop at
//! `KNRECS` unless asked, it lists non-live entries with their raw status, and it records
//! anomalies (count mismatch, bad pointers, cycles) instead of failing.

use super::hash::pathname_hash;
use super::info::{path_words, unpack_chars};
use super::pathname::pseudo_display_path;
use super::source::word_byte_offset;
use super::{Dss6Error, Dss6File, ReadAt, MAX_PATHNAME, NLBIN, NPERM};
use crate::catalog::{AnomalyKind, Catalog, CatalogAnomaly, CatalogEntry, RecordStatus};
use crate::chain::ChainHops;
use std::collections::{HashMap, HashSet};

/// Bins read per I/O when walking a block.
const BINS_PER_READ: i64 = 32;
/// Most anomalies of one kind kept per walk; the rest are counted in a summary entry.
const MAX_ANOMALIES_PER_KIND: usize = 100;

/// How much of each record's info block to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InfoCheck {
    /// Bins only (fast; what HEC's short catalog reads).
    #[default]
    None,
    /// Every live entry.
    All,
    /// About this many live entries, evenly spaced in bin order.
    Sample(usize),
}

/// Catalog walk options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogOptions {
    /// Stop once `KNRECS` live entries are found, as `zcatfi6` does (line 196). Off by
    /// default: the full walk finds entries past a stale count and reports the mismatch.
    pub stop_at_knrecs: bool,
    /// Recompute `zhash6` for live entries and compare with `KBHASH`.
    pub verify_hashes: bool,
    /// Check that every live entry's bin is reachable from its hash-table slot.
    pub verify_table_chains: bool,
    /// Report live pathnames listed more than once.
    pub check_duplicates: bool,
    /// Validate info blocks (flag -9753, status, pathname, length, type).
    pub verify_info: InfoCheck,
}

impl Default for CatalogOptions {
    fn default() -> Self {
        Self {
            stop_at_knrecs: false,
            verify_hashes: true,
            verify_table_chains: true,
            check_duplicates: true,
            verify_info: InfoCheck::None,
        }
    }
}

/// One bin entry (dss6.md §4.5), with word addresses as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dss6BinEntry {
    /// `KBSTAT`.
    pub status: i32,
    /// `KBPATH`, `KBNPAT` characters.
    pub pathname: Vec<u8>,
    /// `KBAINF`: info block word address.
    pub info_address: i32,
    /// `KBNHEA`: user header length in words.
    pub user_header_words: i32,
    /// `KBNDAT`: stored data length in words.
    pub data_words: i32,
    /// `KBLNDA`: logical number of data (can be stale; use the info block).
    pub logical_values: i32,
    /// `KBTYPE`.
    pub record_type: i32,
    /// `KBTAG`, 8 characters.
    pub tag: [u8; 8],
    /// `KBHASH`.
    pub hash: i32,
    /// `KBPINTL`: pseudo-regular interval in minutes (0 = none).
    pub pseudo_interval: i32,
    /// Word address of the bin holding the entry.
    pub bin_address: i64,
    /// Word address of the entry's status word.
    pub entry_address: i64,
}

impl Dss6BinEntry {
    pub fn record_status(&self) -> RecordStatus {
        match self.status {
            1 | 11 => RecordStatus::Live,
            2 | 12 => RecordStatus::Deleted,
            3 | 13 => RecordStatus::Renamed,
            4 | 14 => RecordStatus::RewrittenOld,
            _ => RecordStatus::Other,
        }
    }

    pub fn is_live(&self) -> bool {
        self.status == 1 || self.status == 11
    }

    /// Pathname as HEC catalogs it: for pseudo-regular records (`KBPINTL > 0`) the E part
    /// shows `~<interval>` (dss6.md §6.3). Otherwise the stored pathname.
    pub fn display_pathname(&self) -> Vec<u8> {
        if self.pseudo_interval > 0 {
            if let Some(p) = pseudo_display_path(&self.pathname, self.pseudo_interval) {
                return p;
            }
        }
        self.pathname.clone()
    }

    pub fn to_catalog_entry(&self) -> CatalogEntry {
        CatalogEntry {
            pathname: self.pathname.clone(),
            record_type: self.record_type,
            status: self.record_status(),
            raw_status: self.status,
            stored_hash: i64::from(self.hash),
            info_address: word_byte_offset(self.info_address.into()).unwrap_or(0),
            bin_address: word_byte_offset(self.entry_address).unwrap_or(0),
            last_write_ms: None,
        }
    }
}

/// Catalog type abbreviation (`zinit6.f:ZBKDAT` lines 934-971; others are `UND`,
/// `zcatfi6.f` lines 178-185).
pub fn record_type_abbrev(code: i32) -> &'static str {
    match code {
        100 => "RTS",
        101 => "RTP",
        105 => "RTD",
        110 => "ITS",
        111 => "ITP",
        115 => "ITD",
        200 => "PD",
        205 => "PDD",
        300 => "TXT",
        400 => "GUT",
        401 => "GU",
        410 => "GHT",
        411 => "GH",
        420 => "GAT",
        421 => "GA",
        430 => "GST",
        431 => "GS",
        _ => "UND",
    }
}

/// Result of a raw bin walk.
#[derive(Debug, Clone, Default)]
pub struct BinWalk {
    /// Entries with status > 0, in bin order.
    pub entries: Vec<Dss6BinEntry>,
    pub anomalies: Vec<CatalogAnomaly>,
    /// Bin blocks and bins visited.
    pub blocks: usize,
    pub bins: usize,
    /// Bins marked full (status -1), with their overflow pointer (word 126).
    pub overflow: HashMap<i64, i64>,
    /// Live entries checked against their info block.
    pub info_checked: usize,
}

impl BinWalk {
    pub fn live(&self) -> impl Iterator<Item = &Dss6BinEntry> {
        self.entries.iter().filter(|e| e.is_live())
    }

    pub fn into_catalog(self) -> Catalog {
        Catalog {
            entries: self
                .entries
                .iter()
                .map(Dss6BinEntry::to_catalog_entry)
                .collect(),
            anomalies: self.anomalies,
        }
    }
}

struct Anomalies {
    list: Vec<CatalogAnomaly>,
    counts: HashMap<AnomalyKind, usize>,
}

impl Anomalies {
    fn push(&mut self, kind: AnomalyKind, word_address: Option<i64>, detail: String) {
        let n = self.counts.entry(kind).or_insert(0);
        *n += 1;
        if *n <= MAX_ANOMALIES_PER_KIND {
            let address = word_address.and_then(word_byte_offset);
            self.list.push(CatalogAnomaly {
                kind,
                address,
                detail,
            });
        }
    }

    fn finish(mut self) -> Vec<CatalogAnomaly> {
        let mut over: Vec<_> = self
            .counts
            .iter()
            .filter(|(_, &n)| n > MAX_ANOMALIES_PER_KIND)
            .collect();
        over.sort_by_key(|(k, _)| format!("{k:?}"));
        for (kind, n) in over {
            self.list.push(CatalogAnomaly {
                kind: *kind,
                address: None,
                detail: format!(
                    "{n} anomalies of this kind; first {MAX_ANOMALIES_PER_KIND} listed"
                ),
            });
        }
        self.list
    }
}

impl<S: ReadAt> Dss6File<S> {
    /// Catalog with default options: full bin walk, hash and chain checks, no info reads.
    pub fn catalog(&self) -> Result<Catalog, Dss6Error> {
        self.catalog_with(&CatalogOptions::default())
    }

    /// Catalog with explicit options. Fails only on I/O errors; damaged structures become
    /// anomalies.
    pub fn catalog_with(&self, opts: &CatalogOptions) -> Result<Catalog, Dss6Error> {
        Ok(self.walk_bins(opts)?.into_catalog())
    }

    /// Walk the bin blocks in `zcatfi6` order and return the raw entries.
    pub fn walk_bins(&self, opts: &CatalogOptions) -> Result<BinWalk, Dss6Error> {
        let h = &self.header;
        let mut an = Anomalies {
            list: self.warnings.clone(),
            counts: HashMap::new(),
        };
        let mut walk = BinWalk::default();
        let bnsiz = h.bin_size as usize;
        let bnsiz_i = i64::from(h.bin_size);
        // zcatfi6.f lines 62-69: the first block has KBNBLK bins (type 1) or all KHASH
        // preallocated bins (type 2); later blocks have KBNBLK.
        let mut nbin = if h.table_type == 1 {
            i64::from(h.bins_per_block)
        } else {
            i64::from(h.max_hash)
        };
        let mut block_addr = i64::from(h.first_bin_address);
        // Every bin occupies KBNSIZ distinct words, so no walk can visit more bins than
        // fit in the file; more means a cycle.
        let max_bins = (self.limit_words / bnsiz_i + 2) as usize;
        let mut seen_blocks = HashSet::new();
        let mut live = 0i64;
        let knrecs = i64::from(h.number_records);

        'blocks: loop {
            if !seen_blocks.insert(block_addr) {
                an.push(
                    AnomalyKind::CycleDetected,
                    Some(block_addr),
                    format!("bin block {block_addr} revisited"),
                );
                break;
            }
            walk.blocks += 1;
            let mut last_bin: Vec<i32> = Vec::new();
            let mut i = 0i64;
            while i < nbin {
                let chunk = (nbin - i).min(BINS_PER_READ);
                let addr = block_addr + i * bnsiz_i;
                let words = match self.read_bins(addr, chunk, bnsiz) {
                    Ok(w) => w,
                    Err(Dss6Error::Io(e)) => return Err(Dss6Error::Io(e)),
                    Err(e) => {
                        an.push(
                            AnomalyKind::OutOfBounds,
                            Some(addr),
                            format!("bin read failed: {e}"),
                        );
                        break 'blocks;
                    }
                };
                for (b, bin) in words.chunks_exact(bnsiz).enumerate() {
                    walk.bins += 1;
                    if walk.bins > max_bins {
                        an.push(
                            AnomalyKind::CycleDetected,
                            Some(addr),
                            format!("more than {max_bins} bins walked"),
                        );
                        break 'blocks;
                    }
                    let bin_addr = addr + b as i64 * bnsiz_i;
                    let stop = scan_bin(bin, bin_addr, &mut walk, &mut an, |e| {
                        if e.is_live() {
                            live += 1;
                        }
                        opts.stop_at_knrecs && e.is_live() && live >= knrecs
                    });
                    if stop {
                        break 'blocks;
                    }
                }
                last_bin = words[words.len() - bnsiz..].to_vec();
                if (words.len() / bnsiz) < chunk as usize {
                    let at = addr + (words.len() / bnsiz) as i64 * bnsiz_i;
                    an.push(
                        AnomalyKind::OutOfBounds,
                        Some(at),
                        format!("bin at word {at} outside the file"),
                    );
                    break 'blocks;
                }
                i += chunk;
            }
            // zcatfi6.f lines 200-224: word KBNSIZ of the block's last bin links the next
            // block; 0 ends the walk.
            let Some(&link) = last_bin.last() else { break };
            if link == 0 {
                break;
            }
            let link = i64::from(link);
            if link < 1 || link > self.limit_words {
                an.push(
                    AnomalyKind::OutOfBounds,
                    Some(block_addr),
                    format!("next bin block address {link} out of range"),
                );
                break;
            }
            block_addr = link;
            nbin = i64::from(h.bins_per_block);
        }

        let live_n = walk.live().count() as i64;
        if live_n != knrecs {
            an.push(
                AnomalyKind::RecordCountMismatch,
                Some(2),
                format!("walk found {live_n} live entries, header KNRECS = {knrecs}"),
            );
        }
        if opts.check_duplicates {
            let mut seen: HashSet<&[u8]> = HashSet::new();
            for e in walk.entries.iter().filter(|e| e.is_live()) {
                if !seen.insert(&e.pathname) {
                    an.push(
                        AnomalyKind::DuplicatePathname,
                        Some(e.entry_address),
                        format!(
                            "pathname listed live more than once: {}",
                            latin1(&e.pathname)
                        ),
                    );
                }
            }
        }
        if opts.verify_hashes {
            for e in walk.entries.iter().filter(|e| e.is_live()) {
                let calc = pathname_hash(&e.pathname, h.max_hash);
                if calc != e.hash {
                    an.push(
                        AnomalyKind::HashMismatch,
                        Some(e.entry_address),
                        format!(
                            "stored hash {} computed {calc}: {}",
                            e.hash,
                            latin1(&e.pathname)
                        ),
                    );
                }
            }
        }
        if opts.verify_table_chains {
            self.verify_chains(&walk, &mut an)?;
        }
        self.verify_infos(opts.verify_info, &mut walk, &mut an)?;
        walk.anomalies = an.finish();
        Ok(walk)
    }

    /// Read `count` contiguous bins; on a bounds failure fall back to the readable prefix.
    fn read_bins(&self, addr: i64, count: i64, bnsiz: usize) -> Result<Vec<i32>, Dss6Error> {
        match self.read_words(addr, count as usize * bnsiz) {
            Ok(w) => Ok(w),
            Err(Dss6Error::Io(e)) => Err(Dss6Error::Io(e)),
            Err(first) => {
                let mut out = Vec::new();
                for b in 0..count {
                    match self.read_words(addr + b * bnsiz as i64, bnsiz) {
                        Ok(w) => out.extend(w),
                        Err(Dss6Error::Io(e)) => return Err(Dss6Error::Io(e)),
                        Err(_) => break,
                    }
                }
                if out.is_empty() {
                    Err(first)
                } else {
                    Ok(out)
                }
            }
        }
    }

    /// Every live entry must be reachable from hash-table slot `KBHASH` through the
    /// overflow chain (`zcheck6.f` lines 211-322). Unreachable ones are [`AnomalyKind::OrphanBin`].
    fn verify_chains(&self, walk: &BinWalk, an: &mut Anomalies) -> Result<(), Dss6Error> {
        let h = &self.header;
        if walk.live().next().is_none() {
            return Ok(());
        }
        let table = if h.table_type == 1 {
            match self.read_words(NPERM as i64 + 1, h.max_hash as usize) {
                Ok(t) => Some(t),
                Err(Dss6Error::Io(e)) => return Err(Dss6Error::Io(e)),
                Err(e) => {
                    an.push(
                        AnomalyKind::OutOfBounds,
                        Some(NPERM as i64 + 1),
                        format!("hash table unreadable: {e}"),
                    );
                    return Ok(());
                }
            }
        } else {
            None
        };
        let hops_max = walk.overflow.len() + 2;
        // Each distinct chain start is walked once (`crate::chain`); the answer is
        // the per-entry walk's (at most `hops_max` bins from the slot).
        let mut chains = ChainHops::new(
            &walk.overflow,
            hops_max,
            ChainHops::default_budget(walk.overflow.len(), h.max_hash.max(0) as usize),
        );
        for e in walk.live() {
            if !(1..=h.max_hash).contains(&e.hash) {
                an.push(
                    AnomalyKind::OrphanBin,
                    Some(e.entry_address),
                    format!("stored hash {} outside table", e.hash),
                );
                continue;
            }
            let start = match &table {
                Some(t) => i64::from(t[(e.hash - 1) as usize]),
                None => {
                    i64::from(h.first_bin_address) + i64::from(e.hash - 1) * i64::from(h.bin_size)
                }
            };
            let found = chains.hop(start, e.bin_address).is_some();
            if !found {
                an.push(
                    AnomalyKind::OrphanBin,
                    Some(e.entry_address),
                    format!(
                        "bin {} not reachable from hash slot {}: {}",
                        e.bin_address,
                        e.hash,
                        latin1(&e.pathname)
                    ),
                );
            }
        }
        Ok(())
    }

    /// Info block checks (`zrdinf6.f:ZRDINF6` lines 56-62 and dss6.md §4.6 step 4).
    fn verify_infos(
        &self,
        mode: InfoCheck,
        walk: &mut BinWalk,
        an: &mut Anomalies,
    ) -> Result<(), Dss6Error> {
        let live: Vec<usize> = walk
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_live())
            .map(|(i, _)| i)
            .collect();
        let pick: Vec<usize> = match mode {
            InfoCheck::None => return Ok(()),
            InfoCheck::All => live,
            InfoCheck::Sample(n) if n >= live.len() => live,
            InfoCheck::Sample(0) => Vec::new(),
            InfoCheck::Sample(n) => (0..n).map(|k| live[k * live.len() / n]).collect(),
        };
        for i in pick {
            let e = &walk.entries[i];
            walk.info_checked += 1;
            let addr = i64::from(e.info_address);
            let info = match self.read_info(addr, e.pathname.len() as i32) {
                Ok(info) => info,
                Err(Dss6Error::Io(err)) => return Err(Dss6Error::Io(err)),
                Err(err @ (Dss6Error::OutOfBounds { .. } | Dss6Error::LockRecord { .. })) => {
                    an.push(
                        AnomalyKind::OutOfBounds,
                        Some(e.entry_address),
                        format!("info block: {err}"),
                    );
                    continue;
                }
                Err(err) => {
                    an.push(
                        AnomalyKind::BadInfoBlock,
                        Some(addr),
                        format!("{err}: {}", latin1(&e.pathname)),
                    );
                    continue;
                }
            };
            let mut problems = Vec::new();
            if !info.has_valid_flag() {
                problems.push(format!("flag {}", info.flag));
            }
            if info.status != e.status {
                problems.push(format!("status {} vs bin {}", info.status, e.status));
            }
            if info.pathname != e.pathname {
                problems.push("pathname differs from bin".to_string());
            }
            if info.record_type != e.record_type {
                problems.push(format!(
                    "type {} vs bin {}",
                    info.record_type, e.record_type
                ));
            }
            if !problems.is_empty() {
                an.push(
                    AnomalyKind::BadInfoBlock,
                    Some(addr),
                    format!("{}: {}", problems.join(", "), latin1(&e.pathname)),
                );
            }
        }
        Ok(())
    }
}

/// Scan one bin (`zcatfi6.f` lines 91-198). `on_entry` returns true to stop the walk.
/// Returns true when the walk should stop.
fn scan_bin(
    bin: &[i32],
    bin_addr: i64,
    walk: &mut BinWalk,
    an: &mut Anomalies,
    mut on_entry: impl FnMut(&Dss6BinEntry) -> bool,
) -> bool {
    let bnsiz = bin.len();
    let mut k = 0usize; // 0-based; HEC's JPNBIN is k + 1
    while k < bnsiz {
        let status = bin[k];
        let at = bin_addr + k as i64;
        if status <= 0 {
            // 0: no more entries; -1: bin full, overflow pointer in word KBNSIZ-1.
            if status == -1 {
                walk.overflow.insert(bin_addr, i64::from(bin[bnsiz - 2]));
            } else if status != 0 {
                an.push(
                    AnomalyKind::Other,
                    Some(at),
                    format!("bin status {status} ends bin"),
                );
            }
            break;
        }
        // zplist6 treats |status| > 1000 as corruption (dss6.md §4.5).
        if status > 1000 {
            an.push(
                AnomalyKind::Other,
                Some(at),
                format!("bin status {status} out of range"),
            );
            break;
        }
        let Some(&npath) = bin.get(k + 1) else {
            an.push(
                AnomalyKind::OutOfBounds,
                Some(at),
                "bin entry crosses the bin end".into(),
            );
            break;
        };
        if !(1..=MAX_PATHNAME).contains(&npath) {
            an.push(
                AnomalyKind::Other,
                Some(at),
                format!("bin entry pathname length {npath}"),
            );
            break;
        }
        let p = path_words(npath);
        if k + p + NLBIN > bnsiz {
            an.push(
                AnomalyKind::OutOfBounds,
                Some(at),
                "bin entry crosses the bin end".into(),
            );
            break;
        }
        let o = k + 2 + p; // KBAINF
        let mut tag = [0u8; 8];
        tag[..4].copy_from_slice(&bin[o + 5].to_le_bytes());
        tag[4..].copy_from_slice(&bin[o + 6].to_le_bytes());
        let entry = Dss6BinEntry {
            status,
            pathname: unpack_chars(&bin[k + 2..k + 2 + p], npath as usize),
            info_address: bin[o],
            user_header_words: bin[o + 1],
            data_words: bin[o + 2],
            logical_values: bin[o + 3],
            record_type: bin[o + 4],
            tag,
            hash: bin[o + 7],
            pseudo_interval: bin[o + 8],
            bin_address: bin_addr,
            entry_address: at,
        };
        if !matches!(status, 1..=4 | 11..=14) {
            an.push(
                AnomalyKind::Other,
                Some(at),
                format!("unknown bin status {status}"),
            );
        }
        let stop = on_entry(&entry);
        walk.entries.push(entry);
        if stop {
            return true;
        }
        // zcatfi6.f lines 186-194: next entry, or the bin is done past KBNSIZ-2.
        k += p + NLBIN;
        if k + 1 > bnsiz - 2 {
            break;
        }
    }
    false
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

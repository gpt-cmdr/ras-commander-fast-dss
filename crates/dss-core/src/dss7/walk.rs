//! DSS7 catalog walk, hash self-checks and anomaly detection.
//!
//! Sources (relative to `heclib/heclib_c/src/`):
//! - Block/bin scan order: `Internal/zcatalogInternal.c:zcatalogInternal()` lines 177–487
//!   (dss7.md §4.1). HEC stops at the first bin whose first word is 0 and otherwise
//!   follows the link word after the last bin of a block.
//! - Entry layout and end-of-bin rule: `Internal/zcheckInternal.c:zcheckInternal()`
//!   lines 277–420, `Internal/zbinUpdate.c:zbinUpdate()` (dss7.md §3.4).
//! - Overflow link (word `kbinSize-1`): `zcheckInternal()` line 426, `Internal/zbinNew.c`.
//! - Status filter: `Internal/zcompareRecordStatus.c:zcompareRecordStatus()` (§3.6).
//! - Julian first/last days only for types 100..199: `zcatalogInternal()` lines 392–398.
//!
//! Anomalies follow the private DSS corpus census (§6):
//! orphan bins, zeroed info blocks, duplicate live pathnames, record-count mismatch.

use super::info::{self, InfoBlock, InfoProblem};
use super::{
    bin, record_status, status, word, Dss7Error, Dss7File, BIN_ENTRY_WORDS, MAX_PATHNAME_BYTES,
};
use crate::catalog::{AnomalyKind, Catalog, CatalogAnomaly, CatalogEntry};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};

/// Which info blocks a catalog walk reads and verifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InfoCheck {
    /// Bins only (fast; what HEC's catalog does).
    #[default]
    None,
    /// Read the info block of every live entry.
    Live,
    /// Read every live info block when there are at most this many live entries;
    /// otherwise read an evenly spaced sample of this many.
    LiveUpTo(usize),
}

/// Options for [`Dss7File::catalog_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogOptions {
    /// Recompute pathname hashes and check hash-table reachability (orphan bins).
    pub check_hashes: bool,
    pub check_info: InfoCheck,
    /// Coalesce info-block reads that are closer than this many bytes.
    pub coalesce_gap_bytes: u64,
    /// Recovery mode. Also list entries in bins that hash-table slots (or their
    /// overflow links) reach but the bin-block chain does not. HEC's catalog
    /// (`zcatalogInternal()`) never lists these, but `zcheck()` lookups still find
    /// them; they are left by interrupted writes (dss7.md §6.4, P06 report §4).
    /// Recovered entries are appended after the bin-order entries and have
    /// [`Dss7EntryMeta::recovered`] set. Implies the hash-table scan even when
    /// `check_hashes` is false. Off by default for HEC catalog parity.
    pub recover_unlinked: bool,
}

impl Default for CatalogOptions {
    fn default() -> Self {
        CatalogOptions {
            check_hashes: true,
            check_info: InfoCheck::None,
            coalesce_gap_bytes: 64 * 1024,
            recover_unlinked: false,
        }
    }
}

/// How a live entry relates to the hash table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChainState {
    /// Hashes were not checked, or the entry is not live.
    #[default]
    NotChecked,
    /// Found in the chain of its own table hash at this hop (0 = first bin).
    OwnChain(u32),
    /// Its bin is reachable from some slot, but not from its own table hash.
    OtherSlot,
    /// Its bin is reachable from no hash-table slot (P02 "orphan bin").
    Orphan,
}

/// Info-block fields kept for a verified entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoSummary {
    pub problem: Option<InfoProblem>,
    pub status: i64,
    pub record_type: i32,
    pub last_write_ms: i64,
    pub first_date: (i32, i32),
    pub last_date: (i32, i32),
}

/// DSS7-specific facts for one catalog entry (parallel to `Catalog::entries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dss7EntryMeta {
    /// Raw bin status (i64 as stored).
    pub raw_status: i64,
    /// Word address of the bin that holds the entry.
    pub bin_address: i64,
    /// Word offset of the entry inside its bin.
    pub entry_offset: u32,
    /// Word address of the info block (`kbinInfoAdd`).
    pub info_address: i64,
    /// High half of `kbinTypeAndCatSort` (obsolete catalog sequence).
    pub catalog_sequence: i32,
    /// `kbinDates` julian first / last day, for types 100..199 only.
    pub julian_first: Option<i32>,
    pub julian_last: Option<i32>,
    /// Table hash computed for the pathname (when hashes were checked).
    pub table_hash: Option<i64>,
    pub chain: ChainState,
    /// For a duplicate live pathname, the index of the entry that lookup returns.
    pub duplicate_of: Option<usize>,
    pub info: Option<InfoSummary>,
    /// Listed only because of [`CatalogOptions::recover_unlinked`]: the entry's bin is
    /// outside the bin-block chain, so HEC's catalog does not list it.
    pub recovered: bool,
}

/// Counters from a walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkStats {
    pub blocks: u64,
    pub bins: u64,
    pub entries: u64,
    /// Status 0 or 1.
    pub live: u64,
    /// Status 0 (undeleted).
    pub undeleted: u64,
    pub aliases: u64,
    pub deleted: u64,
    pub renamed: u64,
    pub other_status: u64,
    pub hash_mismatch_live: u64,
    pub hash_mismatch_nonlive: u64,
    /// Live/alias entries whose bin is reachable from no table slot.
    pub orphan_entries: u64,
    /// Live/alias entries not in their own table-hash chain (orphan + other slot).
    pub table_chain_miss: u64,
    /// Bins reachable from no hash-table slot (any entries).
    pub orphan_bins: u64,
    pub duplicate_entries: u64,
    pub duplicate_pathnames: u64,
    pub info_checked: u64,
    pub info_bad: u64,
    pub info_zeroed: u64,
    /// Hash-table or overflow links to bins outside the walked blocks.
    pub dangling_links: u64,
    /// Of those, links to bins whose first word is 0 (allocated, never written).
    pub dangling_zeroed_bins: u64,
    /// Non-empty bins reachable from the hash table but outside the block chain.
    pub unlisted_bins: u64,
    /// Live entries in those bins: found by lookup, absent from the catalog (as in HEC).
    pub unlisted_live_entries: u64,
    /// Entries added by recovery mode (any status). Not included in `entries`,
    /// `live`, `aliases` or the other status counters, which describe HEC's catalog.
    pub recovered_entries: u64,
    /// Of those, live (status 0/1) entries.
    pub recovered_live: u64,
    pub structural_anomalies: u64,
}

/// A DSS7 catalog: the shared [`Catalog`] plus per-entry DSS7 facts and counters.
#[derive(Debug, Clone, Default)]
pub struct Dss7Catalog {
    pub catalog: Catalog,
    /// One element per `catalog.entries` element.
    pub meta: Vec<Dss7EntryMeta>,
    pub stats: WalkStats,
}

impl Dss7Catalog {
    /// Indices of live (status 0/1) and alias entries, keeping only the entry lookup
    /// would return for duplicated pathnames.
    pub fn resolved_live(&self) -> Vec<usize> {
        (0..self.catalog.entries.len())
            .filter(|&i| {
                let s = self.meta[i].raw_status;
                (s == status::VALID || s == status::PRIMARY || s == status::ALIAS)
                    && self.meta[i].duplicate_of.is_none()
            })
            .collect()
    }

    /// Index of the live or alias entry for `pathname` (case-insensitive) that is not a
    /// duplicate. Default catalogs return the copy HEC's lookup returns; recovery-mode
    /// catalogs return the best-preserved copy. Linear scan.
    pub fn find(&self, pathname: &[u8]) -> Option<usize> {
        (0..self.catalog.entries.len()).find(|&i| {
            is_live_or_alias(self.meta[i].raw_status)
                && self.meta[i].duplicate_of.is_none()
                && self.catalog.entries[i]
                    .pathname
                    .eq_ignore_ascii_case(pathname)
        })
    }
}

fn is_live_or_alias(s: i64) -> bool {
    s == status::VALID || s == status::PRIMARY || s == status::ALIAS
}

fn anomaly(cat: &mut Catalog, kind: AnomalyKind, word_address: Option<i64>, detail: String) {
    cat.anomalies.push(CatalogAnomaly {
        kind,
        address: word_address
            .and_then(|a| u64::try_from(a).ok())
            .map(|a| a.saturating_mul(8)),
        detail,
    });
}

pub(super) fn walk<R: Read + Seek>(
    f: &mut Dss7File<R>,
    opts: &CatalogOptions,
) -> Result<Dss7Catalog, Dss7Error> {
    let h = f.header().clone();
    let bs = h.bin_size as usize;
    let bpb = h.bins_per_block as usize;
    let block_words = h.block_words();
    let mut out = Dss7Catalog::default();
    let mut overflow: HashMap<i64, i64> = HashMap::new();
    let mut bin_order: Vec<i64> = Vec::new();

    // --- Block scan (zcatalogInternal()).
    let max_blocks = (h.file_size_words / block_words).saturating_add(1);
    let mut seen_blocks: HashSet<i64> = HashSet::new();
    let mut addr = h.first_bin_address;
    'blocks: while addr != 0 {
        if !seen_blocks.insert(addr) || out.stats.blocks as i64 >= max_blocks {
            anomaly(
                &mut out.catalog,
                AnomalyKind::CycleDetected,
                Some(addr),
                "bin block chain revisits a block".into(),
            );
            break;
        }
        // A block may be cut short only if the file is damaged; read what lies inside
        // the data area in whole bins.
        let avail = if f.in_data_area(addr, block_words) {
            block_words
        } else if f.in_data_area(addr, h.bin_size) {
            let a = (h.file_size_words - addr) / h.bin_size * h.bin_size;
            anomaly(
                &mut out.catalog,
                AnomalyKind::OutOfBounds,
                Some(addr),
                "bin block extends past end of file".into(),
            );
            a
        } else {
            anomaly(
                &mut out.catalog,
                AnomalyKind::OutOfBounds,
                Some(addr),
                "bin block address outside the data area".into(),
            );
            break;
        };
        let raw = f.read_words(addr, avail)?;
        out.stats.blocks += 1;
        let nbins = (avail as usize / bs).min(bpb);
        for j in 0..nbins {
            let base = j * bs;
            if word(&raw, base) == 0 {
                // zcatalogInternal() line 280: an empty bin ends the scan.
                break 'blocks;
            }
            let bin_addr = addr + base as i64;
            out.stats.bins += 1;
            overflow.insert(bin_addr, word(&raw, base + bs - 1));
            bin_order.push(bin_addr);
            walk_bin(&raw[base * 8..(base + bs) * 8], bin_addr, bs, &mut out);
        }
        if (avail as usize) < bs * bpb + 1 {
            break;
        }
        addr = word(&raw, bpb * bs);
    }

    // --- Status counters and record-count check.
    for m in &out.meta {
        match m.raw_status {
            status::VALID => {
                out.stats.live += 1;
                out.stats.undeleted += 1;
            }
            status::PRIMARY => out.stats.live += 1,
            status::ALIAS => out.stats.aliases += 1,
            status::DELETED | status::ALIAS_DELETED | status::REMOVED => out.stats.deleted += 1,
            status::RENAMED => out.stats.renamed += 1,
            _ => out.stats.other_status += 1,
        }
    }
    let unknown: Vec<(i64, i64)> = out
        .meta
        .iter()
        .filter(|m| !matches!(m.raw_status, 0 | 1 | 2 | 10 | 11 | 12 | 13 | 15))
        .map(|m| (m.bin_address + i64::from(m.entry_offset), m.raw_status))
        .collect();
    for (a, s) in unknown {
        anomaly(
            &mut out.catalog,
            AnomalyKind::Other,
            Some(a),
            format!("unknown bin status {s}"),
        );
    }
    if out.stats.live as i64 != h.number_records {
        anomaly(
            &mut out.catalog,
            AnomalyKind::RecordCountMismatch,
            None,
            format!(
                "walked {} live entries, header knumberRecords = {}",
                out.stats.live, h.number_records
            ),
        );
    }
    if out.stats.aliases as i64 != h.number_aliases {
        anomaly(
            &mut out.catalog,
            AnomalyKind::RecordCountMismatch,
            None,
            format!(
                "walked {} aliases, header knumberAliases = {}",
                out.stats.aliases, h.number_aliases
            ),
        );
    }

    if opts.check_hashes || opts.recover_unlinked {
        check_hashes(f, &mut out, overflow, &bin_order, opts.recover_unlinked)?;
    }
    match opts.check_info {
        InfoCheck::None => {}
        InfoCheck::Live => check_info(f, &mut out, usize::MAX, opts.coalesce_gap_bytes)?,
        InfoCheck::LiveUpTo(n) => check_info(f, &mut out, n, opts.coalesce_gap_bytes)?,
    }
    if opts.recover_unlinked {
        // Recovery ranks duplicate copies by their info blocks, so every copy of a
        // duplicated pathname needs one.
        let missing: Vec<usize> = duplicate_groups(&out)
            .into_iter()
            .flatten()
            .filter(|&i| out.meta[i].info.is_none())
            .collect();
        check_info_indices(f, &mut out, missing, opts.coalesce_gap_bytes)?;
    }
    find_duplicates(&mut out, opts.recover_unlinked);
    out.stats.structural_anomalies = out
        .catalog
        .anomalies
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                AnomalyKind::OutOfBounds | AnomalyKind::CycleDetected
            )
        })
        .count() as u64;
    Ok(out)
}

/// Walk the entries of one bin (`zcheckInternal()` / `zcatalogInternal()` inner loop).
fn walk_bin(b: &[u8], bin_addr: i64, bs: usize, out: &mut Dss7Catalog) {
    let mut ic = 0usize;
    // An entry needs 7 words plus its pathname before the overflow word (bs-1).
    while ic + (BIN_ENTRY_WORDS as usize) < bs {
        let eh = word(b, ic + bin::HASH);
        if eh == 0 {
            return;
        }
        let (chars, pwords) = info::split_i4(word(b, ic + bin::PATH_LEN));
        if chars <= 0
            || chars as usize > MAX_PATHNAME_BYTES
            || i64::from(pwords) != (i64::from(chars) + 7) / 8
            || ic + BIN_ENTRY_WORDS as usize + pwords as usize > bs - 1
        {
            anomaly(
                &mut out.catalog,
                AnomalyKind::OutOfBounds,
                Some(bin_addr + ic as i64),
                format!(
                    "bad bin entry pathname length {chars} / {pwords} words; rest of bin skipped"
                ),
            );
            return;
        }
        let po = (ic + bin::PATH) * 8;
        let pathname = b[po..po + chars as usize].to_vec();
        let raw_status = word(b, ic + bin::STATUS);
        let info_address = word(b, ic + bin::INFO_ADD);
        let (record_type, catalog_sequence) = info::split_i4(word(b, ic + bin::TYPE_AND_CAT_SORT));
        let last_write = word(b, ic + bin::LAST_WRITE);
        let (jf, jl) = info::split_i4(word(b, ic + bin::DATES));
        let ts = (100..200).contains(&record_type);
        out.catalog.entries.push(CatalogEntry {
            pathname,
            record_type,
            status: record_status(raw_status),
            raw_status: i32::try_from(raw_status).unwrap_or(if raw_status < 0 {
                i32::MIN
            } else {
                i32::MAX
            }),
            stored_hash: eh,
            info_address: u64::try_from(info_address)
                .map(|a| a.saturating_mul(8))
                .unwrap_or(0),
            bin_address: ((bin_addr + ic as i64) as u64).saturating_mul(8),
            last_write_ms: Some(last_write),
        });
        out.meta.push(Dss7EntryMeta {
            raw_status,
            bin_address: bin_addr,
            entry_offset: ic as u32,
            info_address,
            catalog_sequence,
            julian_first: ts.then_some(jf),
            julian_last: ts.then_some(jl),
            ..Dss7EntryMeta::default()
        });
        out.stats.entries += 1;
        ic += BIN_ENTRY_WORDS as usize + pwords as usize;
        if ic >= bs - 2 {
            return;
        }
    }
}

/// Recompute pathname hashes (§3.1), check table-chain membership (§3.2/§3.3) and find
/// bins that no hash-table slot reaches (P02 anomaly 1).
fn check_hashes<R: Read + Seek>(
    f: &mut Dss7File<R>,
    out: &mut Dss7Catalog,
    mut overflow: HashMap<i64, i64>,
    bin_order: &[i64],
    recover: bool,
) -> Result<(), Dss7Error> {
    let h = f.header().clone();
    let table_bytes = f.read_words(h.hash_table_address, h.max_hash)?;
    let table: Vec<i64> = (0..h.max_hash as usize)
        .map(|i| word(&table_bytes, i))
        .collect();

    // Reachable bins: closure of every non-zero slot over the overflow links. Chains
    // are short, so each chain keeps its own visited list to tell a loop from two
    // chains that merge.
    let mut bound = overflow.len() + 1;
    let mut reachable: HashSet<i64> = HashSet::with_capacity(overflow.len());
    let mut dangling: Vec<i64> = Vec::new();
    let (mut cycles, mut merges) = (0u64, 0u64);
    let mut chain: Vec<i64> = Vec::new();
    for &slot in &table {
        let mut a = slot;
        chain.clear();
        while a != 0 {
            let Some(&next) = overflow.get(&a) else {
                dangling.push(a);
                break;
            };
            if chain.contains(&a) || chain.len() > bound {
                cycles += 1;
                break;
            }
            if !reachable.insert(a) {
                merges += 1;
                break;
            }
            chain.push(a);
            a = next;
        }
    }
    if !dangling.is_empty() {
        // Classify the targets. Allocated-but-never-written (all-zero) bins are the
        // signature of an interrupted HEC write (dss7.md §6.4). Non-zero bins outside the
        // block chain hold entries that a hash lookup reaches but HEC's bin-walk catalog
        // (zcatalogInternal()) never lists; count their live entries, following their own
        // overflow links (bounded, each bin visited once).
        let max_bins = (h.file_size_words / h.bin_size).saturating_add(1) as usize;
        let mut seen: HashSet<i64> = HashSet::new();
        let (mut zeroed, mut unlisted_bins, mut unlisted_live) = (0u64, 0u64, 0u64);
        let mut recovered_bins: Vec<(i64, i64)> = Vec::new();
        let mut stack: Vec<(i64, bool)> = dangling.iter().map(|&a| (a, true)).collect();
        while let Some((a, from_table)) = stack.pop() {
            if seen.len() >= max_bins || overflow.contains_key(&a) || !seen.insert(a) {
                continue;
            }
            if !f.in_data_area(a, h.bin_size) {
                continue;
            }
            let Ok(b) = f.read_words(a, h.bin_size) else {
                continue;
            };
            if word(&b, 0) == 0 {
                if from_table {
                    zeroed += 1;
                }
                continue;
            }
            unlisted_bins += 1;
            unlisted_live += count_live_entries(&b, bs_of(&h));
            let next = word(&b, bs_of(&h) - 1);
            if recover {
                // List the bin's entries and make it part of the chain map, so the
                // chain check below sees recovered entries as reachable.
                let first = out.meta.len();
                walk_bin(&b, a, bs_of(&h), out);
                let added = (out.meta.len() - first) as u64;
                out.stats.entries -= added;
                out.stats.recovered_entries += added;
                for m in &mut out.meta[first..] {
                    m.recovered = true;
                    if matches!(m.raw_status, status::VALID | status::PRIMARY) {
                        out.stats.recovered_live += 1;
                    }
                }
                recovered_bins.push((a, next));
            }
            if next != 0 {
                stack.push((next, false));
            }
        }
        for (a, next) in recovered_bins {
            overflow.insert(a, next);
            reachable.insert(a);
        }
        bound = overflow.len() + 1;
        out.stats.dangling_links = dangling.len() as u64;
        out.stats.dangling_zeroed_bins = zeroed;
        out.stats.unlisted_bins = unlisted_bins;
        out.stats.unlisted_live_entries = unlisted_live;
        anomaly(
            &mut out.catalog,
            AnomalyKind::OutOfBounds,
            dangling.first().copied(),
            format!(
                "{} hash-table or overflow links point outside the walked bins ({zeroed} to empty bins; {unlisted_bins} unlinked bins hold {unlisted_live} live entries that lookup can reach but the catalog does not list{})",
                dangling.len(),
                if recover { "; listed by recovery mode" } else { "" }
            ),
        );
    }
    if cycles > 0 {
        anomaly(
            &mut out.catalog,
            AnomalyKind::CycleDetected,
            None,
            format!("{cycles} overflow chains loop"),
        );
    }
    if merges > 0 {
        anomaly(
            &mut out.catalog,
            AnomalyKind::Other,
            None,
            format!("{merges} hash chains share bins"),
        );
    }
    out.stats.orphan_bins = bin_order.iter().filter(|a| !reachable.contains(a)).count() as u64;

    for i in 0..out.catalog.entries.len() {
        let e = &out.catalog.entries[i];
        let m = out.meta[i];
        let calc = super::pathname_hash(&e.pathname);
        let live = is_live_or_alias(m.raw_status);
        let entry_addr = m.bin_address + i64::from(m.entry_offset);
        let stored = e.stored_hash;
        if calc != stored {
            if live {
                out.stats.hash_mismatch_live += 1;
                anomaly(
                    &mut out.catalog,
                    AnomalyKind::HashMismatch,
                    Some(entry_addr),
                    format!("stored hash {stored} != computed {calc}"),
                );
            } else {
                out.stats.hash_mismatch_nonlive += 1;
            }
        }
        if !live {
            continue;
        }
        let th = super::table_hash(&out.catalog.entries[i].pathname, h.max_hash);
        out.meta[i].table_hash = Some(th);
        let mut a = table.get(th as usize).copied().unwrap_or(0);
        let mut hop = 0u32;
        let mut found = None;
        while a != 0 && (hop as usize) <= bound {
            if a == m.bin_address {
                found = Some(hop);
                break;
            }
            a = overflow.get(&a).copied().unwrap_or(0);
            hop += 1;
        }
        out.meta[i].chain = match found {
            Some(hop) => ChainState::OwnChain(hop),
            None => {
                out.stats.table_chain_miss += 1;
                if reachable.contains(&m.bin_address) {
                    anomaly(
                        &mut out.catalog,
                        AnomalyKind::Other,
                        Some(entry_addr),
                        format!("live entry not in the chain of its table hash {th}"),
                    );
                    ChainState::OtherSlot
                } else {
                    out.stats.orphan_entries += 1;
                    anomaly(
                        &mut out.catalog,
                        AnomalyKind::OrphanBin,
                        Some(entry_addr),
                        "live entry in a bin no hash-table slot reaches".into(),
                    );
                    ChainState::Orphan
                }
            }
        };
    }
    Ok(())
}

fn bs_of(h: &super::Dss7Header) -> usize {
    h.bin_size as usize
}

/// Live (status 0/1/2) entries in one bin, using the same entry rules as [`walk_bin`].
fn count_live_entries(b: &[u8], bs: usize) -> u64 {
    let mut ic = 0usize;
    let mut n = 0u64;
    while ic + (BIN_ENTRY_WORDS as usize) < bs {
        if word(b, ic + bin::HASH) == 0 {
            break;
        }
        let (chars, pwords) = info::split_i4(word(b, ic + bin::PATH_LEN));
        if chars <= 0
            || chars as usize > MAX_PATHNAME_BYTES
            || i64::from(pwords) != (i64::from(chars) + 7) / 8
            || ic + BIN_ENTRY_WORDS as usize + pwords as usize > bs - 1
        {
            break;
        }
        if is_live_or_alias(word(b, ic + bin::STATUS)) {
            n += 1;
        }
        ic += BIN_ENTRY_WORDS as usize + pwords as usize;
        if ic >= bs - 2 {
            break;
        }
    }
    n
}

/// Groups of live/alias entries that share a pathname (case-insensitive), each sorted.
fn duplicate_groups(out: &Dss7Catalog) -> Vec<Vec<usize>> {
    let mut groups: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, e) in out.catalog.entries.iter().enumerate() {
        if is_live_or_alias(out.meta[i].raw_status) {
            groups
                .entry(e.pathname.to_ascii_uppercase())
                .or_default()
                .push(i);
        }
    }
    let mut dups: Vec<Vec<usize>> = groups.into_values().filter(|v| v.len() > 1).collect();
    dups.sort_unstable();
    dups
}

/// Mark duplicate live pathnames (case-insensitive).
///
/// By default the entry that lookup returns wins (lowest hop in its own chain, then bin
/// order), as in HEC. In recovery mode the best-preserved copy wins instead: a valid
/// info block first, then the latest info-block write time, then the lookup order. In
/// the P06 interrupted-write file the chain-first copy of every duplicated pathname has
/// a zeroed or invalid info block and the recovered copy is newer and valid.
fn find_duplicates(out: &mut Dss7Catalog, recover: bool) {
    for g in duplicate_groups(out) {
        let rank = |i: usize| -> (bool, std::cmp::Reverse<i64>, u32, usize) {
            let m = &out.meta[i];
            let hop = match m.chain {
                ChainState::OwnChain(hop) => hop,
                _ => u32::MAX,
            };
            if !recover {
                return (false, std::cmp::Reverse(0), hop, i);
            }
            let (bad, written) = match m.info {
                Some(s) if s.problem.is_none() => (false, s.last_write_ms),
                _ => (true, i64::MIN),
            };
            (bad, std::cmp::Reverse(written), hop, i)
        };
        let winner = g.iter().copied().min_by_key(|&i| rank(i)).unwrap_or(g[0]);
        out.stats.duplicate_pathnames += 1;
        for &i in &g {
            if i == winner {
                continue;
            }
            out.meta[i].duplicate_of = Some(winner);
            out.stats.duplicate_entries += 1;
            let m = out.meta[i];
            anomaly(
                &mut out.catalog,
                AnomalyKind::DuplicatePathname,
                Some(m.bin_address + i64::from(m.entry_offset)),
                if recover {
                    format!("duplicate live pathname; catalog entry {winner} is the best-preserved copy")
                } else {
                    format!(
                        "duplicate live pathname; catalog entry {winner} is the one lookup returns"
                    )
                },
            );
        }
    }
}

/// Read and verify live info blocks with coalesced positioned reads.
fn check_info<R: Read + Seek>(
    f: &mut Dss7File<R>,
    out: &mut Dss7Catalog,
    limit: usize,
    gap: u64,
) -> Result<(), Dss7Error> {
    let mut idx: Vec<usize> = (0..out.meta.len())
        .filter(|&i| matches!(out.meta[i].raw_status, status::VALID | status::PRIMARY))
        .collect();
    if idx.len() > limit {
        let n = idx.len();
        idx = (0..limit).map(|k| idx[k * n / limit]).collect();
    }
    check_info_indices(f, out, idx, gap)
}

/// Read and verify the info blocks of the given entries.
fn check_info_indices<R: Read + Seek>(
    f: &mut Dss7File<R>,
    out: &mut Dss7Catalog,
    idx: Vec<usize>,
    gap: u64,
) -> Result<(), Dss7Error> {
    let span = |i: usize, e: &CatalogEntry| -> (i64, i64) {
        let words = info::INFO_FIXED_WORDS as i64 + (e.pathname.len() as i64 + 7) / 8;
        (out.meta[i].info_address, words)
    };
    let mut items: Vec<(i64, i64, usize)> = idx
        .iter()
        .map(|&i| {
            let (a, w) = span(i, &out.catalog.entries[i]);
            (a, w, i)
        })
        .collect();
    items.sort_unstable();
    let gap_words = (gap / 8) as i64;
    const MAX_CHUNK_WORDS: i64 = 1 << 20; // 8 MiB per read
    let mut k = 0usize;
    while k < items.len() {
        let (a0, w0, i0) = items[k];
        if !f.in_data_area(a0, w0) {
            record_info(out, i0, None);
            k += 1;
            continue;
        }
        // Grow the chunk while the next block starts within `gap` of the current end.
        let mut end = a0 + w0;
        let mut j = k + 1;
        while j < items.len() {
            let (a, w, _) = items[j];
            if a < a0
                || a > end.saturating_add(gap_words)
                || a + w - a0 > MAX_CHUNK_WORDS
                || !f.in_data_area(a, w)
            {
                break;
            }
            end = end.max(a + w);
            j += 1;
        }
        let chunk = f.read_words(a0, end - a0)?;
        for &(a, w, i) in &items[k..j] {
            let off = ((a - a0) * 8) as usize;
            let blk = InfoBlock::decode(a, &chunk[off..off + (w * 8) as usize]);
            record_info(out, i, blk);
        }
        k = j;
    }
    Ok(())
}

fn record_info(out: &mut Dss7Catalog, i: usize, blk: Option<InfoBlock>) {
    out.stats.info_checked += 1;
    let e = &out.catalog.entries[i];
    let (summary, problem) = match blk {
        None => (
            InfoSummary {
                problem: Some(InfoProblem::OutOfBounds),
                status: 0,
                record_type: 0,
                last_write_ms: 0,
                first_date: (0, 0),
                last_date: (0, 0),
            },
            Some(InfoProblem::OutOfBounds),
        ),
        Some(b) => {
            let p = b.mismatch(e.stored_hash, &e.pathname);
            (
                InfoSummary {
                    problem: p,
                    status: b.status,
                    record_type: b.record_type,
                    last_write_ms: b.last_write_ms,
                    first_date: b.first_date,
                    last_date: b.last_date,
                },
                p,
            )
        }
    };
    out.meta[i].info = Some(summary);
    if let Some(p) = problem {
        out.stats.info_bad += 1;
        if p == InfoProblem::Zeroed {
            out.stats.info_zeroed += 1;
        }
        let a = out.meta[i].info_address;
        anomaly(
            &mut out.catalog,
            AnomalyKind::BadInfoBlock,
            Some(a),
            p.to_string(),
        );
    }
}

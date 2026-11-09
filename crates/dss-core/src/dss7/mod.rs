//! DSS7 reader: header, catalog and point lookup; time-series reads
//! ([`ts`]).
//!
//! Format rules: `docs/format/dss7.md`. Each submodule cites the `heclib` function
//! its layout comes from.
//!
//! The reader is strictly read-only: it opens files with [`std::fs::File::open`], takes
//! no locks, never writes, and never reads the lock area (words `100..lock_area_end`,
//! dss7.md §6.3). All structure walks are bounded and report problems as
//! [`crate::catalog::CatalogAnomaly`] values instead of failing; only a bad header or an
//! I/O error is fatal.
//!
//! ```no_run
//! use dss_core::dss7::Dss7File;
//! let mut f = Dss7File::open("sample7.dss")?;
//! let cat = f.catalog()?;
//! for e in cat.catalog.live() {
//!     println!("{}", e.pathname_lossy());
//! }
//! let rec = f.lookup(b"//SACRAMENTO/PRECIP-INC/01JAN1877/1DAY/OBS/")?;
//! # Ok::<(), dss_core::dss7::Dss7Error>(())
//! ```

mod compress;
pub mod grid;
pub mod hash;
pub mod header;
pub mod info;
pub mod interval;
pub mod pd;
// Location records.
pub mod location;
pub mod ts;
mod walk;
// Per-catalog dataset date-range index.
mod range_index;
// Regular time-series writer (create, add, replace). Not built for wasm32.
#[cfg(not(target_arch = "wasm32"))]
pub mod write;

use std::collections::HashSet;
use std::fmt;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

pub use hash::{pathname_hash, table_hash};
pub use header::Dss7Header;
pub use info::{InfoBlock, InfoProblem};
pub use location::{Dss7Location, LocationError};
pub use pd::{Dss7Pd, Dss7PdMeta, PdError};
pub use range_index::IndexedDss7Catalog;
pub use ts::{Dss7Profile, Dss7Ts, TsError, TsInternalHeader, TsMeta, TsReadOptions, TsWindow};
pub use walk::{
    CatalogOptions, ChainState, Dss7Catalog, Dss7EntryMeta, InfoCheck, InfoSummary, WalkStats,
};

/// `MAX_PATHNAME_LENGTH` (`headers/zdssVals.h`): longest pathname in bytes.
pub const MAX_PATHNAME_BYTES: usize = 393;
/// `zdssBinKeys.kbinSize`: fixed words of a bin entry before the pathname (dss7.md §3.4).
pub const BIN_ENTRY_WORDS: i64 = 7;

/// Bin entry word offsets (`zdssBinKeys`, `Internal/zinit.c:zinit()`; dss7.md §3.4).
pub(crate) mod bin {
    pub const HASH: usize = 0;
    pub const STATUS: usize = 1;
    pub const PATH_LEN: usize = 2;
    pub const INFO_ADD: usize = 3;
    pub const TYPE_AND_CAT_SORT: usize = 4;
    pub const LAST_WRITE: usize = 5;
    pub const DATES: usize = 6;
    pub const PATH: usize = 7;
}

/// Record status codes (`headers/zdssVals.h`; dss7.md §3.6).
pub mod status {
    pub const VALID: i64 = 0;
    pub const PRIMARY: i64 = 1;
    pub const ALIAS: i64 = 2;
    pub const MOVED: i64 = 10;
    pub const DELETED: i64 = 11;
    pub const RENAMED: i64 = 12;
    pub const ALIAS_DELETED: i64 = 13;
    pub const REMOVED: i64 = 15;
}

/// Errors that stop a DSS7 open or read. Structural damage found during a catalog walk
/// is reported as anomalies instead.
#[derive(Debug)]
pub enum Dss7Error {
    Io(std::io::Error),
    /// Fewer bytes than the 800-byte permanent section.
    TooShort {
        len: u64,
    },
    /// Bytes 0..4 are not `ZDSS`.
    BadSignature,
    /// Version string does not start with `7`.
    NotVersion7 {
        version: [u8; 4],
    },
    /// Word 99 is not −97531 (`zpermRead()` INVALID_FILE_HEADER).
    BadEndHeaderFlag(i64),
    /// Word 89 (`kdetune`) is non-zero: a test-build file whose hashes are fixed.
    DetuneSet(i64),
    /// `kbinSize` outside 20..1000.
    BadBinSize(i64),
    /// `kbinsPerBlock` outside 1..1024.
    BadBinsPerBlock(i64),
    /// `kmaxHash` outside 1..1,000,000.
    BadMaxHash(i64),
    /// `kfileHeaderSize` outside 100..kfileSize (0 is accepted as 100).
    BadHeaderSize(i64),
    /// A header pointer outside `[lock_area_end, kfileSize)`.
    PointerOutOfRange {
        field: &'static str,
        value: i64,
    },
    /// The header's end-of-file word lies beyond the file length.
    Truncated {
        header_bytes: u64,
        file_len: u64,
    },
    /// A read was requested outside the file data or inside the lock area.
    ReadOutOfBounds {
        address: i64,
        words: i64,
    },
    /// The record's bin entry exists but its info block is not valid.
    BadInfoBlock {
        info_address: i64,
        problem: InfoProblem,
    },
    /// Pathname argument is empty or longer than 393 bytes.
    BadPathname,
    /// [`Dss7File::record_at`]: the index is past the catalog, or the entry is not a
    /// live or alias entry.
    NotLiveEntry {
        index: usize,
        status: Option<i64>,
    },
}

impl fmt::Display for Dss7Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dss7Error::Io(e) => write!(f, "I/O error: {e}"),
            Dss7Error::TooShort { len } => {
                write!(f, "file too short for a DSS7 header ({len} bytes)")
            }
            Dss7Error::BadSignature => write!(f, "not a DSS file (missing ZDSS signature)"),
            Dss7Error::NotVersion7 { version } => {
                write!(
                    f,
                    "not a DSS7 file (version {:?})",
                    String::from_utf8_lossy(version)
                )
            }
            Dss7Error::BadEndHeaderFlag(v) => {
                write!(f, "invalid end-of-header flag {v} (expected -97531)")
            }
            Dss7Error::DetuneSet(v) => {
                write!(f, "kdetune is {v}: test-only file with fixed hashes")
            }
            Dss7Error::BadBinSize(v) => write!(f, "invalid bin size {v} (expected 20..1000)"),
            Dss7Error::BadBinsPerBlock(v) => {
                write!(f, "invalid bins per block {v} (expected 1..1024)")
            }
            Dss7Error::BadMaxHash(v) => write!(f, "invalid hash table size {v}"),
            Dss7Error::BadHeaderSize(v) => write!(f, "invalid header size {v}"),
            Dss7Error::PointerOutOfRange { field, value } => {
                write!(f, "header pointer {field} = {value} out of range")
            }
            Dss7Error::Truncated {
                header_bytes,
                file_len,
            } => {
                write!(
                    f,
                    "file truncated: header needs {header_bytes} bytes, file has {file_len}"
                )
            }
            Dss7Error::ReadOutOfBounds { address, words } => {
                write!(
                    f,
                    "read of {words} words at word {address} is out of bounds"
                )
            }
            Dss7Error::BadInfoBlock {
                info_address,
                problem,
            } => {
                write!(f, "record info block at word {info_address}: {problem}")
            }
            Dss7Error::BadPathname => write!(f, "pathname must be 1..393 bytes"),
            Dss7Error::NotLiveEntry { index, status } => match status {
                Some(s) => write!(f, "catalog entry {index} has status {s}, not live or alias"),
                None => write!(f, "catalog entry {index} does not exist"),
            },
        }
    }
}

impl std::error::Error for Dss7Error {}

impl From<std::io::Error> for Dss7Error {
    fn from(e: std::io::Error) -> Self {
        Dss7Error::Io(e)
    }
}

/// Result of a point lookup (dss7.md §3.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef {
    /// Pathname bytes as stored in the bin.
    pub pathname: Vec<u8>,
    /// Bin status of the matched entry (0, 1, or 2 for an alias).
    pub status: i64,
    /// Record type from the bin entry.
    pub record_type: i32,
    /// Word address of the matched bin entry.
    pub bin_entry_address: i64,
    /// Word address of the record's info block (after alias resolution).
    pub info_address: i64,
    pub info: InfoBlock,
}

/// A read-only DSS7 file.
pub struct Dss7File<R> {
    src: R,
    len: u64,
    header: Dss7Header,
}

impl Dss7File<std::fs::File> {
    /// Open a file read-only. No locks are taken and nothing is written.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Dss7Error> {
        Self::new(std::fs::File::open(path)?)
    }
}

impl<'a> Dss7File<Cursor<&'a [u8]>> {
    /// Read from an in-memory copy of the whole file (used by the wasm binding).
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, Dss7Error> {
        Self::new(Cursor::new(bytes))
    }
}

impl<R: Read + Seek> Dss7File<R> {
    /// Wrap any seekable byte source and validate its header.
    pub fn new(mut src: R) -> Result<Self, Dss7Error> {
        let len = src.seek(SeekFrom::End(0))?;
        src.seek(SeekFrom::Start(0))?;
        let want = header::HEADER_BYTES.min(usize::try_from(len).unwrap_or(usize::MAX));
        let mut head = vec![0u8; want];
        src.read_exact(&mut head)?;
        let header = Dss7Header::parse(&head, len)?;
        Ok(Dss7File { src, len, header })
    }

    pub fn header(&self) -> &Dss7Header {
        &self.header
    }

    /// File length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Give back the underlying source.
    pub fn into_inner(self) -> R {
        self.src
    }

    /// True when `[address, address + words)` lies in the readable data area: after the
    /// lock area and before the end-of-file flag word.
    pub(crate) fn in_data_area(&self, address: i64, words: i64) -> bool {
        words >= 0
            && address >= self.header.lock_area_end
            && address
                .checked_add(words)
                .is_some_and(|end| end <= self.header.file_size_words)
    }

    /// Read `words` 8-byte words at word `address`, as raw bytes.
    pub(crate) fn read_words(&mut self, address: i64, words: i64) -> Result<Vec<u8>, Dss7Error> {
        if !self.in_data_area(address, words) {
            return Err(Dss7Error::ReadOutOfBounds { address, words });
        }
        let off = (address as u64) * 8;
        let n = usize::try_from(words as u64 * 8)
            .map_err(|_| Dss7Error::ReadOutOfBounds { address, words })?;
        let mut buf = vec![0u8; n];
        self.src.seek(SeekFrom::Start(off))?;
        self.src.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read and decode the info block at word `info_address` for a pathname of
    /// `path_len` bytes (`Internal/zreadInfo.c:zreadInfo()`: `30 + ceil(n/8)` words).
    pub fn read_info(
        &mut self,
        info_address: i64,
        path_len: usize,
    ) -> Result<InfoBlock, Dss7Error> {
        let path_words = path_len.min(MAX_PATHNAME_BYTES).div_ceil(8) as i64;
        let words = info::INFO_FIXED_WORDS as i64 + path_words;
        let bytes = self.read_words(info_address, words)?;
        InfoBlock::decode(info_address, &bytes).ok_or(Dss7Error::ReadOutOfBounds {
            address: info_address,
            words,
        })
    }

    /// Full catalog walk with default options (hash checks on, info blocks not read).
    pub fn catalog(&mut self) -> Result<Dss7Catalog, Dss7Error> {
        self.catalog_with(&CatalogOptions::default())
    }

    /// Catalog walk in physical bin order (`Internal/zcatalogInternal.c:zcatalogInternal()`;
    /// dss7.md §4.1). Returns every bin entry (live, alias, deleted, renamed) and the
    /// anomalies found.
    pub fn catalog_with(&mut self, opts: &CatalogOptions) -> Result<Dss7Catalog, Dss7Error> {
        walk::walk(self, opts)
    }

    /// Case-insensitive point lookup through the hash table (dss7.md §3.7,
    /// `Internal/zcheckInternal.c:zcheckInternal()`).
    ///
    /// Returns `Ok(None)` when no live entry is reachable for the pathname (this includes
    /// entries that sit in orphan bins, which HEC cannot read either), and
    /// `Err(BadInfoBlock)` when the bin entry exists but its info block is invalid.
    /// Open the record behind one catalog entry, applying the same checks as
    /// [`lookup`](Self::lookup) (alias resolution, info flag, info hash).
    ///
    /// Use this with a recovery-mode catalog ([`CatalogOptions::recover_unlinked`]):
    /// when a pathname is listed more than once, `lookup` follows HEC and returns the
    /// first copy in its hash chain, which in interrupted-write files can be the
    /// damaged one. [`Dss7Catalog::find`] returns the best-preserved copy's index.
    pub fn record_at(&mut self, cat: &Dss7Catalog, index: usize) -> Result<RecordRef, Dss7Error> {
        let (Some(e), Some(m)) = (cat.catalog.entries.get(index), cat.meta.get(index)) else {
            return Err(Dss7Error::NotLiveEntry {
                index,
                status: None,
            });
        };
        let st = m.raw_status;
        if !((status::VALID..=status::PRIMARY).contains(&st) || st == status::ALIAS) {
            return Err(Dss7Error::NotLiveEntry {
                index,
                status: Some(st),
            });
        }
        let mut info_addr = m.info_address;
        if st == status::ALIAS {
            info_addr = self.resolve_alias(info_addr)?;
        }
        let bad = |problem| Dss7Error::BadInfoBlock {
            info_address: info_addr,
            problem,
        };
        let info = self
            .read_info(info_addr, e.pathname.len())
            .map_err(|err| match err {
                Dss7Error::ReadOutOfBounds { .. } => bad(InfoProblem::OutOfBounds),
                other => other,
            })?;
        if info.flag != info::INFO_FLAG {
            return Err(bad(if info.flag == 0 {
                InfoProblem::Zeroed
            } else {
                InfoProblem::BadFlag(info.flag)
            }));
        }
        // An alias's info block belongs to the primary; read its own pathname.
        let info = if st == status::ALIAS {
            self.primary_info(info)
        } else {
            info
        };
        // Same rule as lookup(): an info hash that differs from the entry's hash means
        // the info space was reused (zreadInfo()); aliases are exempt.
        if st != status::ALIAS && info.hash != e.stored_hash {
            return Err(bad(InfoProblem::HashMismatch));
        }
        Ok(RecordRef {
            pathname: e.pathname.clone(),
            status: st,
            record_type: e.record_type,
            bin_entry_address: m.bin_address + i64::from(m.entry_offset),
            info_address: info_addr,
            info,
        })
    }

    pub fn lookup(&mut self, pathname: &[u8]) -> Result<Option<RecordRef>, Dss7Error> {
        if pathname.is_empty() || pathname.len() > MAX_PATHNAME_BYTES {
            return Err(Dss7Error::BadPathname);
        }
        let h = self.header.clone();
        let want = &pathname[..pathname.len().min(hash::MAX_HASHED_PATH)];
        let bin_hash = pathname_hash(want);
        let th = table_hash(want, h.max_hash);
        let slot = self.read_words(h.hash_table_address + th, 1)?;
        let mut bin_addr = word(&slot, 0);
        let max_hops = (h.file_size_words / h.bin_size).saturating_add(1);
        let mut hops = 0i64;
        while bin_addr != 0 {
            hops += 1;
            if hops > max_hops || !self.in_data_area(bin_addr, h.bin_size) {
                return Ok(None);
            }
            let bytes = self.read_words(bin_addr, h.bin_size)?;
            let b = |i: usize| word(&bytes, i);
            let bs = h.bin_size as usize;
            let mut ic = 0usize;
            // Entry walk: zcheckInternal() lines 312–420.
            while ic + (BIN_ENTRY_WORDS as usize) < bs {
                let eh = b(ic + bin::HASH);
                if eh == 0 {
                    break;
                }
                let (chars, pwords) = info::split_i4(b(ic + bin::PATH_LEN));
                if chars <= 0
                    || chars as usize > MAX_PATHNAME_BYTES
                    || pwords as i64 != (chars as i64 + 7) / 8
                    || ic + BIN_ENTRY_WORDS as usize + pwords as usize > bs - 1
                {
                    break;
                }
                let st = b(ic + bin::STATUS);
                if eh == bin_hash && chars as usize == pathname.len() {
                    let po = (ic + bin::PATH) * 8;
                    let stored = &bytes[po..po + chars as usize];
                    // boolDoubleCheckPath: case-insensitive compare (zpathnameCompare.c).
                    if stored.eq_ignore_ascii_case(pathname)
                        && ((status::VALID..=status::PRIMARY).contains(&st) || st == status::ALIAS)
                    {
                        let entry_addr = bin_addr + ic as i64;
                        let (rtype, _) = info::split_i4(b(ic + bin::TYPE_AND_CAT_SORT));
                        let mut info_addr = b(ic + bin::INFO_ADD);
                        if st == status::ALIAS {
                            info_addr = self.resolve_alias(info_addr)?;
                        }
                        let info =
                            self.read_info(info_addr, chars as usize)
                                .map_err(|e| match e {
                                    Dss7Error::ReadOutOfBounds { .. } => Dss7Error::BadInfoBlock {
                                        info_address: info_addr,
                                        problem: InfoProblem::OutOfBounds,
                                    },
                                    other => other,
                                })?;
                        if info.flag != info::INFO_FLAG {
                            let problem = if info.flag == 0 {
                                InfoProblem::Zeroed
                            } else {
                                InfoProblem::BadFlag(info.flag)
                            };
                            return Err(Dss7Error::BadInfoBlock {
                                info_address: info_addr,
                                problem,
                            });
                        }
                        // An alias's info block belongs to the primary.
                        let info = if st == status::ALIAS {
                            self.primary_info(info)
                        } else {
                            info
                        };
                        // zreadInfo(): an info hash that differs from the pathname hash means
                        // the space was reused; the record is not found. For an alias the
                        // info block belongs to the primary, whose hash differs from the
                        // alias hash; zreadInfo() as written would reject every alias read.
                        // We skip the hash check for aliases ([UNVERIFIED]: the test corpus has
                        // no aliases).
                        if st != status::ALIAS && info.hash != bin_hash {
                            return Ok(None);
                        }
                        return Ok(Some(RecordRef {
                            pathname: stored.to_vec(),
                            status: st,
                            record_type: rtype,
                            bin_entry_address: entry_addr,
                            info_address: info_addr,
                            info,
                        }));
                    }
                }
                ic += BIN_ENTRY_WORDS as usize + pwords as usize;
                if ic >= bs - 2 {
                    break;
                }
            }
            // Overflow link: word kbinSize-1 (zcheckInternal() line 426).
            bin_addr = b(bs - 1);
        }
        Ok(None)
    }

    /// Re-read an alias's primary info block with the primary's own pathname length,
    /// so `InfoBlock::pathname` is the primary pathname. The first read used the alias
    /// pathname length, which leaves the primary pathname empty or cut when the lengths
    /// differ. `Public/zaliasGetPrimary.c:zaliasGetPrimary()` (hec-dss `7-IU-15`, lines
    /// 126-138) does the same: it takes `kinfoPathnameLength` from the info block and reads
    /// `infoSize + numberLongsInBytes(npath)` words. On a read error the first decode is kept.
    fn primary_info(&mut self, info: InfoBlock) -> InfoBlock {
        let len = info.pathname_length;
        if !(1..=MAX_PATHNAME_BYTES as i64).contains(&len) || info.pathname.len() == len as usize {
            return info;
        }
        match self.read_info(info.address, len as usize) {
            Ok(full) if full.flag == info::INFO_FLAG => full,
            _ => info,
        }
    }

    /// Follow an alias chain to the primary info block (`zcheckInternal()` lines 444–475).
    fn resolve_alias(&mut self, mut address: i64) -> Result<i64, Dss7Error> {
        let max = self.header.number_aliases.clamp(0, 10_000) + 1;
        for _ in 0..max {
            let w = self.read_words(address, 8)?;
            if word(&w, 0) == info::INFO_FLAG {
                return Ok(address);
            }
            if word(&w, bin::STATUS) != status::ALIAS {
                return Err(Dss7Error::BadInfoBlock {
                    info_address: address,
                    problem: InfoProblem::BadFlag(word(&w, 0)),
                });
            }
            address = word(&w, bin::INFO_ADD);
        }
        Err(Dss7Error::BadInfoBlock {
            info_address: address,
            problem: InfoProblem::OutOfBounds,
        })
    }
}

/// Little-endian word `i` of a byte buffer (0 when out of range; callers size buffers).
#[inline]
pub(crate) fn word(bytes: &[u8], i: usize) -> i64 {
    match bytes.get(i * 8..i * 8 + 8) {
        Some(s) => {
            let mut b = [0u8; 8];
            b.copy_from_slice(s);
            i64::from_le_bytes(b)
        }
        None => 0,
    }
}

/// Map a DSS7 bin status to the shared [`crate::RecordStatus`] (dss7.md §3.6).
pub fn record_status(raw: i64) -> crate::RecordStatus {
    use crate::RecordStatus as S;
    match raw {
        status::VALID | status::PRIMARY => S::Live,
        status::ALIAS => S::Alias,
        status::DELETED | status::ALIAS_DELETED | status::REMOVED => S::Deleted,
        status::RENAMED => S::Renamed,
        _ => S::Other,
    }
}

/// One bin entry found by [`Dss7File::discover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundEntry {
    /// Pathname bytes as stored in the bin.
    pub pathname: Vec<u8>,
    /// Bin status (0/1 live, 2 alias, others deleted, renamed or moved).
    pub status: i64,
    /// Record type from the bin entry.
    pub record_type: i32,
    /// The entry's hash word (`zcheckInternal()` compares it with the info block's).
    pub stored_hash: i64,
    /// Word address of the entry.
    pub bin_entry_address: i64,
    /// The entry's info address (an alias's: the start of its alias chain).
    pub info_address: i64,
}

/// What [`Dss7File::discover`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Every parsed entry of every bin reached, each bin once, in traversal order.
    pub entries: Vec<FoundEntry>,
    /// Pointer targets the procedure reached but cannot classify: `(word address, what)`.
    pub unclassified: Vec<(i64, String)>,
    /// Bins visited.
    pub bins_visited: u64,
}

impl<R: Read + Seek> Dss7File<R> {
    /// The discovery procedure of copy-out (`write::repair_dss7`), in one
    /// place. Everything a reader can reach through a pointer of the file's pathname
    /// structures, and nothing else:
    ///
    /// 1. **Roots:** every bin of every block of the bin-block chain (header
    ///    `kaddFirstBin`, each block's chain word after its `kbinsPerBlock` bins; the
    ///    catalog walk of `zcatalogInternal()`), and every non-zero hash-table slot
    ///    (`kaddHashTableStart`, `kmaxHash` slots; where `lookup()` starts).
    /// 2. **Bins:** each bin reached is parsed exactly as [`Dss7File::lookup`] parses it
    ///    (`zcheckInternal()` lines 312-420): entries until a zero hash word or an entry
    ///    whose lengths do not fit. Every entry is reported, whatever its status.
    /// 3. **Overflow:** from **every** bin reached, the overflow word (word
    ///    `kbinSize - 1`) is followed when non-zero, also from a bin whose first word is
    ///    zero (an empty bin): `lookup()` follows it too (`zcheckInternal()` line 426).
    ///    The recovery catalog did not, which hid records behind an empty bin.
    /// 4. **Bounds:** each bin is visited once (a visited set), at most
    ///    `kfileSize / kbinSize + 1` bins. A bin block whose declared span
    ///    (`kbinsPerBlock` bins and the chain word) does not fit the data area stops the
    ///    chain walk and is reported as unclassified; none of its bins is searched, unlike
    ///    the catalog walk, which salvages the whole bins of a partial block. A link
    ///    outside the data area, an entry that
    ///    does not parse after a non-zero hash word, and reaching the visit bound are
    ///    returned in [`Discovery::unclassified`], never dropped.
    /// 5. **Order:** breadth-first; every bin of the bin-block chain first, in the catalog
    ///    walk's physical order, then the hash slots in order, then overflow targets.
    ///    Entries therefore come out in catalog order.
    ///
    /// Each entry's info block (and, for an alias, its alias chain) is read later by
    /// [`Dss7File::record_from_entry`]. Bytes that no pointer reaches (for example an
    /// info block that no entry names, or entries after an entry with a zero hash word,
    /// where `lookup()` stops) are out of scope.
    pub fn discover(&mut self) -> Result<Discovery, Dss7Error> {
        let h = self.header.clone();
        let bs = h.bin_size;
        let mut out = Discovery::default();
        if bs < BIN_ENTRY_WORDS + 2 {
            out.unclassified
                .push((0, format!("bin size {bs} is too small to hold an entry")));
            return Ok(out);
        }
        let max_bins = (h.file_size_words / bs).saturating_add(1) as usize;
        let mut work: Vec<i64> = Vec::new();
        // Roots: the bin-block chain.
        let bpb = h.bins_per_block.max(0);
        let mut block = h.first_bin_address;
        let mut blocks = 0usize;
        let mut seen_blocks: HashSet<i64> = HashSet::new();
        while block != 0 {
            blocks += 1;
            if blocks > max_bins || !seen_blocks.insert(block) {
                out.unclassified
                    .push((block, "the bin-block chain loops or is too long".into()));
                break;
            }
            if !self.in_data_area(block, bs * bpb + 1) {
                out.unclassified
                    .push((block, "a bin block lies outside the data area".into()));
                break;
            }
            for i in 0..bpb {
                work.push(block + i * bs);
            }
            block = word(&self.read_words(block + bs * bpb, 1)?, 0);
        }
        // Roots: every hash-table slot.
        if h.max_hash > 0 && self.in_data_area(h.hash_table_address, h.max_hash) {
            let slots = self.read_words(h.hash_table_address, h.max_hash)?;
            for i in 0..h.max_hash as usize {
                let a = word(&slots, i);
                if a != 0 {
                    work.push(a);
                }
            }
        } else {
            out.unclassified.push((
                h.hash_table_address,
                "the hash table lies outside the file".into(),
            ));
        }
        // Breadth-first, roots in order: every bin of the bin-block chain first, in the
        // physical order of the catalog walk, then the hash slots, then overflow targets.
        // Entries come out in catalog order, so ties between copies of a pathname that no
        // lookup reaches are broken as the catalog lists them.
        let mut work: std::collections::VecDeque<i64> = work.into_iter().collect();
        let mut visited: HashSet<i64> = HashSet::new();
        while let Some(a) = work.pop_front() {
            if visited.contains(&a) {
                continue;
            }
            if visited.len() >= max_bins {
                out.unclassified.push((
                    a,
                    format!("traversal stopped at its bound of {max_bins} bins"),
                ));
                break;
            }
            visited.insert(a);
            if !self.in_data_area(a, bs) {
                out.unclassified
                    .push((a, "a link names a bin outside the data area".into()));
                continue;
            }
            out.bins_visited += 1;
            let bytes = self.read_words(a, bs)?;
            let b = |i: usize| word(&bytes, i);
            let bsu = bs as usize;
            let mut ic = 0usize;
            while ic + (BIN_ENTRY_WORDS as usize) < bsu {
                let eh = b(ic + bin::HASH);
                if eh == 0 {
                    break;
                }
                let (chars, pwords) = info::split_i4(b(ic + bin::PATH_LEN));
                if chars <= 0
                    || chars as usize > MAX_PATHNAME_BYTES
                    || pwords as i64 != (chars as i64 + 7) / 8
                    || ic + BIN_ENTRY_WORDS as usize + pwords as usize > bsu - 1
                {
                    out.unclassified.push((
                        a + ic as i64,
                        "a bin entry with a non-zero hash word does not parse".into(),
                    ));
                    break;
                }
                let po = (ic + bin::PATH) * 8;
                let (rtype, _) = info::split_i4(b(ic + bin::TYPE_AND_CAT_SORT));
                out.entries.push(FoundEntry {
                    pathname: bytes[po..po + chars as usize].to_vec(),
                    status: b(ic + bin::STATUS),
                    record_type: rtype,
                    stored_hash: eh,
                    bin_entry_address: a + ic as i64,
                    info_address: b(ic + bin::INFO_ADD),
                });
                ic += BIN_ENTRY_WORDS as usize + pwords as usize;
                if ic >= bsu - 2 {
                    break;
                }
            }
            let next = b(bsu - 1);
            if next != 0 && !visited.contains(&next) {
                work.push_back(next);
            }
        }
        Ok(out)
    }

    /// The record of an entry found by [`Dss7File::discover`]: its info block read and
    /// checked as [`Dss7File::lookup`] and [`Dss7File::record_at`] do (flag, the hash
    /// rule for non-aliases, an alias's chain resolved to its primary).
    pub fn record_from_entry(&mut self, e: &FoundEntry) -> Result<RecordRef, Dss7Error> {
        let st = e.status;
        if !((status::VALID..=status::PRIMARY).contains(&st) || st == status::ALIAS) {
            return Err(Dss7Error::NotLiveEntry {
                index: 0,
                status: Some(st),
            });
        }
        let mut info_addr = e.info_address;
        if st == status::ALIAS {
            info_addr = self.resolve_alias(info_addr)?;
        }
        let bad = |problem| Dss7Error::BadInfoBlock {
            info_address: info_addr,
            problem,
        };
        let info = self
            .read_info(info_addr, e.pathname.len())
            .map_err(|err| match err {
                Dss7Error::ReadOutOfBounds { .. } => bad(InfoProblem::OutOfBounds),
                other => other,
            })?;
        if info.flag != info::INFO_FLAG {
            return Err(bad(if info.flag == 0 {
                InfoProblem::Zeroed
            } else {
                InfoProblem::BadFlag(info.flag)
            }));
        }
        let info = if st == status::ALIAS {
            self.primary_info(info)
        } else {
            info
        };
        if st != status::ALIAS && info.hash != e.stored_hash {
            return Err(bad(InfoProblem::HashMismatch));
        }
        Ok(RecordRef {
            pathname: e.pathname.clone(),
            status: st,
            record_type: e.record_type,
            bin_entry_address: e.bin_entry_address,
            info_address: info_addr,
            info,
        })
    }
}

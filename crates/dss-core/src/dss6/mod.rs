//! DSS6 reader: header, catalog walk, hash lookup. Format rules:
//! `docs/format/dss6.md`, derived from HEC's `heclib_f` Fortran sources.
//!
//! The reader is read-only and lock-free. It opens a file with a plain read handle (or
//! works on an in-memory byte slice, which is what the WebAssembly binding uses), never
//! writes, and never reads the lock record (dss6.md §4.4, §9.1). Every pointer is
//! bounds-checked against both `KFSIZE` and the physical size, and all loops over
//! on-disk chains are bounded, so hostile input yields errors or
//! [`crate::CatalogAnomaly`] values, never a panic.
//!
//! ```no_run
//! use dss_core::dss6::Dss6File;
//! let file = Dss6File::open("sample.dss")?;
//! let catalog = file.catalog()?;
//! for e in catalog.live() {
//!     println!("{} {}", e.record_type, e.pathname_lossy());
//! }
//! # Ok::<(), dss_core::dss6::Dss6Error>(())
//! ```
//!
//! Time-series reads are in [`ts`] (one block: [`Dss6File::read_ts_record`])
//! and [`dataset`] (blocks stitched over a window: [`Dss6File::read_ts_dataset`]).
//! Paired data and grids are in their own modules; [`Dss6File::read_words`] and
//! [`Dss6Info`] give them the addresses they need.

pub mod compress;
pub mod dataset;
mod error;
pub mod grid;
pub mod hash;
mod header;
mod info;
pub mod interval;
mod lookup;
pub mod pathname;
pub mod pd;
pub mod source;
pub mod ts;
mod walk;
// DSS6 time-series writer (native targets only, as the DSS7 writer).
#[cfg(not(target_arch = "wasm32"))]
pub mod write;

pub use dataset::{DatasetIndex, DatasetOptions, DatasetReport, Dss6Dataset, TimeWindow};
pub use error::Dss6Error;
pub use hash::{normalize_pathname, pathname_hash, zhash6};
pub use header::{Dss6Header, EOF_FLAG, HEADER_BYTES, KFSIZE_WRITE_LOCKED, NPERM};
pub use info::{Dss6Info, INFO_FLAG, NINFO};
pub use lookup::Dss6Record;
pub use pd::{Dss6Pd, Dss6PdMeta, LabelStyle, PdError};
pub use source::{word_byte_offset, FileSource, ReadAt};
pub use ts::{Dss6TsRecord, TsError, TsLayout, TsReadOptions};
// Opt-in HEC "no data" refusal.
pub use ts::{all_hec_no_data, NoDataPolicy, NO_DATA_STATUS};
pub use walk::{record_type_abbrev, BinWalk, CatalogOptions, Dss6BinEntry, InfoCheck};

use crate::catalog::{AnomalyKind, CatalogAnomaly};
use source::physical_words;
use std::path::Path;

/// Longest pathname HEC writes (`dss_maxpath` = 392, `headers/dss_parameters.h`;
/// `zwritex6.f` lines 46-49).
pub const MAX_PATHNAME: i32 = 392;
/// Fixed words in a bin entry besides the pathname (`zinit6.f`, `NLBIN = 11`).
pub const NLBIN: usize = 11;

/// An open DSS6 file.
#[derive(Debug)]
pub struct Dss6File<S: ReadAt> {
    src: S,
    header: Dss6Header,
    /// Highest readable word address (inclusive).
    limit_words: i64,
    lock_bytes: (u64, u64),
    eof_word: Option<i32>,
    warnings: Vec<CatalogAnomaly>,
}

impl Dss6File<FileSource> {
    /// Open a file read-only.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Dss6Error> {
        let src = FileSource::open(path).map_err(Dss6Error::Io)?;
        Self::new(src)
    }
}

impl<'a> Dss6File<&'a [u8]> {
    /// Read from an in-memory copy of the file (used by the WebAssembly binding).
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, Dss6Error> {
        Self::new(bytes)
    }
}

impl<S: ReadAt> Dss6File<S> {
    /// Parse the header of `src` and prepare for reads.
    pub fn new(src: S) -> Result<Self, Dss6Error> {
        let size = src.size();
        let want = (HEADER_BYTES as u64).min(size) as usize;
        let mut head = vec![0u8; want];
        src.read_exact_at(0, &mut head).map_err(Dss6Error::Io)?;
        let header = Dss6Header::parse(&head)?;

        let phys = physical_words(size);
        let limit_words = if header.is_write_locked() {
            // zopen6Int.f lines 212-229: size unknown, HEC estimates it from the file.
            phys
        } else {
            // KFSIZE is the next free word and holds the EOF flag (dss6.md §1.3).
            phys.min(i64::from(header.file_size_words))
        };
        // Files before 6-K keep live data in the lock record ("pre 6-K file structure
        // saved information on the same record used for locking", zgtrec6.f:ZGTREC6
        // lines 122-157, which reads it normally and only works around the 12 lock
        // bytes when another process holds them). Only 6-K and later keep it blank.
        // 6-K = KVERNO 60000 + 100*ichar('K') (zopen6Int.f line 250).
        let lock_bytes = if header.version_number < 60_000 + 100 * i32::from(b'K') {
            (0, 0)
        } else {
            header.lock_byte_range()
        };
        let mut file = Self {
            src,
            header,
            limit_words,
            lock_bytes,
            eof_word: None,
            warnings: Vec::new(),
        };
        file.check_layout(size);
        Ok(file)
    }

    /// Non-fatal header findings (version and layout checks).
    fn check_layout(&mut self, size: u64) {
        let h = &self.header;
        let mut warn = |kind, address: Option<u64>, detail: String| {
            self.warnings.push(CatalogAnomaly {
                kind,
                address,
                detail,
            });
        };
        if h.is_write_locked() {
            warn(
                AnomalyKind::Other,
                Some(20),
                "KFSIZE = -1357: file write-locked or left locked; reads bounded by physical size"
                    .into(),
            );
        }
        if let Some(expected) = h.expected_file_bytes() {
            // A short (508-byte) final record is accepted (dss6.md §1.3).
            if size < expected - 4 {
                warn(
                    AnomalyKind::OutOfBounds,
                    None,
                    format!("file is {size} bytes, header implies {expected}: truncated"),
                );
            } else if size != expected && size != expected - 4 {
                warn(
                    AnomalyKind::Other,
                    None,
                    format!("file is {size} bytes, header implies {expected}"),
                );
            }
        }
        // The first bin should follow the lock record (zopen6Int.f lines 410-451).
        let lock_rec = h.lock_record();
        let first_bin_rec = (i64::from(h.first_bin_address) - 1) / source::WORDS_PER_RECORD + 1;
        if first_bin_rec <= lock_rec {
            warn(
                AnomalyKind::Other,
                word_byte_offset(h.first_bin_address.into()),
                format!(
                    "KAFBIN {} is not after lock record {lock_rec}",
                    h.first_bin_address
                ),
            );
        }
        if !self.header.is_write_locked() {
            self.eof_word = self
                .read_words(self.header.file_size_words.into(), 1)
                .ok()
                .map(|w| w[0]);
        }
    }

    pub fn header(&self) -> &Dss6Header {
        &self.header
    }

    /// Header and layout findings recorded at open. They are also the first anomalies of
    /// every catalog.
    pub fn warnings(&self) -> &[CatalogAnomaly] {
        &self.warnings
    }

    /// The word at address `KFSIZE` (normally [`EOF_FLAG`]). `None` when write-locked or
    /// unreadable. Some HEC-HMS 4.x writers leave it 0.
    pub fn eof_word(&self) -> Option<i32> {
        self.eof_word
    }

    /// Highest word address reads may touch: `min(KFSIZE, physical words)`.
    pub fn limit_words(&self) -> i64 {
        self.limit_words
    }

    /// Size of the underlying source in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.src.size()
    }

    /// Read `n` logical words starting at word address `addr`, skipping record pads
    /// (`zgtrec6.f:ZGTREC6` lines 81-191). Fails for ranges outside `1..=limit_words`
    /// and for any range that overlaps the lock record.
    pub fn read_words(&self, addr: i64, n: usize) -> Result<Vec<i32>, Dss6Error> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let oob = || Dss6Error::OutOfBounds {
            address: addr,
            words: n,
        };
        let last = addr.checked_add(n as i64 - 1).ok_or_else(oob)?;
        if addr < 1 || last > self.limit_words {
            return Err(oob());
        }
        let start = word_byte_offset(addr).ok_or_else(oob)?;
        let end = word_byte_offset(last).ok_or_else(oob)? + 4;
        if start < self.lock_bytes.1 && end > self.lock_bytes.0 {
            return Err(Dss6Error::LockRecord { address: addr });
        }
        if end > self.src.size() {
            return Err(oob());
        }
        let mut raw = vec![0u8; (end - start) as usize];
        self.src
            .read_exact_at(start, &mut raw)
            .map_err(Dss6Error::Io)?;
        let mut out = Vec::with_capacity(n);
        let mut pos = 0usize;
        let mut in_rec = ((start % source::RECORD_BYTES) / 4) as usize;
        while out.len() < n {
            if in_rec == source::WORDS_PER_RECORD as usize {
                pos += 4; // skip the record pad
                in_rec = 0;
                continue;
            }
            let b = &raw[pos..pos + 4];
            out.push(i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            pos += 4;
            in_rec += 1;
        }
        Ok(out)
    }

    /// Read and decode the info block at `addr` for a pathname of `npath` characters.
    pub fn read_info(&self, addr: i64, npath: i32) -> Result<Dss6Info, Dss6Error> {
        if !(1..=MAX_PATHNAME).contains(&npath) {
            return Err(Dss6Error::Corrupt {
                address: Some(addr),
                detail: format!("pathname length {npath}"),
            });
        }
        let words = self.read_words(addr, info::info_words(npath))?;
        Dss6Info::decode(addr, &words).ok_or_else(|| Dss6Error::Corrupt {
            address: Some(addr),
            detail: format!(
                "info block pathname length {} differs from {npath}",
                words[2]
            ),
        })
    }
}

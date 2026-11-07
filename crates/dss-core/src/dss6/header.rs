//! DSS6 permanent section (file header).
//!
//! The header is `NPERM = 69` words at word addresses 1..69, read in one go by
//! `heclib_f/src/zrdprm6.f:ZRDPRM6` line 51. Word offsets come from the pointer chain in
//! `heclib_f/src/zinit6.f:ZINIT6` lines 446-572 (dss6.md §3). Validation follows
//! `heclib_f/src/zopen6Int.f:ZOPEN6INT` lines 240-301.

use super::error::Dss6Error;
use super::source::{record_and_word, WORDS_PER_RECORD};
use crate::header::{ByteOrder, DssVersion, FileSignature, HeaderError};

/// Header length in words (`zinit6.f` lines 566-572, `KEY2 - KPERM`).
pub const NPERM: usize = 69;
/// Header length in bytes; it always lies inside record 1.
pub const HEADER_BYTES: usize = NPERM * 4;
/// `KFSIZE` value while a program holds the write lock (`zwlock6.f:ZWLOCK6` line 69).
pub const KFSIZE_WRITE_LOCKED: i32 = -1357;
/// End-of-file flag written at address `KFSIZE` (`zptrec6.f:ZPTREC6` lines 197-210).
pub const EOF_FLAG: i32 = -9630;

// 0-based word indexes (dss6.md §3 table).
const KNRECS: usize = 1;
const KSEQNO: usize = 2;
const KHSIZE: usize = 3;
const KVERS: usize = 4;
const KFSIZE: usize = 5;
const KDEAD: usize = 6;
const KCREAT: usize = 7;
const KLWDAT: usize = 9;
const KLWTIM: usize = 11;
const KTAGBK: usize = 13;
const KHASH: usize = 14;
const KTABLE: usize = 15;
const KBNBLK: usize = 16;
const KBNREM: usize = 17;
const KBNSIZ: usize = 18;
const KAFBIN: usize = 19;
const KANBIN: usize = 20;
const KBINS: usize = 37;
const KHUSED: usize = 38;
const KBOVER: usize = 39;
const KCOMPI: usize = 42;
const KMXPRT: usize = 52;
const KCOLL: usize = 54;

/// Smallest and largest accepted `KBNSIZ`: one 11-word entry plus a 1-word path and the
/// two pointer words (`zcheck6.f:ZCHECK6` lines 57-64), and the `IPNBIN(NBSIZE+4)` bin
/// buffer (`headers/zdssiz.h`; `NBSIZE = 127`).
pub const MIN_BIN_SIZE: i32 = 13;
pub const MAX_BIN_SIZE: i32 = 127;

/// Parsed DSS6 permanent section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dss6Header {
    /// All 69 header words as stored (little-endian).
    pub words: [i32; NPERM],
    /// Version string, e.g. `6-YN` (`KVERS`).
    pub version_string: String,
    /// `60000 + 100*c3 + c4` (`zopen6Int.f:ZOPEN6INT` line 250).
    pub version_number: i32,
    /// `KNRECS`: live record count.
    pub number_records: i32,
    /// `KSEQNO`.
    pub sequence_number: i32,
    /// `KHSIZE`: hash size code 1..9 (information only).
    pub hash_size_code: i32,
    /// `KFSIZE`: next free 1-based word address; `-1357` while write-locked.
    pub file_size_words: i32,
    /// `KDEAD`: dead space in words.
    pub dead_words: i32,
    /// `KCREAT`, `KLWDAT`, `KLWTIM` as raw 8-byte fields.
    pub create_date: [u8; 8],
    pub last_write_date: [u8; 8],
    pub last_write_time: [u8; 8],
    /// `KTAGBK`.
    pub tag_block_address: i32,
    /// `KHASH`: hash-table size, also the largest hash code.
    pub max_hash: i32,
    /// `KTABLE`: 1 = hash table, 2 = direct bins.
    pub table_type: i32,
    /// `KBNBLK`: bins per bin block.
    pub bins_per_block: i32,
    /// `KBNREM`.
    pub bins_remaining: i32,
    /// `KBNSIZ`: bin size in words.
    pub bin_size: i32,
    /// `KAFBIN`: address of the first bin.
    pub first_bin_address: i32,
    /// `KANBIN`: address of the next available bin.
    pub next_bin_address: i32,
    /// `KBINS`, `KHUSED`, `KBOVER` (statistics).
    pub bins_used: i32,
    pub hash_codes_used: i32,
    pub overflow_bins: i32,
    /// `KCOMPI`: file-default compression information address.
    pub compression_info_address: i32,
    /// `KMXPRT`: maximum length of pathname parts A..F (one byte each).
    pub max_part_lengths: [u8; 6],
    /// `KCOLL`: 1 if the file holds collection records.
    pub collections: i32,
}

impl Dss6Header {
    /// Parse and validate the header from at least [`HEADER_BYTES`] leading bytes.
    ///
    /// Typed errors (P05 acceptance check 1): bad signature, version not `6-`, byte-swapped
    /// (`KTABLE > 10000`), `KTABLE` not 1/2, `KBNSIZ` out of range, and other fields that
    /// make the file unreadable. `KFSIZE = -1357` is accepted (write-locked; reads are then
    /// bounded by the physical size, `zopen6Int.f` lines 212-229).
    pub fn parse(head: &[u8]) -> Result<Self, Dss6Error> {
        let sig = FileSignature::from_bytes(head)?;
        if sig.version != DssVersion::V6 {
            return Err(Dss6Error::NotVersion6 {
                version: sig.version_string,
            });
        }
        if head.len() < HEADER_BYTES {
            return Err(HeaderError::TooShort(head.len()).into());
        }
        let mut words = [0i32; NPERM];
        for (i, w) in words.iter_mut().enumerate() {
            let b = &head[4 * i..4 * i + 4];
            *w = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let ktable = words[KTABLE];
        if sig.byte_order == ByteOrder::Big {
            return Err(Dss6Error::ByteSwapped { ktable });
        }
        // Version: HEC accepts any file whose first two characters match `6-`
        // (zopen6Int.f line 254).
        let vb = &head[4 * KVERS..4 * KVERS + 4];
        if vb[1] != b'-' {
            return Err(Dss6Error::NotVersion6 {
                version: sig.version_string,
            });
        }
        // Byte swap detection (zopen6Int.f lines 256-267): KTABLE > 10000 and the swapped
        // value is 1 or 2. Other large values are plain corruption.
        if ktable > 10000 {
            let swapped = ktable.swap_bytes();
            if swapped == 1 || swapped == 2 {
                return Err(Dss6Error::ByteSwapped { ktable });
            }
            return Err(Dss6Error::BadTableType(ktable));
        }
        if ktable != 1 && ktable != 2 {
            return Err(Dss6Error::BadTableType(ktable));
        }
        let bnsiz = words[KBNSIZ];
        if !(MIN_BIN_SIZE..=MAX_BIN_SIZE).contains(&bnsiz) {
            return Err(Dss6Error::BinSizeOutOfRange(bnsiz));
        }
        let kfsize = words[KFSIZE];
        // zopen6Int.f lines 269-276: any negative size other than the write-lock marker
        // makes the open fail.
        if kfsize < 0 && kfsize != KFSIZE_WRITE_LOCKED {
            return Err(Dss6Error::BadHeader {
                field: "KFSIZE",
                value: kfsize.into(),
            });
        }
        let khash = words[KHASH];
        if khash < 1 {
            return Err(Dss6Error::BadHeader {
                field: "KHASH",
                value: khash.into(),
            });
        }
        if ktable == 1 && words[KBNBLK] < 1 {
            return Err(Dss6Error::BadHeader {
                field: "KBNBLK",
                value: words[KBNBLK].into(),
            });
        }
        let afbin = words[KAFBIN];
        let afbin_ok = afbin >= 1 && (kfsize == KFSIZE_WRITE_LOCKED || afbin < kfsize);
        if !afbin_ok {
            return Err(Dss6Error::BadHeader {
                field: "KAFBIN",
                value: afbin.into(),
            });
        }

        let field8 = |i: usize| -> [u8; 8] {
            let mut out = [0u8; 8];
            out.copy_from_slice(&head[4 * i..4 * i + 8]);
            out
        };
        let mut max_part_lengths = [0u8; 6];
        max_part_lengths.copy_from_slice(&head[4 * KMXPRT..4 * KMXPRT + 6]);
        let version_string = String::from_utf8_lossy(vb)
            .trim_end_matches(['\0', ' '])
            .to_string();
        Ok(Self {
            words,
            version_number: 60000 + 100 * i32::from(vb[2]) + i32::from(vb[3]),
            version_string,
            number_records: words[KNRECS],
            sequence_number: words[KSEQNO],
            hash_size_code: words[KHSIZE],
            file_size_words: kfsize,
            dead_words: words[KDEAD],
            create_date: field8(KCREAT),
            last_write_date: field8(KLWDAT),
            last_write_time: field8(KLWTIM),
            tag_block_address: words[KTAGBK],
            max_hash: khash,
            table_type: ktable,
            bins_per_block: words[KBNBLK],
            bins_remaining: words[KBNREM],
            bin_size: bnsiz,
            first_bin_address: afbin,
            next_bin_address: words[KANBIN],
            bins_used: words[KBINS],
            hash_codes_used: words[KHUSED],
            overflow_bins: words[KBOVER],
            compression_info_address: words[KCOMPI],
            max_part_lengths,
            collections: words[KCOLL],
        })
    }

    /// True while `KFSIZE = -1357` (write lock held or left behind).
    pub fn is_write_locked(&self) -> bool {
        self.file_size_words == KFSIZE_WRITE_LOCKED
    }

    /// 1-based record number of the lock record. HEC uses the record that follows the
    /// permanent section and hash table: address `KHASH + NPERM + 1` (type 1) or
    /// `NPERM + 1` (type 2), rounded up to a record boundary
    /// (`zopen6Int.f:ZOPEN6INT` lines 280-287; layout lines 410-451).
    pub fn lock_record(&self) -> i64 {
        let size = if self.table_type == 1 {
            i64::from(self.max_hash) + NPERM as i64 + 1
        } else {
            NPERM as i64 + 1
        };
        match record_and_word(size) {
            Some((rec, 1)) => rec,
            Some((rec, _)) => rec + 1,
            // Only reachable for KHASH near i32::MAX; place the lock record past it.
            None => (size - 1) / WORDS_PER_RECORD + 2,
        }
    }

    /// Byte range `[start, end)` of the lock record (`KLOCKB`).
    pub fn lock_byte_range(&self) -> (u64, u64) {
        let start = (self.lock_record() as u64 - 1) * 512;
        (start, start + 512)
    }

    /// File size implied by `KFSIZE`: `ceil(KFSIZE/127) * 512` bytes (dss6.md §1.3).
    /// `None` while write-locked.
    pub fn expected_file_bytes(&self) -> Option<u64> {
        if self.file_size_words <= 0 {
            return None;
        }
        let words = self.file_size_words as u64;
        Some(words.div_ceil(WORDS_PER_RECORD as u64) * 512)
    }

    /// Create date as text (blank-padded `DDMMMYY`; `*******` in some files).
    pub fn create_date_str(&self) -> String {
        latin1_trim(&self.create_date[..7])
    }
}

pub(crate) fn latin1_trim(b: &[u8]) -> String {
    b.iter()
        .map(|&c| c as char)
        .collect::<String>()
        .trim_end_matches(['\0', ' '])
        .to_string()
}

/// A Fortran `CHARACTER` field as HEC's C layer returns it: trailing blanks removed
/// (`Utilities/stringFortToC.c`), and the C string ends at the first NUL (every C/JNI
/// consumer reads it with `strlen`; `ztsRetrieveReg6.c` lines 217-220,
/// `ztsRetrieveIrreg6.c` lines 153, 187-189). Latin-1, lossless for bytes.
pub(crate) fn fort_to_c(b: &[u8]) -> String {
    let end = b.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1);
    let b = &b[..end];
    let b = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
    b.iter().map(|&c| c as char).collect()
}

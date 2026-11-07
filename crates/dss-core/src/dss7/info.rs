//! DSS7 record info block ("record header"), `docs/format/dss7.md` §5.1.
//!
//! Source: `heclib_c/src/Internal/zinit.c:zinit()` (`zdssInfoKeys`), written by
//! `Internal/zwriteNew.c:zwriteNew()`, read by `Internal/zreadInfo.c:zreadInfo()`.

/// `DSS_INFO_FLAG` (`headers/zdssVals.h`), word `kinfoFlag` of every info block.
pub const INFO_FLAG: i64 = -97534;
/// `zdssVals.infoSize`: words before the pathname.
pub const INFO_FIXED_WORDS: usize = 30;

/// `zdssInfoKeys` word indexes.
pub mod key {
    pub const FLAG: usize = 0;
    pub const STATUS: usize = 1;
    pub const PATHNAME_LENGTH: usize = 2;
    pub const HASH: usize = 3;
    pub const TYPE_VERSION: usize = 4;
    pub const EXPANSION: usize = 5;
    pub const LAST_WRITE_TIME: usize = 6;
    pub const PROGRAM: usize = 7;
    pub const FIRST_DATE: usize = 9;
    pub const LAST_DATE: usize = 10;
    pub const CREATION_TIME: usize = 11;
    pub const INTERNAL_HEAD_ADDRESS: usize = 13;
    pub const INTERNAL_HEAD_NUMBER: usize = 14;
    pub const HEADER2_ADDRESS: usize = 15;
    pub const HEADER2_NUMBER: usize = 16;
    pub const USER_HEAD_ADDRESS: usize = 17;
    pub const USER_HEAD_NUMBER: usize = 18;
    pub const VALUES1_ADDRESS: usize = 19;
    pub const VALUES1_NUMBER: usize = 20;
    pub const VALUES2_ADDRESS: usize = 21;
    pub const VALUES2_NUMBER: usize = 22;
    pub const VALUES3_ADDRESS: usize = 23;
    pub const VALUES3_NUMBER: usize = 24;
    pub const ALLOCATED_SIZE: usize = 25;
    pub const NUMBER_DATA: usize = 26;
    pub const LOGICAL_NUMBER: usize = 27;
    pub const ALIASES_BIN_ADDRESS: usize = 28;
    pub const PATHNAME: usize = 30;
}

/// `i8toi4` on little-endian: `a` is the low 4 bytes, `b` the high 4 bytes
/// (`Utilities/i8toi4.c`; dss7.md §1).
#[inline]
pub fn split_i4(w: i64) -> (i32, i32) {
    (w as u32 as i32, (w >> 32) as i32)
}

/// A data area of a record: word address and length in 4-byte ints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Area {
    /// Word address (byte offset = address × 8).
    pub address: i64,
    /// Length in int32 units.
    pub number_ints: i64,
}

/// Decoded info block. Addresses are word addresses as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoBlock {
    /// Word address of this info block.
    pub address: i64,
    pub flag: i64,
    pub status: i64,
    pub pathname_length: i64,
    pub hash: i64,
    pub record_type: i32,
    /// Number of times written (starts at 1).
    pub version: i32,
    pub number_expansions: i32,
    pub expansion_flag: i32,
    pub last_write_ms: i64,
    /// Program name, NUL-trimmed (16 bytes on disk).
    pub program: String,
    /// `(julianDay, secondsOfDay)` of the first valid value (time series; 0 otherwise).
    pub first_date: (i32, i32),
    pub last_date: (i32, i32),
    pub creation_ms: i64,
    pub internal_header: Area,
    pub header2: Area,
    pub user_header: Area,
    pub values1: Area,
    pub values2: Area,
    pub values3: Area,
    /// int32 allocated for values1..3.
    pub allocated_size: i64,
    pub number_data: i64,
    pub logical_number: i64,
    pub aliases_bin_address: i64,
    /// Pathname bytes stored in the info block (`pathname_length` bytes, when in range).
    pub pathname: Vec<u8>,
}

impl InfoBlock {
    /// Decode an info block from its words. `bytes` must start at the info address and
    /// hold at least [`INFO_FIXED_WORDS`] words; the pathname is taken from what follows
    /// when present. Returns `None` if `bytes` is too short.
    pub fn decode(address: i64, bytes: &[u8]) -> Option<Self> {
        if bytes.len() < INFO_FIXED_WORDS * 8 {
            return None;
        }
        let w = |i: usize| -> i64 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            i64::from_le_bytes(b)
        };
        let (record_type, version) = split_i4(w(key::TYPE_VERSION));
        let (number_expansions, expansion_flag) = split_i4(w(key::EXPANSION));
        let program_bytes = &bytes[key::PROGRAM * 8..key::PROGRAM * 8 + 16];
        let end = program_bytes.iter().position(|&b| b == 0).unwrap_or(16);
        let program: String = program_bytes[..end].iter().map(|&b| b as char).collect();
        let area = |a: usize, n: usize| Area {
            address: w(a),
            number_ints: w(n),
        };
        let pathname_length = w(key::PATHNAME_LENGTH);
        let pstart = key::PATHNAME * 8;
        let pathname = if (1..=super::MAX_PATHNAME_BYTES as i64).contains(&pathname_length) {
            let pend = pstart + pathname_length as usize;
            bytes
                .get(pstart..pend)
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Some(InfoBlock {
            address,
            flag: w(key::FLAG),
            status: w(key::STATUS),
            pathname_length,
            hash: w(key::HASH),
            record_type,
            version,
            number_expansions,
            expansion_flag,
            last_write_ms: w(key::LAST_WRITE_TIME),
            program: program.trim_end().to_string(),
            first_date: split_i4(w(key::FIRST_DATE)),
            last_date: split_i4(w(key::LAST_DATE)),
            creation_ms: w(key::CREATION_TIME),
            internal_header: area(key::INTERNAL_HEAD_ADDRESS, key::INTERNAL_HEAD_NUMBER),
            header2: area(key::HEADER2_ADDRESS, key::HEADER2_NUMBER),
            user_header: area(key::USER_HEAD_ADDRESS, key::USER_HEAD_NUMBER),
            values1: area(key::VALUES1_ADDRESS, key::VALUES1_NUMBER),
            values2: area(key::VALUES2_ADDRESS, key::VALUES2_NUMBER),
            values3: area(key::VALUES3_ADDRESS, key::VALUES3_NUMBER),
            allocated_size: w(key::ALLOCATED_SIZE),
            number_data: w(key::NUMBER_DATA),
            logical_number: w(key::LOGICAL_NUMBER),
            aliases_bin_address: w(key::ALIASES_BIN_ADDRESS),
            pathname,
        })
    }

    /// Why this info block does not belong to a live bin entry, or `None` if it does.
    ///
    /// Checks from `zreadInfo()` / `zcheckInternal()` (dss7.md §3.7 step 6, §5.1): flag
    /// −97534, status 0 or 1, hash equal to the bin hash, pathname bytes equal.
    pub fn mismatch(&self, bin_hash: i64, pathname: &[u8]) -> Option<InfoProblem> {
        if self.flag != INFO_FLAG {
            return Some(if self.flag == 0 {
                InfoProblem::Zeroed
            } else {
                InfoProblem::BadFlag(self.flag)
            });
        }
        if self.hash != bin_hash {
            return Some(InfoProblem::HashMismatch);
        }
        if !(0..=1).contains(&self.status) {
            return Some(InfoProblem::Status(self.status));
        }
        if self.pathname != pathname {
            return Some(InfoProblem::PathnameMismatch);
        }
        None
    }
}

/// Reason an info block failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoProblem {
    /// Info flag word is 0 (unwritten info block; P02 anomaly 2).
    Zeroed,
    BadFlag(i64),
    /// Info hash differs from the bin hash: the space was reused (`zreadInfo()`).
    HashMismatch,
    /// Info status is not 0/1 (moved, deleted, ...).
    Status(i64),
    PathnameMismatch,
    /// Info address outside the file or inside the lock area.
    OutOfBounds,
}

impl std::fmt::Display for InfoProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InfoProblem::Zeroed => write!(f, "info block is zeroed"),
            InfoProblem::BadFlag(v) => write!(f, "info flag {v} != -97534"),
            InfoProblem::HashMismatch => write!(f, "info hash differs from bin hash"),
            InfoProblem::Status(s) => write!(f, "info status {s} is not live"),
            InfoProblem::PathnameMismatch => write!(f, "info pathname differs from bin pathname"),
            InfoProblem::OutOfBounds => write!(f, "info address out of bounds"),
        }
    }
}

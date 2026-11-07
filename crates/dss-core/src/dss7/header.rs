//! DSS7 permanent section (file header), `docs/format/dss7.md` §2.
//!
//! Sources (paths relative to `heclib/heclib_c/src/`):
//! - Word map: `Internal/zinit.c:zinit()` (`zdssFileKeys`), `headers/zdssKeys.h`.
//! - Signature and version: `Internal/zpermCreate.c:zpermCreate()`,
//!   `Public/zgetFileVersion.c:zgetFileVersion()`, `Internal/zopenInternal.c:zopenInternal()`.
//! - Validity: `Internal/zpermRead.c:zpermRead()` (word 99 = −97531; bin size 20..1000;
//!   header size 0 means 100).
//! - Numeric revision: `Public/zinquire.c:zinquire()` ("fver").
//! - Test-only hash detune: `Internal/zhash.c:zhash()` (`kdetune`).

use super::Dss7Error;

/// Words in the permanent section that this reader decodes (`DSS_MIN_FILE_HEADER_SIZE`).
pub const HEADER_WORDS: usize = 100;
/// Bytes in [`HEADER_WORDS`].
pub const HEADER_BYTES: usize = HEADER_WORDS * 8;

/// `DSS_END_HEADER_FLAG` (`headers/zdssVals.h`), stored in word `kendFileHeader` (99).
pub const END_HEADER_FLAG: i64 = -97531;
/// `DSS_END_FILE_FLAG`, written at word `kfileSize` by `Internal/zwriteEOF.c:zwriteEOF()`.
pub const END_FILE_FLAG: i64 = -97532;
/// `DSS_MIN_FILE_HEADER_SIZE`; first word after the permanent section.
pub const MIN_HEADER_SIZE: i64 = 100;

/// `zdssFileKeys` word indexes (`zinit.c`; dss7.md §2.2).
pub mod key {
    pub const DSS: usize = 0;
    pub const FILE_HEADER_SIZE: usize = 1;
    pub const VERSION: usize = 2;
    pub const NUMBER_RECORDS: usize = 3;
    pub const NUMBER_ALIASES: usize = 4;
    pub const FILE_SIZE: usize = 5;
    pub const DEAD: usize = 6;
    pub const NUMBER_EXPANSIONS: usize = 7;
    pub const NUMBER_COLLECTIONS: usize = 8;
    pub const NUMBER_RENAMES: usize = 9;
    pub const NUMBER_DELETES: usize = 10;
    pub const NUMBER_ALIAS_DELETES: usize = 11;
    pub const CREATE_DATE: usize = 12;
    pub const LAST_WRITE_TIME: usize = 13;
    pub const LOCK_ADDRESS_WORD: usize = 14;
    pub const MAX_HASH: usize = 15;
    pub const HASHS_USED: usize = 16;
    pub const ADD_HASH_TABLE_START: usize = 19;
    pub const HASH_COLLISIONS: usize = 20;
    pub const BINS_PER_BLOCK: usize = 21;
    pub const BIN_SIZE: usize = 23;
    pub const ADD_FIRST_BIN: usize = 24;
    pub const ADD_NEXT_EMPTY_BIN: usize = 25;
    pub const TOTAL_BINS: usize = 26;
    pub const BINS_OVERFLOW: usize = 27;
    pub const CAT_SORT_STATUS: usize = 33;
    pub const CAT_SORT_NUMBER: usize = 37;
    pub const CAT_SORT_ADDRESS: usize = 38;
    pub const MAX_PATH: usize = 51;
    pub const DETUNE: usize = 89;
    pub const LOCK_ARRAY_SIZES: usize = 90;
    pub const LOCK_WRITE_ARRAY_ADDRESS: usize = 91;
    pub const LOCK_READ_ARRAY_ADDRESS: usize = 92;
    pub const PID_ARRAY_ADDRESS: usize = 93;
    pub const ENDIAN: usize = 94;
    pub const END_FILE_HEADER: usize = 99;
}

/// Decoded and validated DSS7 permanent section.
///
/// All 100 raw words are kept in [`Dss7Header::words`]; the named fields are the ones
/// the reader depends on (dss7.md §2.3 "Reader needs").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dss7Header {
    /// Raw words 0..100, little-endian decoded.
    pub words: [i64; HEADER_WORDS],
    /// Four-character writer revision, e.g. `7-IU` (word 2).
    pub version_string: String,
    /// `zinquire(…,"fver")` revision number, e.g. 70921 for `7-IU`.
    pub fver: i32,
    /// Header size in words; a stored 0 is reported as 100 (`zpermRead()`).
    pub header_size: i64,
    /// True when word 1 stored 0 (only the `7-10` writer, dss7.md §2.1).
    pub header_size_was_zero: bool,
    pub number_records: i64,
    pub number_aliases: i64,
    /// Word address of the end-of-file flag (−97532).
    pub file_size_words: i64,
    pub dead_words: i64,
    pub number_expansions: i64,
    pub number_renames: i64,
    pub number_deletes: i64,
    pub number_alias_deletes: i64,
    pub create_time_ms: i64,
    pub last_write_time_ms: i64,
    pub max_hash: i64,
    pub hash_table_address: i64,
    pub bins_per_block: i64,
    pub bin_size: i64,
    pub first_bin_address: i64,
    pub total_bins: i64,
    pub bins_overflow: i64,
    pub catalog_sort_status: i64,
    /// True when the file uses the old locking scheme (`klockReadArrayAddress` = 0,
    /// `Internal/zlockingOld.c`).
    pub old_locking: bool,
    /// First word after the lock area (`[100, lock_area_end)` is never read; dss7.md §6.3).
    pub lock_area_end: i64,
}

/// Numeric revision from a 4-byte version string (`Public/zinquire.c:zinquire()` "fver").
pub fn fver(version: &[u8; 4]) -> i32 {
    let l = |c: u8| -> i32 {
        let c = i32::from(c);
        c - if c < i32::from(b'[') {
            i32::from(b'@')
        } else {
            i32::from(b'`')
        }
    };
    10000 * (i32::from(version[0]) - i32::from(b'0')) + 100 * l(version[2]) + l(version[3])
}

impl Dss7Header {
    /// Parse and validate the permanent section.
    ///
    /// `head` must hold at least the first 800 bytes; `file_len` is the length of the
    /// whole file in bytes (used for the truncation check).
    pub fn parse(head: &[u8], file_len: u64) -> Result<Self, Dss7Error> {
        if head.len() < 20 {
            return Err(Dss7Error::TooShort { len: file_len });
        }
        // zopenInternal(): bytes 0..4 must be "ZDSS".
        if &head[0..4] != b"ZDSS" {
            return Err(Dss7Error::BadSignature);
        }
        let version: [u8; 4] = [head[16], head[17], head[18], head[19]];
        // zopenInternal(): first version character must be '7' (6 or 8 is
        // INCOMPATIBLE_VERSION).
        if version[0] != b'7' {
            return Err(Dss7Error::NotVersion7 { version });
        }
        if head.len() < HEADER_BYTES {
            return Err(Dss7Error::TooShort { len: file_len });
        }
        let mut words = [0i64; HEADER_WORDS];
        for (i, w) in words.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&head[i * 8..i * 8 + 8]);
            *w = i64::from_le_bytes(b);
        }
        // zpermRead(): fixed index 99 regardless of the stored header size.
        if words[key::END_FILE_HEADER] != END_HEADER_FLAG {
            return Err(Dss7Error::BadEndHeaderFlag(words[key::END_FILE_HEADER]));
        }
        // zhash(): a non-zero kdetune hard-wires every hash (test builds only).
        if words[key::DETUNE] != 0 {
            return Err(Dss7Error::DetuneSet(words[key::DETUNE]));
        }
        let raw_hs = words[key::FILE_HEADER_SIZE];
        let header_size = if raw_hs == 0 { MIN_HEADER_SIZE } else { raw_hs };
        let bin_size = words[key::BIN_SIZE];
        // zpermRead(): bin size outside 20..1000 is INVALID_HEADER_PARAMETER.
        if !(20..=1000).contains(&bin_size) {
            return Err(Dss7Error::BadBinSize(bin_size));
        }
        let bins_per_block = words[key::BINS_PER_BLOCK];
        // znewFileSize() always writes 32; dss7.md §9 accepts 1..1024.
        if !(1..=1024).contains(&bins_per_block) {
            return Err(Dss7Error::BadBinsPerBlock(bins_per_block));
        }
        let max_hash = words[key::MAX_HASH];
        // zhash() casts kmaxHash to int and uses frexp(); znewFileSize() accepts 1..100000
        // and writes at most 64000. Larger values cannot come from HEC.
        if !(1..=1_000_000).contains(&max_hash) {
            return Err(Dss7Error::BadMaxHash(max_hash));
        }
        let file_size_words = words[key::FILE_SIZE];
        if file_size_words < MIN_HEADER_SIZE {
            return Err(Dss7Error::PointerOutOfRange {
                field: "kfileSize",
                value: file_size_words,
            });
        }
        // zwriteEOF(): the EOF flag occupies word kfileSize, so the file holds
        // (kfileSize + 1) words. A shorter file is truncated.
        let needed = (file_size_words as u64).saturating_add(1).saturating_mul(8);
        if needed > file_len {
            return Err(Dss7Error::Truncated {
                header_bytes: needed,
                file_len,
            });
        }
        if !(MIN_HEADER_SIZE..=file_size_words).contains(&header_size) {
            return Err(Dss7Error::BadHeaderSize(raw_hs));
        }

        // Lock area (dss7.md §6.1): words 100..102 plus, in the new scheme, the write,
        // read and PID arrays. Never read by this reader (§6.3).
        let lock_read = words[key::LOCK_READ_ARRAY_ADDRESS];
        let old_locking = lock_read == 0;
        let hash_table_address = words[key::ADD_HASH_TABLE_START];
        let mut lock_area_end = MIN_HEADER_SIZE + 3;
        if !old_locking {
            let size = words[key::LOCK_ARRAY_SIZES];
            for k in [
                key::LOCK_WRITE_ARRAY_ADDRESS,
                key::LOCK_READ_ARRAY_ADDRESS,
                key::PID_ARRAY_ADDRESS,
            ] {
                let a = words[k];
                // Only trust array pointers that sit between the header and the hash
                // table; anything else is ignored here and caught by the range checks.
                if (MIN_HEADER_SIZE..hash_table_address).contains(&a) && (0..=1024).contains(&size)
                {
                    lock_area_end = lock_area_end.max(a + size);
                }
            }
        }

        // Every structural pointer must lie in [lock_area_end, kfileSize) (dss7.md §9).
        let check = |field: &'static str, value: i64| -> Result<(), Dss7Error> {
            if value < lock_area_end.max(header_size) || value >= file_size_words {
                Err(Dss7Error::PointerOutOfRange { field, value })
            } else {
                Ok(())
            }
        };
        check("kaddHashTableStart", hash_table_address)?;
        if hash_table_address.saturating_add(max_hash) > file_size_words {
            return Err(Dss7Error::PointerOutOfRange {
                field: "kaddHashTableStart+kmaxHash",
                value: hash_table_address,
            });
        }
        let first_bin_address = words[key::ADD_FIRST_BIN];
        check("kaddFirstBin", first_bin_address)?;

        Ok(Dss7Header {
            version_string: String::from_utf8_lossy(&version)
                .trim_end_matches(['\0', ' '])
                .to_string(),
            fver: fver(&version),
            header_size,
            header_size_was_zero: raw_hs == 0,
            number_records: words[key::NUMBER_RECORDS],
            number_aliases: words[key::NUMBER_ALIASES],
            file_size_words,
            dead_words: words[key::DEAD],
            number_expansions: words[key::NUMBER_EXPANSIONS],
            number_renames: words[key::NUMBER_RENAMES],
            number_deletes: words[key::NUMBER_DELETES],
            number_alias_deletes: words[key::NUMBER_ALIAS_DELETES],
            create_time_ms: words[key::CREATE_DATE],
            last_write_time_ms: words[key::LAST_WRITE_TIME],
            max_hash,
            hash_table_address,
            bins_per_block,
            bin_size,
            first_bin_address,
            total_bins: words[key::TOTAL_BINS],
            bins_overflow: words[key::BINS_OVERFLOW],
            catalog_sort_status: words[key::CAT_SORT_STATUS],
            old_locking,
            lock_area_end,
            words,
        })
    }

    /// Words in one bin block: `kbinSize * kbinsPerBlock + 1` (the last word links to the
    /// next block; `Internal/zbinNew.c:zbinNew()`).
    pub fn block_words(&self) -> i64 {
        self.bin_size * self.bins_per_block + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fver_matches_zinquire_examples() {
        assert_eq!(fver(b"7-HA"), 70801);
        assert_eq!(fver(b"7-IU"), 70921);
        // Digit revisions sort before 7-HA.
        assert!(fver(b"7-10") < 70801);
    }
}

//! File signature and permanent-section header summary.
//!
//! Sources:
//! - Signature: `heclib_c/src/Public/zgetFileVersion.c` — the characters `ZDSS`
//!   occupy bytes 0..4 and the version string bytes 16..20.
//! - DSS7 permanent section: `heclib_c/src/Internal/zinit.c` (`zdssFileKeys`),
//!   8-byte words.
//! - DSS6 permanent section: `heclib_f/src/zinit6.f` (`KDSS`, `KNRECS`, `KSEQNO`,
//!   `KHSIZE`, `KVERS`, `KFSIZE`, `KDEAD`), 4-byte words on standard platforms.
//! - Size rules: `docs/format/dss7.md` §2 (`zwriteEOF.c`) and `docs/format/dss6.md`
//!   (`zdsskz.h` `NBSIZE`, `zrrec6.f`, `zwrec6.f`): DSS6 stores 127 words per
//!   512-byte physical record.

use std::fmt;

/// Bytes needed to decode the leading header words of either version.
pub const MIN_HEADER_BYTES: usize = 8 * (V7_LAST_WORD_WINDOW + 1);

const SIGNATURE: &[u8; 4] = b"ZDSS";
const VERSION_OFFSET: usize = 16;

// DSS7 word indexes (zinit.c, zdssFileKeys).
const V7_NUMBER_RECORDS: usize = 3;
const V7_FILE_SIZE: usize = 5;
const V7_DEAD: usize = 6;
const V7_NUMBER_DELETES: usize = 10;
const V7_LAST_WORD_WINDOW: usize = 13;

// DSS6 word indexes (zinit6.f).
const V6_NUMBER_RECORDS: usize = 1;
const V6_FILE_SIZE: usize = 5;
const V6_DEAD: usize = 6;
/// DSS6 data words per 512-byte physical record (`zdsskz.h`, `NBSIZE`).
const V6_WORDS_PER_RECORD: i64 = 127;
const V6_RECORD_BYTES: i64 = 512;

/// Byte order of the words in a DSS file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    Little,
    Big,
}

/// DSS major version, as identified by the first character of the version string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DssVersion {
    V6,
    V7,
}

impl DssVersion {
    /// Size in bytes of one file word.
    pub fn word_bytes(self) -> usize {
        match self {
            DssVersion::V6 => 4,
            DssVersion::V7 => 8,
        }
    }
}

/// Header facts that can be read without opening the hash table or catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSignature {
    pub version: DssVersion,
    /// Writer-library revision string, e.g. `6-ND` or `7-IQ`.
    pub version_string: String,
    pub byte_order: ByteOrder,
    pub number_records: i64,
    /// Header size word. DSS7: word address of the end-of-file flag (−97532).
    /// DSS6 (`KFSIZE`): next free 1-based word address, holding the flag −9630.
    pub eof_address: i64,
    pub dead_words: i64,
    /// DSS7 only.
    pub number_deletes: Option<i64>,
}

impl FileSignature {
    /// File size implied by the header, in bytes.
    ///
    /// DSS7: `(eof_address + 1) * 8`. DSS6: `ceil(KFSIZE / 127) * 512`; files from
    /// early writers may end with a 508-byte record, so see [`Self::min_file_bytes`].
    pub fn file_size_bytes(&self) -> i64 {
        match self.version {
            DssVersion::V7 => (self.eof_address + 1) * 8,
            DssVersion::V6 => {
                let records = (self.eof_address + V6_WORDS_PER_RECORD - 1) / V6_WORDS_PER_RECORD;
                records * V6_RECORD_BYTES
            }
        }
    }

    /// Smallest physical size consistent with the header; a smaller file is truncated.
    pub fn min_file_bytes(&self) -> i64 {
        match self.version {
            DssVersion::V7 => self.file_size_bytes(),
            // The final physical record can omit its 4 pad bytes (6-JE `hrap.dss`).
            DssVersion::V6 => self.file_size_bytes() - 4,
        }
    }

    /// Classify a file from its leading bytes.
    pub fn from_bytes(head: &[u8]) -> Result<Self, HeaderError> {
        if head.len() < VERSION_OFFSET + 4 {
            return Err(HeaderError::TooShort(head.len()));
        }
        let (byte_order, version_bytes) = if &head[0..4] == SIGNATURE {
            (ByteOrder::Little, [head[16], head[17], head[18], head[19]])
        } else if head[0..4].iter().rev().eq(SIGNATURE.iter()) {
            // Big-endian files reverse the characters within each 4-byte word.
            (ByteOrder::Big, [head[19], head[18], head[17], head[16]])
        } else {
            return Err(HeaderError::NotDss);
        };
        let version = match version_bytes[0] {
            b'6' => DssVersion::V6,
            b'7' => DssVersion::V7,
            other => return Err(HeaderError::UnsupportedVersion(other as char)),
        };
        let version_string = String::from_utf8_lossy(&version_bytes)
            .trim_end_matches(['\0', ' '])
            .to_string();

        let needed = match version {
            DssVersion::V6 => 4 * (V6_DEAD + 1),
            DssVersion::V7 => MIN_HEADER_BYTES,
        };
        if head.len() < needed {
            return Err(HeaderError::TooShort(head.len()));
        }
        let word = |index: usize| -> i64 {
            match version {
                DssVersion::V6 => {
                    let b: [u8; 4] = head[4 * index..4 * index + 4].try_into().unwrap();
                    i64::from(match byte_order {
                        ByteOrder::Little => i32::from_le_bytes(b),
                        ByteOrder::Big => i32::from_be_bytes(b),
                    })
                }
                DssVersion::V7 => {
                    let b: [u8; 8] = head[8 * index..8 * index + 8].try_into().unwrap();
                    match byte_order {
                        ByteOrder::Little => i64::from_le_bytes(b),
                        ByteOrder::Big => i64::from_be_bytes(b),
                    }
                }
            }
        };
        Ok(match version {
            DssVersion::V6 => FileSignature {
                version,
                version_string,
                byte_order,
                number_records: word(V6_NUMBER_RECORDS),
                eof_address: word(V6_FILE_SIZE),
                dead_words: word(V6_DEAD),
                number_deletes: None,
            },
            DssVersion::V7 => FileSignature {
                version,
                version_string,
                byte_order,
                number_records: word(V7_NUMBER_RECORDS),
                eof_address: word(V7_FILE_SIZE),
                dead_words: word(V7_DEAD),
                number_deletes: Some(word(V7_NUMBER_DELETES)),
            },
        })
    }
}

/// Why a byte buffer could not be classified as a DSS file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    TooShort(usize),
    NotDss,
    UnsupportedVersion(char),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderError::TooShort(n) => write!(f, "header too short ({n} bytes)"),
            HeaderError::NotDss => write!(f, "not a DSS file (missing ZDSS signature)"),
            HeaderError::UnsupportedVersion(c) => write!(f, "unsupported DSS version '{c}'"),
        }
    }
}

impl std::error::Error for HeaderError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn v6_header(records: i32, size_words: i32) -> Vec<u8> {
        let mut h = vec![0u8; MIN_HEADER_BYTES];
        h[0..4].copy_from_slice(b"ZDSS");
        h[4..8].copy_from_slice(&records.to_le_bytes());
        h[16..20].copy_from_slice(b"6-ND");
        h[20..24].copy_from_slice(&size_words.to_le_bytes());
        h
    }

    #[test]
    fn classifies_dss6_little_endian() {
        let sig = FileSignature::from_bytes(&v6_header(3, 1849)).unwrap();
        assert_eq!(sig.version, DssVersion::V6);
        assert_eq!(sig.version_string, "6-ND");
        assert_eq!(sig.number_records, 3);
        // ceil(1849 / 127) = 15 physical records.
        assert_eq!(sig.file_size_bytes(), 15 * 512);
        assert_eq!(sig.min_file_bytes(), 15 * 512 - 4);
    }

    #[test]
    fn dss6_size_on_record_boundary() {
        // HMS 3.3 castro.dss: KFSIZE = 56008 (dss6.md golden value).
        let sig = FileSignature::from_bytes(&v6_header(1, 127)).unwrap();
        assert_eq!(sig.file_size_bytes(), 512);
        let sig = FileSignature::from_bytes(&v6_header(1, 56008)).unwrap();
        assert_eq!(sig.file_size_bytes(), 442 * 512);
    }

    #[test]
    fn classifies_dss7_little_endian() {
        let mut h = vec![0u8; MIN_HEADER_BYTES];
        h[0..4].copy_from_slice(b"ZDSS");
        h[16..20].copy_from_slice(b"7-IQ");
        h[24..32].copy_from_slice(&42i64.to_le_bytes());
        h[40..48].copy_from_slice(&1000i64.to_le_bytes());
        let sig = FileSignature::from_bytes(&h).unwrap();
        assert_eq!(sig.version, DssVersion::V7);
        assert_eq!(sig.number_records, 42);
        assert_eq!(sig.file_size_bytes(), 8008);
        assert_eq!(sig.number_deletes, Some(0));
    }

    #[test]
    fn classifies_dss6_big_endian() {
        let mut h = vec![0u8; MIN_HEADER_BYTES];
        h[0..4].copy_from_slice(b"SSDZ");
        h[4..8].copy_from_slice(&7i32.to_be_bytes());
        h[16..20].copy_from_slice(b"DN-6");
        let sig = FileSignature::from_bytes(&h).unwrap();
        assert_eq!(sig.byte_order, ByteOrder::Big);
        assert_eq!(sig.version_string, "6-ND");
        assert_eq!(sig.number_records, 7);
    }

    #[test]
    fn rejects_non_dss_and_short_input() {
        assert_eq!(
            FileSignature::from_bytes(b"PK\x03\x04"),
            Err(HeaderError::TooShort(4))
        );
        assert_eq!(
            FileSignature::from_bytes(&[0u8; 64]),
            Err(HeaderError::NotDss)
        );
    }
}

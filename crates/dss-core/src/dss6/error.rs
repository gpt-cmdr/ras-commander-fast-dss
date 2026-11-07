//! Typed errors for the DSS6 reader.

use crate::header::HeaderError;
use std::fmt;
use std::io;

/// Why a DSS6 file could not be opened or a structure could not be read.
///
/// Catalog walks do not fail on damaged structures; they record
/// [`crate::CatalogAnomaly`] values instead. These errors cover the header (which must be
/// sound before anything else can be located), I/O failures, and point lookups.
#[derive(Debug)]
pub enum Dss6Error {
    /// Too short, missing the `ZDSS` signature, or an unknown major version.
    Header(HeaderError),
    /// A DSS file, but not version 6: the version string does not start with `6-`
    /// (`heclib_f/src/zopen6Int.f:ZOPEN6INT` line 254). DSS7 files land here.
    NotVersion6 { version: String },
    /// The words are in the opposite byte order: `KTABLE > 10000` and its byte swap is 1
    /// or 2 (`zopen6Int.f:ZOPEN6INT` lines 256-267), or the signature is reversed. No
    /// such file exists in the corpus, so the reader rejects them (dss6.md §12 O4).
    ByteSwapped { ktable: i32 },
    /// `KTABLE` is not 1 (hash table) or 2 (direct bins) (`zinit6.f` lines 73-76).
    BadTableType(i32),
    /// `KBNSIZ` outside 13..=127. HEC needs room for one entry (`zcheck6.f` lines 57-64)
    /// and holds a bin in `IPNBIN(NBSIZE+4)` (`headers/zdssiz.h`).
    BinSizeOutOfRange(i32),
    /// Another header word that makes the file unreadable (`KFSIZE`, `KHASH`, `KBNBLK`,
    /// `KAFBIN`); see dss6.md §3 "Reading the header".
    BadHeader { field: &'static str, value: i64 },
    /// The underlying source failed.
    Io(io::Error),
    /// A word range outside the file (or past `KFSIZE`).
    OutOfBounds { address: i64, words: usize },
    /// A read that would touch the lock record; the reader never reads it (dss6.md §9.1).
    LockRecord { address: i64 },
    /// A structure that is present but inconsistent.
    Corrupt {
        address: Option<i64>,
        detail: String,
    },
}

impl fmt::Display for Dss6Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dss6Error::Header(e) => write!(f, "{e}"),
            Dss6Error::NotVersion6 { version } => {
                write!(f, "not a DSS6 file (version '{version}')")
            }
            Dss6Error::ByteSwapped { ktable } => {
                write!(
                    f,
                    "byte-swapped (big-endian) DSS6 file is not supported (KTABLE {ktable})"
                )
            }
            Dss6Error::BadTableType(v) => write!(f, "invalid DSS6 KTABLE {v} (expected 1 or 2)"),
            Dss6Error::BinSizeOutOfRange(v) => {
                write!(f, "invalid DSS6 KBNSIZ {v} (expected 13..=127)")
            }
            Dss6Error::BadHeader { field, value } => {
                write!(f, "invalid DSS6 header {field} = {value}")
            }
            Dss6Error::Io(e) => write!(f, "I/O error: {e}"),
            Dss6Error::OutOfBounds { address, words } => {
                write!(f, "word range {address}+{words} is outside the file")
            }
            Dss6Error::LockRecord { address } => {
                write!(
                    f,
                    "address {address} lies in the lock record, which is never read"
                )
            }
            Dss6Error::Corrupt { address, detail } => match address {
                Some(a) => write!(f, "corrupt structure at word {a}: {detail}"),
                None => write!(f, "corrupt structure: {detail}"),
            },
        }
    }
}

impl std::error::Error for Dss6Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Dss6Error::Header(e) => Some(e),
            Dss6Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<HeaderError> for Dss6Error {
    fn from(e: HeaderError) -> Self {
        Dss6Error::Header(e)
    }
}

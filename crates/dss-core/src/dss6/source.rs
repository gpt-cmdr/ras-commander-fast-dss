//! Positioned byte sources and the DSS6 word-address map.
//!
//! Sources:
//! - 127 data words per logical record: `heclib_f/src/headers/zdsskz.h` (`NBSIZE = 127`).
//! - 512-byte physical records, record `r` at byte `(r-1)*512`, 508 bytes read per record:
//!   `heclib_f/src/zrrec6.f:ZRREC6` lines 74-112; `heclib_f/src/zwrec6.f:ZWREC6` lines 93-118.
//! - Address to (record, word): `heclib_f/src/zgetrw6.f:ZGETRW6` lines 10-11; the inverse is
//!   `heclib_f/src/zgetad6.f:ZGETAD6` line 26.
//! - Multi-word reads cross records and skip the 4 pad bytes:
//!   `heclib_f/src/zgtrec6.f:ZGTREC6` lines 81-191.
//!
//! Readers never write: [`FileSource`] opens files read-only and never takes locks.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// Data words per DSS6 logical record (`zdsskz.h`, `NBSIZE`).
pub const WORDS_PER_RECORD: i64 = 127;
/// Bytes per DSS6 physical record (`zrrec6.f`, `zwrec6.f`).
pub const RECORD_BYTES: u64 = 512;
/// Bytes of data in a physical record; the remaining 4 are pad (`zrrec6.f` reads 508).
pub const RECORD_DATA_BYTES: u64 = 508;
/// Largest word address a signed 32-bit DSS6 pointer can hold.
pub const MAX_ADDRESS: i64 = i32::MAX as i64;

/// Byte offset of 1-based word address `addr`:
/// `((addr-1) div 127) * 512 + ((addr-1) mod 127) * 4` (`zgetrw6.f:ZGETRW6` lines 10-11
/// combined with the 512-byte record of `zrrec6.f`). `None` outside `1..=i32::MAX`.
pub fn word_byte_offset(addr: i64) -> Option<u64> {
    if !(1..=MAX_ADDRESS).contains(&addr) {
        return None;
    }
    let a = (addr - 1) as u64;
    Some((a / WORDS_PER_RECORD as u64) * RECORD_BYTES + (a % WORDS_PER_RECORD as u64) * 4)
}

/// Inverse of [`word_byte_offset`]. `None` when `offset` is not word-aligned or lies in
/// the 4 pad bytes at the end of a physical record.
pub fn byte_offset_word(offset: u64) -> Option<i64> {
    let rec = offset / RECORD_BYTES;
    let within = offset % RECORD_BYTES;
    if within % 4 != 0 || within >= RECORD_DATA_BYTES {
        return None;
    }
    let addr = rec as i64 * WORDS_PER_RECORD + (within / 4) as i64 + 1;
    (addr <= MAX_ADDRESS).then_some(addr)
}

/// 1-based (record, word) of a word address (`zgetrw6.f:ZGETRW6` lines 10-11).
pub fn record_and_word(addr: i64) -> Option<(i64, i64)> {
    if !(1..=MAX_ADDRESS).contains(&addr) {
        return None;
    }
    let rec = (addr - 1) / WORDS_PER_RECORD + 1;
    Some((rec, addr - (rec - 1) * WORDS_PER_RECORD))
}

/// Number of whole words a file of `size` bytes holds, counting a short final record
/// (the 508-byte last record written by early versions, dss6.md §1.3).
pub fn physical_words(size: u64) -> i64 {
    let full = (size / RECORD_BYTES) as i64 * WORDS_PER_RECORD;
    let tail = ((size % RECORD_BYTES).min(RECORD_DATA_BYTES) / 4) as i64;
    full.saturating_add(tail)
}

/// A random-access, read-only byte source.
pub trait ReadAt {
    /// Total length in bytes.
    fn size(&self) -> u64;
    /// Fill `buf` from `offset`. Fails with `UnexpectedEof` if the range passes the end.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

impl ReadAt for [u8] {
    fn size(&self) -> u64 {
        self.len() as u64
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| eof())?;
        let end = start.checked_add(buf.len()).ok_or_else(eof)?;
        let src = self.get(start..end).ok_or_else(eof)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn size(&self) -> u64 {
        self.as_slice().size()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_exact_at(offset, buf)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn size(&self) -> u64 {
        (**self).size()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_exact_at(offset, buf)
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of source")
}

/// A file opened read-only. The size is captured at open, so a file that grows while
/// it is read is treated as its original length.
///
/// On Windows, `std` opens files with `FILE_SHARE_READ | FILE_SHARE_WRITE |
/// FILE_SHARE_DELETE`, so a file held open by HEC software can still be opened. No byte
/// locks are taken, and the DSS6 reader never reads the lock record (dss6.md §9.1).
#[derive(Debug)]
pub struct FileSource {
    file: Mutex<File>,
    size: u64,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            size,
        })
    }
}

impl ReadAt for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset.checked_add(buf.len() as u64).ok_or_else(eof)?;
        if end > self.size {
            return Err(eof());
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("file source lock poisoned"))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_map_matches_zgetrw6() {
        assert_eq!(word_byte_offset(1), Some(0));
        assert_eq!(word_byte_offset(127), Some(504));
        assert_eq!(word_byte_offset(128), Some(512));
        // HMS 3.3 castro.dss: KAFBIN = 382 is word 1 of record 4 (dss6.md §4.4).
        assert_eq!(word_byte_offset(382), Some(1536));
        assert_eq!(record_and_word(382), Some((4, 1)));
        assert_eq!(record_and_word(381), Some((3, 127)));
        assert_eq!(word_byte_offset(0), None);
        assert_eq!(word_byte_offset(-5), None);
        assert_eq!(word_byte_offset(MAX_ADDRESS + 1), None);
        // Largest address stays inside u64 arithmetic.
        assert!(word_byte_offset(MAX_ADDRESS).is_some());
    }

    #[test]
    fn inverse_map_skips_pad() {
        for addr in [1i64, 2, 126, 127, 128, 254, 255, 56008, 1_000_000] {
            let off = word_byte_offset(addr).unwrap();
            assert_eq!(byte_offset_word(off), Some(addr));
        }
        assert_eq!(byte_offset_word(508), None);
        assert_eq!(byte_offset_word(511), None);
        assert_eq!(byte_offset_word(2), None);
    }

    #[test]
    fn physical_words_counts_short_record() {
        assert_eq!(physical_words(0), 0);
        assert_eq!(physical_words(512), 127);
        assert_eq!(physical_words(1024 - 4), 254);
        assert_eq!(physical_words(1024), 254);
        assert_eq!(physical_words(515), 127);
    }

    #[test]
    fn slice_source_bounds() {
        let data = [1u8, 2, 3, 4];
        let src: &[u8] = &data;
        let mut b = [0u8; 2];
        src.read_exact_at(2, &mut b).unwrap();
        assert_eq!(b, [3, 4]);
        assert!(src.read_exact_at(3, &mut b).is_err());
        assert!(src.read_exact_at(u64::MAX, &mut b).is_err());
    }
}

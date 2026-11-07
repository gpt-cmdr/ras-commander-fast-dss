//! Word I/O on a DSS6 file through one handle.
//!
//! DSS6 stores 127 logical 4-byte words per 512-byte physical record; the last 4 bytes of
//! each record are a pad that HEC fills with leftover memory (`zwrec6.f:ZWREC6` lines
//! 107-118, dss6.md §1.1). Word address `a` (1-based) is at byte
//! `((a-1) div 127)·512 + ((a-1) mod 127)·4` (`zgetrw6.f`, `zgetad6.f`). This writer writes
//! zero pads.
//!
//! Every write is split per physical record, so a write that stays inside one record (a
//! bin entry, the header) is a single `write` call: under the writer's crash model
//! (process death, the OS keeps every completed `write`) it is all-or-nothing.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use super::WriteError;
use crate::dss6::source::{physical_words, RECORD_BYTES, WORDS_PER_RECORD};
use crate::dss7::write::overlay::Overlay;

/// Byte offset of a 1-based word address.
pub(super) fn byte_of(addr: i64) -> u64 {
    let a = (addr - 1) as u64;
    (a / WORDS_PER_RECORD as u64) * RECORD_BYTES + (a % WORDS_PER_RECORD as u64) * 4
}

/// Physical bytes of a file holding `words` (word address `a` is element `a - 1`), with
/// zero pads, whole records.
pub(super) fn words_to_file_bytes(words: &[i32]) -> Vec<u8> {
    let recs = words.len().div_ceil(WORDS_PER_RECORD as usize);
    let mut out = vec![0u8; recs * RECORD_BYTES as usize];
    for (i, w) in words.iter().enumerate() {
        let off = byte_of(i as i64 + 1) as usize;
        out[off..off + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

pub(super) struct Io<'a> {
    f: &'a File,
    /// While planning (P38 re-check): writes are held here and reads see them.
    plan: RefCell<Option<Overlay<i32>>>,
}

impl<'a> Io<'a> {
    pub fn new(f: &'a File) -> Self {
        Io {
            f,
            plan: RefCell::new(None),
        }
    }

    /// Start or end planning; see [`crate::dss7::write::overlay`].
    pub fn set_planning(&self, on: bool) {
        *self.plan.borrow_mut() = on.then(Overlay::new);
    }

    pub fn planning(&self) -> bool {
        self.plan.borrow().is_some()
    }

    pub fn len(&self) -> io::Result<u64> {
        Ok(self.f.metadata()?.len())
    }

    /// Words present in the file (a short final record counts its whole words).
    pub fn physical_words(&self) -> io::Result<i64> {
        let held = self.plan.borrow().as_ref().map_or(0, |o| o.end() - 1);
        Ok(physical_words(self.len()?).max(held))
    }

    /// Read `n` logical words at `addr`; the range must lie inside the file.
    pub fn read_words(&self, addr: i64, n: usize) -> Result<Vec<i32>, WriteError> {
        if let Some(o) = &*self.plan.borrow() {
            return o.read(addr, n, |a, m| self.read_file_words(a, m));
        }
        self.read_file_words(addr, n)
    }

    fn read_file_words(&self, addr: i64, n: usize) -> Result<Vec<i32>, WriteError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let last = addr
            .checked_add(n as i64 - 1)
            .ok_or_else(|| corrupt(addr, n))?;
        if addr < 1 || last > physical_words(self.len()?) {
            return Err(corrupt(addr, n));
        }
        let start = byte_of(addr);
        let end = byte_of(last) + 4;
        let mut raw = vec![0u8; (end - start) as usize];
        let mut f = self.f;
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut raw)?;
        let mut out = Vec::with_capacity(n);
        let mut pos = 0usize;
        let mut in_rec = ((start % RECORD_BYTES) / 4) as i64;
        while out.len() < n {
            if in_rec == WORDS_PER_RECORD {
                pos += 4;
                in_rec = 0;
                continue;
            }
            out.push(i32::from_le_bytes([
                raw[pos],
                raw[pos + 1],
                raw[pos + 2],
                raw[pos + 3],
            ]));
            pos += 4;
            in_rec += 1;
        }
        Ok(out)
    }

    /// Write `words` at `addr`, one `write` call per physical record touched. A chunk that
    /// ends at word 127 of its record also writes the record's 4-byte pad (zero), so new
    /// records are whole.
    pub fn write_words(&self, addr: i64, words: &[i32]) -> io::Result<()> {
        if let Some(o) = &mut *self.plan.borrow_mut() {
            o.put(addr, words);
            return Ok(());
        }
        let mut a = addr;
        let mut rest = words;
        let mut f = self.f;
        while !rest.is_empty() {
            let in_rec = (a - 1) % WORDS_PER_RECORD; // 0-based word in record
            let room = (WORDS_PER_RECORD - in_rec) as usize;
            let take = room.min(rest.len());
            let mut buf = Vec::with_capacity(take * 4 + 4);
            for w in &rest[..take] {
                buf.extend_from_slice(&w.to_le_bytes());
            }
            if take == room {
                buf.extend_from_slice(&[0u8; 4]);
            }
            f.seek(SeekFrom::Start(byte_of(a)))?;
            f.write_all(&buf)?;
            a += take as i64;
            rest = &rest[take..];
        }
        Ok(())
    }

    pub fn sync(&self, on: bool) -> io::Result<()> {
        if on && !self.planning() {
            self.f.sync_data()?;
        }
        Ok(())
    }
}

fn corrupt(addr: i64, n: usize) -> WriteError {
    WriteError::Corrupt(format!("{n} words at address {addr} lie outside the file"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(byte_of(1), 0);
        assert_eq!(byte_of(127), 504);
        assert_eq!(byte_of(128), 512);
        assert_eq!(byte_of(2287), 18 * 512);
        let b = words_to_file_bytes(&[7; 128]);
        assert_eq!(b.len(), 1024);
        assert_eq!(&b[508..512], &[0, 0, 0, 0]);
        assert_eq!(&b[512..516], &7i32.to_le_bytes());
    }
}

//! Detection of HEC lock words (P19a; DSS_TASK DT7; decision D10).
//!
//! Before any native write, a writer must find out whether HEC software (heclib C or
//! Fortran, as used by HEC-RAS, HEC-HMS, HEC-DSSVue, Monolith, hecdss, pydsstools) holds
//! the file. This module only **detects**: [`lock_status`] reports which HEC lock words
//! another handle holds, and [`ensure_unlocked_for_write`] turns a [`LockReport`] into a
//! refusal under an explicit [`LockPolicy`]. No writer calls it yet. Reads never call
//! it and never take or test locks (SPEC §2, D10).
//!
//! # Lock words (`docs/format/dss7.md` §6, `docs/format/dss6.md` §4.4 / §9.1)
//!
//! Paths are relative to `heclib/heclib_c/src/` (C) and `heclib/heclib_f/src/` (Fortran).
//!
//! DSS7, new scheme (8-byte words; byte offset = word address × 8, `System/zlockdss.c:
//! zlockDss()` line 83):
//!
//! | Word | Reason | Source |
//! |---|---|---|
//! | `klockAddressWord` (header word 14) | [`LockReason::PrimaryWrite`] | 8-byte standard lock `Internal/zlockActive.c` line 262; 16-byte exclusive lock line 253; unlock 16 bytes line 404 |
//! | `klockAddressWord + 1` | [`LockReason::Exclusive`] | `Internal/zopenInternal.c` line 327, `Internal/zpermCreate.c` line 274; tested `zlockActive.c` lines 276, 291 |
//! | `klockWriteArrayAddress` (word 91) + i, i < `klockArraySizes` (word 90) | [`LockReason::WriteArray`] | `Internal/zlockPassive.c` lines 202-209 |
//! | `klockReadArrayAddress` (word 92) + i, i < `klockArraySizes` | [`LockReason::ReadArray`] | `Internal/zlockPassive.c` lines 186-193; every open takes one, `Internal/zopenInternal.c` lines 375, 389 |
//!
//! DSS7, old scheme (`klockReadArrayAddress == 0`, e.g. `7-10`): fixed lock address 100
//! (`Internal/zlockingOld.c` line 127). Word 100 [`LockReason::PrimaryWrite`] (line 170),
//! word 101 [`LockReason::Request`] (lines 229-230), word 102 [`LockReason::Exclusive`]
//! (lines 184-185; an exclusive hold covers 24 bytes, line 161). There are no passive
//! arrays, so an HEC process that only has the file open cannot be seen:
//! `readers_detectable = false`.
//!
//! DSS6 (4-byte words, `IWORD(2) = 4`, `Support/bkdatw.f:BKDATW` line 36), at
//! `KLOCKB = (IREC − 1) × 512` ([`crate::dss6::Dss6Header::lock_byte_range`],
//! `zopen6Int.f:ZOPEN6INT` lines 280-287):
//!
//! | Bytes | Reason | Source |
//! |---|---|---|
//! | `KLOCKB + 0 .. 4` | [`LockReason::PrimaryWrite`] | `zmultu6.f:ZMULTU6` lines 113-114 |
//! | `KLOCKB + 4 .. 8` | [`LockReason::Request`] | `zmultu6.f` lines 154-156 |
//! | `KLOCKB + 8 .. 12` | [`LockReason::Exclusive`] | `zmultu6.f` lines 126-127, `zopen6Int.f` lines 291-293; exclusive mode locks 12 bytes, `zmultu6.f` lines 98-102 |
//! | `KLOCKB + 16 + 4k`, k = 0..99 | [`LockReason::ReadArray`] | `zlockrd.f:ZLOCKRD` lines 51-61; taken at open `zopen6Int.f` line 603, released at close `zclose6.f` line 46 |
//! | header `KFSIZE = −1357` (byte 20) | [`LockReason::StaleWriteMarker`] | `zwlock6.f:ZWLOCK6` line 69 |
//!
//! DSS6 files written before `6-K` report `readers_detectable = false` (dss6.md §4.4
//! "Pre-6-K"): the lock record may hold live bins there, and HEC software of that era
//! may not take read slots.
//!
//! # How a word is tested
//!
//! heclib locks with `_locking` on Windows (`Dss6/lockdss.c` lines 28-42,
//! `System/zlockdss.c` lines 98-127) and `lockf` on POSIX (`Dss6/lockdss.c` lines 83-96,
//! `System/zlockdss.c` lines 217-242). HEC's own test is mode 3: try a non-blocking lock
//! and release it at once.
//!
//! - **Windows:** `_locking` takes **mandatory**, exclusive `LockFile` region locks, so
//!   a read of a locked range from any other handle fails with `ERROR_LOCK_VIOLATION`
//!   (33) (P17 observed this, `evidence/P17/REPORT.md` §B.4). This module tests each
//!   word with a positioned, unbuffered read of exactly that word on its own read-only
//!   handle. It never takes a lock, so it cannot make an HEC process's own no-wait
//!   lock attempt fail (`zlockPassive.c` lines 187-194 skip any word that is busy;
//!   `zlockActive.c` lines 276-291 retry the exclusive test). It also keeps
//!   `dss-core` free of `unsafe` (AGENTS.md: `#![forbid(unsafe_code)]`); `LockFileEx`
//!   would need FFI. Limitation: a *shared* `LockFileEx` lock does not block reads and
//!   is not detected; heclib never takes shared locks. A sharing violation (32) at
//!   open is reported as [`LockReason::SharingViolation`]. HEC's exclusive open
//!   (`_SH_DENYRW`, `System/zopendisk.c` line 85) causes it, but so does **any**
//!   process that opened the file with a deny-share mode (an editor, a backup or
//!   antivirus scanner, a non-HEC tool); the reason does not identify HEC. `holder_pids` stays empty on
//!   Windows: the OS does not report lock owners.
//! - **Linux / Android:** `fcntl(F_OFD_GETLK)` with `F_WRLCK` on each word (read-only
//!   handle, no lock taken). It reports conflicting traditional (`lockf`) locks of
//!   other processes *and* of this process, with the holder's PID (`-1`, not
//!   reported, for OFD locks). If the kernel or filesystem rejects `F_OFD_GETLK` with
//!   `EINVAL`, the test falls back to `F_GETLK` (`method = "getlk"`); any other
//!   `fcntl` failure is [`LockError::Probe`] (lock state unknown).
//! - **macOS / iOS:** `fcntl(F_GETLK)`. It does not report locks held by this process.
//! - Other targets return [`LockError::Unsupported`].
//! - **Network filesystems (best effort):** SMB carries Windows byte-range locks to the
//!   server, so holders on other machines should be visible, but this is untested.
//!   NFS record locks depend on `lockd`/NFSv4 lock state and are only as reliable as
//!   the mount; a lock held by another client may not be reported.
//!
//! Every probed range lies inside the file and after the header bytes read
//! (DSS7 words 0..100, DSS6 `NPERM` words); a layout that breaks this is
//! [`LockError::BadLockLayout`], never "unlocked".
//!
//! **POSIX hazard:** closing *any* descriptor of a file drops every traditional
//! `fcntl`/`lockf` lock the calling process holds on it. [`lock_status`] opens and closes
//! its own descriptor, so calling it from a process that also runs heclib in-process
//! (pydsstools, hecdss) on the same file drops heclib's locks. Writers should test
//! through [`lock_status_of`] on their own handle.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::dss6::Dss6Header;
use crate::header::{ByteOrder, DssVersion, FileSignature, HeaderError};

/// Why a file counts as in use by HEC software.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LockReason {
    /// Primary write lock word (DSS7 `klockAddressWord`, DSS6 `KLOCKB + 0`).
    PrimaryWrite,
    /// A word of the DSS7 passive write-lock array.
    WriteArray,
    /// Exclusive-access word.
    Exclusive,
    /// Lock-request word (DSS6 `KLOCKB + 4`; DSS7 old scheme word 101).
    Request,
    /// A word of the DSS7 passive read-lock array, or a DSS6 read slot: an HEC process
    /// has the file open.
    ReadArray,
    /// DSS6 header `KFSIZE = −1357` (write in progress, or left behind by a crash).
    StaleWriteMarker,
    /// Windows refused to open the file: an HEC exclusive open (`_SH_DENYRW`).
    SharingViolation,
}

impl LockReason {
    /// Stable snake_case name (used by the Python binding).
    pub fn as_str(self) -> &'static str {
        match self {
            LockReason::PrimaryWrite => "primary_write",
            LockReason::WriteArray => "write_array",
            LockReason::Exclusive => "exclusive",
            LockReason::Request => "request",
            LockReason::ReadArray => "read_array",
            LockReason::StaleWriteMarker => "stale_write_marker",
            LockReason::SharingViolation => "sharing_violation",
        }
    }
}

impl fmt::Display for LockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of probing a file's lock words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockReport {
    /// 6 or 7; 0 when the file could not be opened (sharing violation).
    pub version: u8,
    /// Four-character writer revision (`6-QF`, `7-IU`); empty when unknown.
    pub version_string: String,
    /// Held words: `(reason, byte offset, length in bytes)`, in probe order.
    /// [`LockReason::StaleWriteMarker`] cites the header word (byte 20, 4 bytes) and
    /// [`LockReason::SharingViolation`] has byte 0, length 0.
    pub held: Vec<(LockReason, u64, u32)>,
    /// False when an HEC process that only has the file open cannot be detected:
    /// DSS7 old lock scheme, DSS6 before `6-K`, or the file could not be opened.
    pub readers_detectable: bool,
    /// PIDs of lock holders, when the OS reports them cheaply (`F_GETLK`/`F_OFD_GETLK`
    /// on POSIX). Always empty on Windows. Sorted, unique.
    pub holder_pids: Vec<u32>,
    /// Number of byte ranges tested.
    pub ranges_tested: usize,
    /// Test method: `read_probe` (Windows), `ofd_getlk` (Linux/Android), `getlk`.
    pub method: &'static str,
}

impl LockReport {
    /// True when any word is held, or the stale marker or a sharing violation was seen.
    pub fn locked(&self) -> bool {
        !self.held.is_empty()
    }

    /// Distinct reasons in first-seen order.
    pub fn reasons(&self) -> Vec<LockReason> {
        let mut out: Vec<LockReason> = Vec::new();
        for (r, _, _) in &self.held {
            if !out.contains(r) {
                out.push(*r);
            }
        }
        out
    }
}

/// Failure to determine the lock state.
#[derive(Debug)]
pub enum LockError {
    /// Opening or reading the file failed (other than a sharing violation).
    Io(io::Error),
    /// Not a DSS file, or the header is damaged.
    Header(String),
    /// Lock-area pointers in a DSS7 header are not usable.
    BadLockLayout { field: &'static str, value: i64 },
    /// No lock test is implemented for this target.
    Unsupported(&'static str),
    /// The OS lock test itself failed for one word (for example `fcntl` refused it).
    Probe {
        method: &'static str,
        byte: u64,
        source: io::Error,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Io(e) => write!(f, "I/O error while testing DSS lock words: {e}"),
            LockError::Header(m) => write!(f, "cannot read DSS header for lock test: {m}"),
            LockError::BadLockLayout { field, value } => {
                write!(f, "DSS7 lock area pointer {field} = {value} is not usable")
            }
            LockError::Unsupported(m) => write!(f, "lock detection unsupported: {m}"),
            LockError::Probe {
                method,
                byte,
                source,
            } => write!(
                f,
                "lock test ({method}) failed at byte {byte}: {source}; the lock state is unknown"
            ),
        }
    }
}

impl std::error::Error for LockError {}

impl From<io::Error> for LockError {
    fn from(e: io::Error) -> Self {
        LockError::Io(e)
    }
}

impl From<HeaderError> for LockError {
    fn from(e: HeaderError) -> Self {
        LockError::Header(e.to_string())
    }
}

// ---------------------------------------------------------------------------------
// Header-derived lock layout
// ---------------------------------------------------------------------------------

/// DSS7 header words used here (`Internal/zinit.c:zinit()` `zdssFileKeys`; dss7.md §2.2).
const W7_FILE_SIZE: usize = 5;
const W7_LOCK_ADDRESS_WORD: usize = 14;
const W7_LOCK_ARRAY_SIZES: usize = 90;
const W7_LOCK_WRITE_ARRAY: usize = 91;
const W7_LOCK_READ_ARRAY: usize = 92;
/// `DSS_MIN_FILE_HEADER_SIZE`: the permanent section is words 0..100.
const W7_HEADER_WORDS: usize = 100;
/// Old-scheme fixed lock address (`Internal/zlockingOld.c` line 127).
const OLD_LOCK_ADDRESS: u64 = 100;
/// Largest lock array accepted (HEC writes `DSS_LOCK_ARRAY_SIZE` = 25,
/// `Internal/zpermCreate.c` line 277; dss7 header parser bound).
const MAX_LOCK_ARRAY: i64 = 1024;

/// DSS6 `KFSIZE` byte offset (word index 5, `zinit6.f:ZINIT6`; dss6.md §3).
const KFSIZE_BYTE: u64 = 5 * 4;
/// DSS6 read slots (`zlockrd.f:ZLOCKRD` lines 52-53: start `KLOCKB + 4·IWORD(2)`, 100
/// slots).
const DSS6_READ_SLOTS: u64 = 100;
/// `6-K` as `KVERNO` (`zopen6Int.f:ZOPEN6INT` line 250: 60000 + 100·c3 + c4).
const DSS6_6K: i32 = 60_000 + 100 * b'K' as i32;

/// The byte ranges to test for one file, derived from its header only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockLayout {
    pub version: u8,
    pub version_string: String,
    /// `(reason, byte, length)` in probe order.
    pub ranges: Vec<(LockReason, u64, u32)>,
    pub readers_detectable: bool,
    /// DSS6 `KFSIZE = −1357` seen in the header.
    pub stale_write_marker: bool,
}

/// `addr·8 + n·8` as a byte range inside a file of `file_len` bytes, or `BadLockLayout`.
fn word_range(field: &'static str, addr: i64, n: i64, file_len: u64) -> Result<u64, LockError> {
    let bad = || LockError::BadLockLayout { field, value: addr };
    // Lock words start after the permanent section (zpermCreate.c lines 272-297
    // allocate them right after word 100), so the 800-byte header read never overlaps.
    if addr < W7_HEADER_WORDS as i64 || n < 0 {
        return Err(bad());
    }
    let start = (addr as u64).checked_mul(8).ok_or_else(bad)?;
    let end = (addr as u64)
        .checked_add(n as u64)
        .and_then(|e| e.checked_mul(8))
        .ok_or_else(bad)?;
    if end > file_len {
        return Err(bad());
    }
    Ok(start)
}

/// Lock layout of a DSS7 file from its first 800 bytes (words 0..100, never the lock
/// area itself) and the file length in bytes. Byte order follows the file signature.
/// Every lock word must lie inside the file and at or after byte 800.
pub fn dss7_layout(head: &[u8], file_len: u64) -> Result<LockLayout, LockError> {
    let sig = FileSignature::from_bytes(head)?;
    if sig.version != DssVersion::V7 {
        return Err(LockError::Header(format!(
            "not a DSS7 file (version {})",
            sig.version_string
        )));
    }
    if head.len() < W7_HEADER_WORDS * 8 {
        return Err(HeaderError::TooShort(head.len()).into());
    }
    let word = |i: usize| -> i64 {
        let b: [u8; 8] = head[8 * i..8 * i + 8].try_into().unwrap_or([0; 8]);
        match sig.byte_order {
            ByteOrder::Little => i64::from_le_bytes(b),
            ByteOrder::Big => i64::from_be_bytes(b),
        }
    };
    // kfileSize (word 5) is the EOF-flag word address (zwriteEOF.c); a file without one
    // is damaged, and lock words cannot be placed.
    let file_words = word(W7_FILE_SIZE);
    if file_words <= W7_HEADER_WORDS as i64 {
        return Err(LockError::BadLockLayout {
            field: "kfileSize",
            value: file_words,
        });
    }
    let mut ranges = Vec::new();
    let read_array = word(W7_LOCK_READ_ARRAY);
    let old = read_array == 0;
    if old {
        // Internal/zlockingOld.c: klockAddress = 100 (line 127); primary 8 bytes
        // (line 170), request word +1 (lines 229-230), exclusive word +2 (lines 184-185).
        let a = word_range("klockAddress", OLD_LOCK_ADDRESS as i64, 3, file_len)?;
        ranges.push((LockReason::PrimaryWrite, a, 8));
        ranges.push((LockReason::Request, a + 8, 8));
        ranges.push((LockReason::Exclusive, a + 16, 8));
    } else {
        let check = |field: &'static str, addr: i64, n: i64| word_range(field, addr, n, file_len);
        let size = word(W7_LOCK_ARRAY_SIZES);
        if !(1..=MAX_LOCK_ARRAY).contains(&size) {
            return Err(LockError::BadLockLayout {
                field: "klockArraySizes",
                value: size,
            });
        }
        // Primary word: zlockActive.c line 262 (8 bytes); exclusive word = +1,
        // zopenInternal.c line 327 / zpermCreate.c line 274.
        let primary = check("klockAddressWord", word(W7_LOCK_ADDRESS_WORD), 2)?;
        ranges.push((LockReason::PrimaryWrite, primary, 8));
        ranges.push((LockReason::Exclusive, primary + 8, 8));
        // Passive write array: zlockPassive.c lines 202-209 (one word per writer).
        // word_range bounded the whole array inside the file, so `+ 8·i` cannot overflow.
        let warr = check("klockWriteArrayAddress", word(W7_LOCK_WRITE_ARRAY), size)?;
        for i in 0..size as u64 {
            ranges.push((LockReason::WriteArray, warr + 8 * i, 8));
        }
        // Passive read array: zlockPassive.c lines 186-193 (one word per open).
        let rarr = check("klockReadArrayAddress", read_array, size)?;
        for i in 0..size as u64 {
            ranges.push((LockReason::ReadArray, rarr + 8 * i, 8));
        }
    }
    Ok(LockLayout {
        version: 7,
        version_string: sig.version_string,
        ranges,
        readers_detectable: !old,
        stale_write_marker: false,
    })
}

/// Lock layout of a DSS6 file from its first `NPERM` words and the file length in
/// bytes. The lock words up to the last read slot must lie inside the file.
pub fn dss6_layout(head: &[u8], file_len: u64) -> Result<LockLayout, LockError> {
    let h = Dss6Header::parse(head).map_err(|e| LockError::Header(e.to_string()))?;
    // KLOCKB: zopen6Int.f lines 280-287 (Dss6Header::lock_record). It is at least
    // record 2 (byte 512), so the NPERM-word header read never overlaps it.
    let (kb, _) = h.lock_byte_range();
    let last = kb
        .checked_add(16 + 4 * DSS6_READ_SLOTS)
        .filter(|&e| e <= file_len);
    if kb < DSS6_HEAD_BYTES as u64 || last.is_none() {
        return Err(LockError::BadLockLayout {
            field: "KLOCKB",
            value: i64::try_from(kb).unwrap_or(i64::MAX),
        });
    }
    let mut ranges = vec![
        // zmultu6.f lines 113-114: write lock, IWORD(2) = 4 bytes at KLOCKB.
        (LockReason::PrimaryWrite, kb, 4),
        // zmultu6.f lines 154-156: lock request at KLOCKB + IWORD(2).
        (LockReason::Request, kb + 4, 4),
        // zmultu6.f lines 126-127, zopen6Int.f lines 291-293: exclusive at KLOCKB + 8.
        (LockReason::Exclusive, kb + 8, 4),
    ];
    // zlockrd.f lines 51-61: read slots from KLOCKB + 4·IWORD(2), 100 of them.
    for k in 0..DSS6_READ_SLOTS {
        ranges.push((LockReason::ReadArray, kb + 16 + 4 * k, 4));
    }
    Ok(LockLayout {
        version: 6,
        version_string: h.version_string.clone(),
        ranges,
        readers_detectable: h.version_number >= DSS6_6K,
        stale_write_marker: h.is_write_locked(),
    })
}

/// Bytes read to classify a file: signature and version string (`header.rs`).
const SIG_BYTES: usize = 20;
/// DSS7 header read: words 0..100 (`DSS_MIN_FILE_HEADER_SIZE`); lock words start at 100.
const DSS7_HEAD_BYTES: usize = W7_HEADER_WORDS * 8;
/// DSS6 header read: `NPERM` = 69 words (`zrdprm6.f:ZRDPRM6` line 51). The lock record
/// can start at byte 512 (`KHASH` ≤ 57), so the read must not extend past 276 bytes.
const DSS6_HEAD_BYTES: usize = crate::dss6::HEADER_BYTES;

/// Header bytes a version needs (and the most this module ever reads before probing).
fn head_bytes(v: DssVersion) -> usize {
    match v {
        DssVersion::V7 => DSS7_HEAD_BYTES,
        DssVersion::V6 => DSS6_HEAD_BYTES,
    }
}

/// Lock layout from the leading bytes of a file (dispatch on the signature).
pub fn layout_from_head(head: &[u8], file_len: u64) -> Result<LockLayout, LockError> {
    let layout = match FileSignature::from_bytes(head)?.version {
        DssVersion::V7 => dss7_layout(head, file_len)?,
        DssVersion::V6 => dss6_layout(head, file_len)?,
    };
    // Invariant: no probed range overlaps the header bytes this module reads (a held
    // word inside the read would turn a lock into an I/O error on Windows).
    let head_end = head_bytes(FileSignature::from_bytes(head)?.version) as u64;
    if let Some(&(_, off, _)) = layout.ranges.iter().find(|r| r.1 < head_end) {
        return Err(LockError::BadLockLayout {
            field: "lock word inside header",
            value: i64::try_from(off).unwrap_or(i64::MAX),
        });
    }
    Ok(layout)
}

// ---------------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------------

/// Outcome of testing one byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Free,
    Held(Option<u32>),
}

#[cfg(windows)]
mod sys {
    use super::Probe;
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::FileExt;

    /// `ERROR_LOCK_VIOLATION`.
    const ERROR_LOCK_VIOLATION: i32 = 33;
    /// `ERROR_SHARING_VIOLATION`.
    pub const ERROR_SHARING_VIOLATION: i32 = 32;

    pub const METHOD: &str = "read_probe";

    /// Positioned read of exactly `[off, off+len)`. `_locking` regions are mandatory
    /// (`Dss6/lockdss.c` lines 28-42), so another handle's lock makes the read fail
    /// with `ERROR_LOCK_VIOLATION`. The layout keeps every range inside the file, so a
    /// short read is an error, never "free".
    pub fn probe(file: &File, off: u64, len: u32, _method: &mut &'static str) -> io::Result<Probe> {
        let mut buf = [0u8; 8];
        let want = (len as usize).min(buf.len());
        let mut done = 0;
        while done < want {
            match file.seek_read(&mut buf[done..want], off + done as u64) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "lock word past end of file",
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION) => {
                    return Ok(Probe::Held(None))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Probe::Free)
    }

    pub fn is_sharing_violation(e: &io::Error) -> bool {
        e.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod sys {
    use super::Probe;
    use nix::fcntl::{fcntl, FcntlArg};
    use nix::libc;
    use std::fs::File;
    use std::io;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const METHOD: &str = "ofd_getlk";
    #[cfg(target_vendor = "apple")]
    pub const METHOD: &str = "getlk";

    /// `F_OFD_GETLK` (Linux/Android) or `F_GETLK` with `F_WRLCK` on `[off, off+len)`:
    /// the POSIX form of heclib's mode-3 test (`lockf(F_TEST)`, `Dss6/lockdss.c` line 96).
    /// Takes no lock. On Linux, `EINVAL` from `F_OFD_GETLK` (kernel before 3.15, or a
    /// filesystem without OFD support) switches `method` to `getlk` for this and every
    /// later range; `F_GETLK` does not see this process's own locks.
    pub fn probe(file: &File, off: u64, len: u32, method: &mut &'static str) -> io::Result<Probe> {
        let start = libc::off_t::try_from(off)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        let mut fl = libc::flock {
            l_type: libc::F_WRLCK as _,
            l_whence: libc::SEEK_SET as _,
            l_start: start,
            l_len: len as libc::off_t,
            // Must be 0 on input for F_OFD_GETLK.
            l_pid: 0,
        };
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if *method == METHOD {
            let mut ofd = fl;
            match fcntl(file, FcntlArg::F_OFD_GETLK(&mut ofd)) {
                Ok(_) => return Ok(result(&ofd)),
                Err(nix::errno::Errno::EINVAL) => *method = "getlk",
                Err(e) => return Err(io::Error::from(e)),
            }
        }
        let _ = &method;
        fcntl(file, FcntlArg::F_GETLK(&mut fl)).map_err(io::Error::from)?;
        Ok(result(&fl))
    }

    fn result(fl: &libc::flock) -> Probe {
        if fl.l_type == libc::F_UNLCK as _ {
            Probe::Free
        } else {
            // l_pid is -1 for an OFD lock; then the holder is not reported.
            Probe::Held(u32::try_from(fl.l_pid).ok().filter(|&p| p > 0))
        }
    }

    pub fn is_sharing_violation(_e: &io::Error) -> bool {
        false
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
)))]
mod sys {
    use super::Probe;
    use std::fs::File;
    use std::io;

    pub const METHOD: &str = "unsupported";

    pub fn probe(
        _file: &File,
        _off: u64,
        _len: u32,
        _method: &mut &'static str,
    ) -> io::Result<Probe> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no byte-range lock test for this target",
        ))
    }

    pub fn is_sharing_violation(_e: &io::Error) -> bool {
        false
    }
}

/// Positioned, unbuffered read of `[0, want)` (shorter at EOF).
fn read_prefix(file: &File, want: usize, len: u64) -> io::Result<Vec<u8>> {
    let want = (want as u64).min(len) as usize;
    let mut buf = vec![0u8; want];
    let mut done = 0;
    while done < want {
        let n = read_at(file, &mut buf[done..], done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    buf.truncate(done);
    Ok(buf)
}

/// Read only the header bytes the file's version needs: first the 20-byte signature,
/// then 276 bytes (DSS6, `NPERM` words) or 800 bytes (DSS7, words 0..100). Neither
/// reaches a lock word: the DSS6 lock record starts at byte 512 or later and DSS7 lock
/// words at byte 800 or later (checked again in [`layout_from_head`]).
fn read_head(file: &File, len: u64) -> Result<Vec<u8>, LockError> {
    let sig = read_prefix(file, SIG_BYTES, len)?;
    let version = signature_version(&sig)?;
    Ok(read_prefix(file, head_bytes(version), len)?)
}

/// DSS version from the 20-byte signature alone: `ZDSS` then the version character
/// at byte 16 (little-endian) or 19 (big-endian, characters reversed per word), as
/// `FileSignature::from_bytes` decodes it (`header.rs`).
fn signature_version(sig: &[u8]) -> Result<DssVersion, LockError> {
    if sig.len() < SIG_BYTES {
        return Err(HeaderError::TooShort(sig.len()).into());
    }
    let c = if &sig[0..4] == b"ZDSS" {
        sig[16]
    } else if &sig[0..4] == b"SSDZ" {
        sig[19]
    } else {
        return Err(HeaderError::NotDss.into());
    };
    match c {
        b'6' => Ok(DssVersion::V6),
        b'7' => Ok(DssVersion::V7),
        other => Err(HeaderError::UnsupportedVersion(other as char).into()),
    }
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, off)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, off)
}

#[cfg(not(any(windows, unix)))]
fn read_at(_file: &File, _buf: &mut [u8], _off: u64) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positioned reads unsupported on this target",
    ))
}

/// Test every range of `layout` through `file`.
pub fn probe_layout(file: &File, layout: &LockLayout) -> Result<LockReport, LockError> {
    if sys::METHOD == "unsupported" {
        return Err(LockError::Unsupported(
            "no byte-range lock test for this target",
        ));
    }
    let mut held = Vec::new();
    let mut pids = Vec::new();
    if layout.stale_write_marker {
        // zwlock6.f line 69: KFSIZE = -1357 while write-locked (or after a crash).
        held.push((LockReason::StaleWriteMarker, KFSIZE_BYTE, 4));
    }
    let mut method = sys::METHOD;
    for &(reason, off, len) in &layout.ranges {
        let probe = sys::probe(file, off, len, &mut method).map_err(|source| LockError::Probe {
            method,
            byte: off,
            source,
        })?;
        if let Probe::Held(pid) = probe {
            held.push((reason, off, len));
            if let Some(p) = pid {
                pids.push(p);
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(LockReport {
        version: layout.version,
        version_string: layout.version_string.clone(),
        held,
        readers_detectable: layout.readers_detectable,
        holder_pids: pids,
        ranges_tested: layout.ranges.len(),
        method,
    })
}

/// Probe the lock words of the DSS file open as `file` (for example a writer's own
/// handle). Reads only the header words 0..100 (DSS7) or `NPERM` (DSS6), never the
/// lock area, and takes no locks.
pub fn lock_status_of(file: &File) -> Result<LockReport, LockError> {
    let len = file.metadata()?.len();
    let head = read_head(file, len)?;
    let layout = layout_from_head(&head, len)?;
    probe_layout(file, &layout)
}

/// Probe whether HEC software holds `path`, through a new read-only handle that takes
/// no locks and is closed before returning (see the POSIX hazard in the module docs).
pub fn lock_status(path: &Path) -> Result<LockReport, LockError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if sys::is_sharing_violation(&e) => {
            // System/zopendisk.c line 85: an exclusive HEC open uses _SH_DENYRW.
            return Ok(LockReport {
                version: 0,
                version_string: String::new(),
                held: vec![(LockReason::SharingViolation, 0, 0)],
                readers_detectable: false,
                holder_pids: Vec::new(),
                ranges_tested: 0,
                method: sys::METHOD,
            });
        }
        Err(e) => return Err(e.into()),
    };
    lock_status_of(&file)
}

// ---------------------------------------------------------------------------------
// Write guard (pure; no writer calls it yet)
// ---------------------------------------------------------------------------------

/// Writer policy. Each field maps to a review question of
/// `packets/P19_lock_detection.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockPolicy {
    /// Question 1, **decided by the maintainer 2026-10-28**: hold HEC's primary lock word
    /// (and one free DSS7 write-array word) on the writer's own handle for the whole
    /// write transaction (spec Policy 3). Default `true`. [`ensure_unlocked_for_write`]
    /// does not read it; the writer packets implement it.
    pub hold_primary_lock_during_write: bool,
    /// Question 2, **decided by the maintainer 2026-10-28: refuse**. The guard refuses
    /// when the only holders are read-array words / read slots, i.e. an HEC process has
    /// a current-scheme file open but is not writing. Such a process caches the header
    /// and may write later (dss7.md §6.2 **[DECISION]**). Default `true`. This applies to
    /// every current-scheme file, not only old ones.
    ///
    /// **Limitation.** The refusal lasts until the HEC process actually closes the file,
    /// which can be later than the user expects. HEC Monolith's `HecDss.done()` makes no
    /// native call. It only releases Monolith's file-cache reference, and the native file
    /// (with its read word) is closed by a background timer about 6 s later. Or it is
    /// closed by `HecDss.close()`, `HecDSSFileAccess.close(path, true)` or
    /// `closeAllFiles()` (P19 evidence, deviation 7). The file stays held indefinitely if
    /// that timer is disabled or another object in the JVM still uses the file. So a
    /// running HEC-HMS or HEC-DSSVue session can block writes for a while after "closing"
    /// a file, and the user may have to close that program. The writer cannot tell an
    /// idle viewer from one about to write, so it errs on the side of refusing.
    pub refuse_when_only_readers: bool,
    /// Question 3, **decided by the maintainer 2026-10-28**: on files whose open-only HEC
    /// readers cannot be detected (DSS7 old lock scheme `7-10`, DSS6 before `6-K`), do
    /// not refuse. The writer holds the lock while it writes
    /// ([`LockPolicy::hold_primary_lock_during_write`]), and the guard reports
    /// [`GuardOutcome::readers_undetected`] so the caller warns. Default `true`. Held
    /// words are still refused on these files.
    pub allow_undetectable_readers: bool,
}

impl Default for LockPolicy {
    fn default() -> Self {
        LockPolicy {
            hold_primary_lock_during_write: true,
            refuse_when_only_readers: true,
            allow_undetectable_readers: true,
        }
    }
}

/// The guard passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardOutcome {
    /// Readers could not be detected and the policy allowed the write: warn.
    pub readers_undetected: bool,
}

/// The guard refused the write (maps to Python `DssLockedError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRefusal {
    /// Reasons that caused the refusal (distinct, first-seen order).
    pub reasons: Vec<LockReason>,
    /// Refused because readers cannot be detected on this file version.
    pub readers_undetectable: bool,
    pub holder_pids: Vec<u32>,
}

impl fmt::Display for LockRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.reasons.iter().map(|r| r.as_str()).collect();
        if !names.is_empty() {
            write!(
                f,
                "DSS file is in use by HEC software ({})",
                names.join(", ")
            )?;
        } else {
            f.write_str("DSS file write refused")?;
        }
        if self.reasons.contains(&LockReason::StaleWriteMarker) {
            f.write_str(
                "; the DSS6 header carries the write-lock marker (KFSIZE = -1357), usually \
                 left by an interrupted HEC write: let HEC software repair the file (for \
                 example open and close it in HEC-DSSVue) before writing",
            )?;
        }
        if self.readers_undetectable {
            f.write_str(
                "; readers cannot be detected on this file version (DSS7 old lock scheme or \
                 DSS6 before 6-K), and the policy does not allow undetectable readers",
            )?;
        }
        if !self.holder_pids.is_empty() {
            write!(f, "; holder PIDs {:?}", self.holder_pids)?;
        }
        Ok(())
    }
}

impl std::error::Error for LockRefusal {}

/// Decide whether a write may proceed, from a [`LockReport`] and an explicit policy.
///
/// Always refused: primary/write, write-array, exclusive and request words, the DSS6
/// stale marker, a sharing violation. Read-array words / read slots refuse unless
/// [`LockPolicy::refuse_when_only_readers`] is false. Files whose readers cannot be
/// detected refuse unless [`LockPolicy::allow_undetectable_readers`] is true.
pub fn ensure_unlocked_for_write(
    report: &LockReport,
    policy: &LockPolicy,
) -> Result<GuardOutcome, LockRefusal> {
    let reasons: Vec<LockReason> = report
        .reasons()
        .into_iter()
        .filter(|r| *r != LockReason::ReadArray || policy.refuse_when_only_readers)
        .collect();
    let undetectable = !report.readers_detectable;
    let refuse_undetectable = undetectable && !policy.allow_undetectable_readers;
    if !reasons.is_empty() || refuse_undetectable {
        return Err(LockRefusal {
            reasons,
            readers_undetectable: refuse_undetectable,
            holder_pids: report.holder_pids.clone(),
        });
    }
    Ok(GuardOutcome {
        readers_undetected: undetectable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(held: &[LockReason], detectable: bool) -> LockReport {
        LockReport {
            version: 7,
            version_string: "7-IU".into(),
            held: held.iter().map(|&r| (r, 800, 8)).collect(),
            readers_detectable: detectable,
            holder_pids: vec![],
            ranges_tested: 52,
            method: "test",
        }
    }

    #[test]
    fn guard_defaults() {
        let p = LockPolicy::default();
        assert!(p.hold_primary_lock_during_write);
        assert!(p.refuse_when_only_readers);
        assert!(p.allow_undetectable_readers);
        assert_eq!(
            ensure_unlocked_for_write(&report(&[], true), &p),
            Ok(GuardOutcome {
                readers_undetected: false
            })
        );
        for r in [
            LockReason::PrimaryWrite,
            LockReason::WriteArray,
            LockReason::Exclusive,
            LockReason::Request,
            LockReason::ReadArray,
            LockReason::StaleWriteMarker,
            LockReason::SharingViolation,
        ] {
            let e = ensure_unlocked_for_write(&report(&[r], true), &p).unwrap_err();
            assert_eq!(e.reasons, vec![r]);
            assert!(e.to_string().contains(r.as_str()));
        }
    }

    #[test]
    fn guard_policy_fields() {
        let readers_ok = LockPolicy {
            refuse_when_only_readers: false,
            ..LockPolicy::default()
        };
        assert!(
            ensure_unlocked_for_write(&report(&[LockReason::ReadArray], true), &readers_ok).is_ok()
        );
        let e = ensure_unlocked_for_write(
            &report(&[LockReason::ReadArray, LockReason::PrimaryWrite], true),
            &readers_ok,
        )
        .unwrap_err();
        assert_eq!(e.reasons, vec![LockReason::PrimaryWrite]);

        // Default (maintainer 2026-10-28): undetectable readers pass with a warning flag;
        // the writer holds the lock while it writes.
        let allow = LockPolicy::default();
        assert_eq!(
            ensure_unlocked_for_write(&report(&[], false), &allow),
            Ok(GuardOutcome {
                readers_undetected: true
            })
        );
        let strict = LockPolicy {
            allow_undetectable_readers: false,
            ..LockPolicy::default()
        };
        let e = ensure_unlocked_for_write(&report(&[], false), &strict).unwrap_err();
        assert!(e.readers_undetectable && e.reasons.is_empty());
        // Undetectable readers never excuse a held word.
        assert!(ensure_unlocked_for_write(&report(&[LockReason::Request], false), &allow).is_err());
    }

    #[test]
    fn stale_marker_message() {
        let e = ensure_unlocked_for_write(
            &report(&[LockReason::StaleWriteMarker], true),
            &LockPolicy::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("HEC-DSSVue"));
    }

    fn dss7_head() -> Vec<u8> {
        let mut h = vec![0u8; 800];
        h[0..4].copy_from_slice(b"ZDSS");
        h[16..20].copy_from_slice(b"7-IU");
        put(&mut h, W7_FILE_SIZE, 10_000);
        put(&mut h, W7_LOCK_ADDRESS_WORD, 100);
        put(&mut h, W7_LOCK_ARRAY_SIZES, 25);
        put(&mut h, W7_LOCK_WRITE_ARRAY, 103);
        put(&mut h, W7_LOCK_READ_ARRAY, 128);
        h
    }

    fn put(h: &mut [u8], i: usize, v: i64) {
        h[8 * i..8 * i + 8].copy_from_slice(&v.to_le_bytes())
    }

    fn bad(r: Result<LockLayout, LockError>) -> bool {
        matches!(r, Err(LockError::BadLockLayout { .. }))
    }

    const LEN: u64 = 10_001 * 8;

    #[test]
    fn layouts_reject_bad_pointers() {
        let mut h = dss7_head();
        let l = dss7_layout(&h, LEN).unwrap();
        assert_eq!(l.ranges.len(), 52);
        assert_eq!(l.ranges[0], (LockReason::PrimaryWrite, 800, 8));
        assert_eq!(l.ranges[1], (LockReason::Exclusive, 808, 8));
        assert_eq!(l.ranges[2], (LockReason::WriteArray, 103 * 8, 8));
        assert_eq!(l.ranges[27], (LockReason::ReadArray, 128 * 8, 8));
        assert!(l.readers_detectable);
        put(&mut h, W7_LOCK_ARRAY_SIZES, 0);
        assert!(bad(dss7_layout(&h, LEN)));
        put(&mut h, W7_LOCK_ARRAY_SIZES, 25);
        // Read array runs past the real file length (word 153 > 150-word file).
        assert!(bad(dss7_layout(&h, 150 * 8)));
        put(&mut h, W7_LOCK_READ_ARRAY, 0);
        let old = dss7_layout(&h, LEN).unwrap();
        assert!(!old.readers_detectable);
        assert_eq!(
            old.ranges,
            vec![
                (LockReason::PrimaryWrite, 800, 8),
                (LockReason::Request, 808, 8),
                (LockReason::Exclusive, 816, 8)
            ]
        );
        // Old scheme also needs words 100..103 inside the file.
        assert!(bad(dss7_layout(&h, 810)));
    }

    /// Review m1: corrupt DSS7 headers must give BadLockLayout, never panic or wrap.
    #[test]
    fn corrupt_dss7_headers_do_not_overflow() {
        for (k, v) in [
            (W7_FILE_SIZE, 0),
            (W7_FILE_SIZE, -5),
            (W7_LOCK_WRITE_ARRAY, i64::MAX),
            (W7_LOCK_WRITE_ARRAY, i64::MAX - 10),
            (W7_LOCK_WRITE_ARRAY, i64::MAX / 8),
            (W7_LOCK_READ_ARRAY, i64::MAX),
            (W7_LOCK_READ_ARRAY, -1),
            (W7_LOCK_ADDRESS_WORD, i64::MAX),
            (W7_LOCK_ADDRESS_WORD, 99),
            (W7_LOCK_ADDRESS_WORD, 0),
            (W7_LOCK_ARRAY_SIZES, i64::MAX),
            (W7_LOCK_ARRAY_SIZES, -1),
        ] {
            let mut h = dss7_head();
            put(&mut h, k, v);
            // Must not panic or wrap even against an absurd file length.
            let _ = dss7_layout(&h, u64::MAX);
            assert!(bad(dss7_layout(&h, LEN)), "{k} = {v}");
        }
    }

    /// Review m2: a DSS6 file too short to hold the read slots is BadLockLayout.
    #[test]
    fn dss6_lock_record_past_eof() {
        // Public fixture header (6-YN, KHASH 8, KTABLE 1): KLOCKB = 512.
        let h = &include_bytes!("../../../fixtures/public/synthetic_python6.dss")
            [..crate::dss6::HEADER_BYTES];
        let l = dss6_layout(h, 4_096).unwrap();
        assert_eq!(l.ranges[0].1, 512);
        assert_eq!(l.ranges.last().unwrap().1, 512 + 16 + 4 * 99);
        assert!(bad(dss6_layout(h, 512 + 16 + 4 * 99 + 3)));
        assert!(dss6_layout(h, 512 + 16 + 4 * 100).is_ok());
        assert!(bad(dss6_layout(h, 600)));
    }
}

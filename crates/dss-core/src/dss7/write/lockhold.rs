//! Holding HEC's write lock for the duration of a native write (`dss7.md` §6.2;
//! `LockPolicy::hold_primary_lock_during_write`).
//!
//! HEC takes an exclusive byte-range lock on the primary lock word
//! (`Internal/zlockActive.c` line 262, 8 bytes at `klockAddressWord × 8`) and, on its
//! first write, one free word of the passive write array (`Internal/zlockPassive.c`
//! lines 202-209). Paths are relative to `heclib/heclib_c/src/`, tag `7-IU-15`.
//!
//! - **Linux / macOS:** the same words, with `fcntl(F_SETLK, F_WRLCK)` (POSIX record
//!   locks, which `lockf` uses too, so HEC sees them) through `nix`, without `unsafe`.
//! - **Windows:** `dss-core` forbids `unsafe`, and `std` has no byte-range lock, so the
//!   writer takes `File::try_lock()`, an exclusive `LockFileEx` over the whole file. It
//!   covers the primary word (an HEC `_locking` attempt fails as against an HEC writer)
//!   and, being mandatory, also blocks other handles' reads for the length of the write.

use std::fs::File;
use std::io;

/// A held write lock; released on drop (and before the handle closes).
pub(crate) struct HeldLock<'a> {
    #[allow(dead_code)]
    file: &'a File,
    /// `(byte, len)` ranges held (POSIX); empty on Windows.
    #[allow(dead_code)]
    ranges: Vec<(u64, u64)>,
    /// How the lock is held: `whole_file_lockfileex` or `fcntl_setlk`.
    pub method: &'static str,
}

/// The lock could not be taken because another handle holds an overlapping lock.
pub(crate) enum HoldError {
    Busy,
    Io(io::Error),
}

// `File::try_lock` / `File::unlock` are std 1.89. The workspace `rust-version` stays at
// 1.80 (raising it enables MSRV-gated clippy lints across older code), so the Windows
// writer lock needs Rust 1.89 or later to build.
#[cfg(windows)]
#[allow(clippy::incompatible_msrv)]
mod sys {
    use super::*;

    pub fn take<'a>(
        file: &'a File,
        _primary: u64,
        _write_array: &[u64],
    ) -> Result<HeldLock<'a>, HoldError> {
        match file.try_lock() {
            Ok(()) => Ok(HeldLock {
                file,
                ranges: Vec::new(),
                method: "whole_file_lockfileex",
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(HoldError::Busy),
            Err(std::fs::TryLockError::Error(e)) => Err(HoldError::Io(e)),
        }
    }

    pub fn release(l: &mut HeldLock<'_>) {
        let _ = l.file.unlock();
    }
}

#[cfg(unix)]
mod sys {
    use super::*;
    use nix::errno::Errno;
    use nix::fcntl::{fcntl, FcntlArg};
    use nix::libc;

    fn set(file: &File, byte: u64, len: u64, kind: libc::c_short) -> Result<(), Errno> {
        let start = libc::off_t::try_from(byte).map_err(|_| Errno::EINVAL)?;
        let fl = libc::flock {
            l_type: kind as _,
            l_whence: libc::SEEK_SET as _,
            l_start: start,
            l_len: len as libc::off_t,
            l_pid: 0,
        };
        fcntl(file, FcntlArg::F_SETLK(&fl)).map(|_| ())
    }

    pub fn take<'a>(
        file: &'a File,
        primary: u64,
        write_array: &[u64],
    ) -> Result<HeldLock<'a>, HoldError> {
        let busy = |e: Errno| matches!(e, Errno::EAGAIN | Errno::EACCES);
        match set(file, primary, 8, libc::F_WRLCK as _) {
            Ok(()) => {}
            Err(e) if busy(e) => return Err(HoldError::Busy),
            Err(e) => return Err(HoldError::Io(io::Error::from(e))),
        }
        let mut ranges = vec![(primary, 8)];
        // One free word of the passive write array (zlockPassive.c lines 202-209).
        for &w in write_array {
            match set(file, w, 8, libc::F_WRLCK as _) {
                Ok(()) => {
                    ranges.push((w, 8));
                    break;
                }
                Err(e) if busy(e) => continue,
                Err(e) => {
                    let _ = set(file, primary, 8, libc::F_UNLCK as _);
                    return Err(HoldError::Io(io::Error::from(e)));
                }
            }
        }
        Ok(HeldLock {
            file,
            ranges,
            method: "fcntl_setlk",
        })
    }

    pub fn release(l: &mut HeldLock<'_>) {
        for &(b, n) in l.ranges.iter().rev() {
            let _ = set(l.file, b, n, libc::F_UNLCK as _);
        }
        l.ranges.clear();
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::*;

    pub fn take<'a>(
        _file: &'a File,
        _primary: u64,
        _write_array: &[u64],
    ) -> Result<HeldLock<'a>, HoldError> {
        Err(HoldError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "no write lock for this target",
        )))
    }

    pub fn release(_l: &mut HeldLock<'_>) {}
}

/// Take the write lock: `primary` is the byte offset of the primary lock word,
/// `write_array` the byte offsets of the passive write-array words.
pub(crate) fn take<'a>(
    file: &'a File,
    primary: u64,
    write_array: &[u64],
) -> Result<HeldLock<'a>, HoldError> {
    sys::take(file, primary, write_array)
}

impl Drop for HeldLock<'_> {
    fn drop(&mut self) {
        sys::release(self);
    }
}

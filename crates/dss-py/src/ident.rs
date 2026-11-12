//! File identity for the held read-write handle of a `DssFile` session.
//!
//! A session keeps one native handle across writes. Before each write session, and
//! before the read view is rebuilt after a write, the identity of the held handle is
//! compared with the identity of whatever file the path names now. A mismatch (the
//! file was replaced or renamed away) or a missing path drops the held handle, and the
//! path is opened again. After each write the endpoint is checked once more: the write
//! is acknowledged only if the path named the written file both before and after it.
//! Otherwise the record is already stored in the previous file (not rolled back), the
//! handle is dropped and the call fails with `OSError` (see `NativeFile::session_write`). The checks bound the window but cannot close it; the DSS lock protects
//! contents, not the name, so namespace changes to a file a session writes must be
//! coordinated with the session.
//!
//! - **Windows:** volume serial number + 64-bit file index
//!   (`GetFileInformationByHandle`). The path side opens the file with **no data access**
//!   (`access_mode(0)`, every share mode, `FILE_FLAG_BACKUP_SEMANTICS`) so the check
//!   does not pay the on-access scan of a data open (measured during development at
//!   about 0.06 ms, against about 6.6 ms for a read open of a just-modified file).
//! - **Unix:** `st_dev` + `st_ino` (`fstat` on the handle, `stat` on the path).
//!
//! Limitation: on ReFS the 64-bit index of `GetFileInformationByHandle` is not
//! guaranteed unique (ReFS uses 128-bit ids); DSS files on ReFS are not a supported
//! configuration that this check relies on.

use std::fs::File;
use std::io;
use std::path::Path;

/// Identity of an open file: `(device or volume serial, inode or file index)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    pub dev: u64,
    pub ino: u64,
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    pub fn of_file(file: &File) -> io::Result<FileId> {
        // SAFETY: `BY_HANDLE_FILE_INFORMATION` is a plain C struct of integers, for
        // which all-zero bytes are a valid value; the handle is a live file handle
        // owned by `file` for the duration of the call; the out pointer is valid for
        // writes of one struct.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileId {
            dev: u64::from(info.dwVolumeSerialNumber),
            ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }

    pub fn of_path(path: &Path) -> io::Result<FileId> {
        // No data access: an attribute-only open does not trigger the on-access scan
        // that a read or write open of a just-modified file pays.
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        of_file(&f)
    }
}

#[cfg(unix)]
mod sys {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub fn of_file(file: &File) -> io::Result<FileId> {
        let m = file.metadata()?;
        Ok(FileId {
            dev: m.dev(),
            ino: m.ino(),
        })
    }

    pub fn of_path(path: &Path) -> io::Result<FileId> {
        let m = std::fs::metadata(path)?;
        Ok(FileId {
            dev: m.dev(),
            ino: m.ino(),
        })
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::*;

    pub fn of_file(_file: &File) -> io::Result<FileId> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity is not available on this platform",
        ))
    }

    pub fn of_path(_path: &Path) -> io::Result<FileId> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity is not available on this platform",
        ))
    }
}

pub(crate) use sys::{of_file, of_path};

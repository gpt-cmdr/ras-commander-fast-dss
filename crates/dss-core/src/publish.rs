//! Creating a file beside its destination and publishing it there without replacing
//! anything.
//!
//! Every new file the engine makes for a caller-named path (`create_dss7`, `create_dss6`,
//! the copy-out of `repair_dss7`) is built in full under a temporary name in the same
//! directory and then given its final name by an operation that fails if the name exists:
//!
//! 1. **Own the temporary file.** [`OwnedTemp::create_beside`] picks a unique name
//!    (`.<name>.<pid>.<nanos>.<seq>.<tag>`) and creates it exclusively (`create_new`). A
//!    name that already exists is never opened, written or removed; the next name is tried.
//!    Only a file this call created is ever removed.
//! 2. **Publish without replacing.** [`OwnedTemp::publish`] gives the complete file its
//!    final name by a hard link, which fails if the name exists (`EEXIST`), or else, where
//!    hard links are not available, by an atomic no-replace rename (glibc Linux
//!    `renameat2(RENAME_NOREPLACE)`). The destination therefore names either nothing or
//!    the complete file. After a hard link the temporary name is removed.
//! 3. **Refuse rather than weaken.** A hard-link failure that may be transient
//!    (interrupted, busy, a Windows sharing or lock violation) is retried a few times.
//!    Any other failure, when no no-replace rename succeeds either, is refused with
//!    [`PublishRefusal`] (`WriteError::Unpublishable`, Python `DssPublishError`), which
//!    names the real link error. The destination is never created or written directly.
//!
//! Under process death (the crash model of `docs/format/dss7.md`) a kill at any point
//! leaves the destination absent or complete. A kill after the link and before the
//! temporary name is removed leaves both names on the one complete file; the temporary
//! name can be deleted. `crates/dss-core/tests/publish_kill.rs` kills a real process at
//! every [`PublishPoint`]. No claim is made for power loss: the parent directory is not
//! flushed.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The boundaries of creating and publishing a file, where the test hook
/// ([`set_publish_hook`]) is called. Not a supported API.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishPoint {
    /// The temporary file was created (empty) and is owned by this call.
    Created,
    /// The caller has written its content (and flushed it, in a flushing mode).
    Written,
    /// Immediately before the hard link (or no-replace rename) to the destination.
    BeforePublish,
    /// The destination names the complete file; the temporary name still exists after a
    /// hard link.
    Published,
    /// The temporary name was removed; the call is about to return.
    Cleaned,
}

/// Killed-process and race test hook, not a supported API: called with the point and
/// the temporary file's path. Per thread, so in-process tests do not see each other.
#[doc(hidden)]
pub type PublishHook = fn(PublishPoint, &Path);

thread_local! {
    static HOOK: std::cell::Cell<Option<PublishHook>> = const { std::cell::Cell::new(None) };
}

/// Install (or with `None` remove) this thread's [`PublishHook`] (tests only).
#[doc(hidden)]
pub fn set_publish_hook(hook: Option<PublishHook>) {
    HOOK.with(|h| h.set(hook));
}

pub(crate) fn hook(point: PublishPoint, tmp: &Path) {
    if let Some(h) = HOOK.with(|h| h.get()) {
        h(point, tmp);
    }
}

/// How many fresh names [`OwnedTemp::create_beside`] tries before giving up.
const NAME_TRIES: usize = 64;

/// Longest part of the destination name kept in a temporary name, in bytes, so that the
/// temporary name stays within the usual 255-byte limit for a destination name that fits.
const NAME_PART_MAX: usize = 120;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary file this call created. Dropping it removes it unless it was kept or
/// renamed away.
#[derive(Debug)]
pub(crate) struct OwnedTemp {
    path: PathBuf,
    keep: bool,
}

/// What [`OwnedTemp::publish`] did.
#[derive(Debug)]
pub(crate) enum Published {
    /// The destination now names the complete file.
    Done,
    /// The destination already existed; nothing was replaced, and the temporary file was
    /// removed.
    Exists,
    /// The hard link failed (after retrying transient errors) and no no-replace rename
    /// succeeded: nothing was published and `dest` was not touched. The temporary file is
    /// handed back for the caller to keep or drop.
    Refused(OwnedTemp, io::Error, RenameAttempt),
}

/// What the no-replace rename did after the hard link failed.
#[derive(Debug)]
pub enum RenameAttempt {
    /// No atomic no-replace rename is available on this platform or filesystem.
    Unavailable,
    /// It was available and failed.
    Failed(io::Error),
}

/// A new file that could not be published without risk (Python `DssPublishError`): the
/// hard link to the destination failed and no atomic no-replace rename succeeded. The
/// destination was not created. Writing it directly could leave an incomplete file there
/// if the process died, so the engine refuses instead.
#[derive(Debug)]
pub struct PublishRefusal {
    /// The destination, which this call did not create.
    pub dest: PathBuf,
    /// The hard link's error (the last one, after retries of transient errors).
    pub link_error: io::Error,
    /// What the no-replace rename did.
    pub rename: RenameAttempt,
    /// The complete temporary file, when it was kept (repair keeps its verified copy;
    /// create removes its temporary file).
    pub kept: Option<PathBuf>,
}

impl std::fmt::Display for PublishRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot publish {} safely: the hard link failed ({})",
            self.dest.display(),
            self.link_error
        )?;
        match &self.rename {
            RenameAttempt::Unavailable => {
                write!(f, " and no atomic no-replace rename is available here")?
            }
            RenameAttempt::Failed(e) => write!(f, " and the no-replace rename failed ({e})")?,
        }
        write!(
            f,
            "; the destination was not created (writing it directly could leave an \
             incomplete file if the process died)"
        )?;
        if let Some(k) = &self.kept {
            write!(f, "; the complete file is kept at {}", k.display())?;
        }
        write!(
            f,
            ". If this filesystem has no hard links (FAT, exFAT, some network shares), \
             create the file on one that does (such as NTFS, APFS, HFS+, ext4 or XFS) and copy it \
             here; otherwise correct the cause of the link error and retry"
        )
    }
}

impl std::error::Error for PublishRefusal {}

/// Test hook (not a supported API): force publication primitives to fail on this thread.
/// Compiled only with the `test-hooks` feature, which no release build enables.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishFault {
    /// Every hard link fails with this error kind.
    LinkKind(io::ErrorKind),
    /// Every hard link fails with this raw OS error (for example Windows 1142, too many
    /// links, or 5, access denied).
    LinkRawOs(i32),
    /// The first `n` hard links fail with `ErrorKind::Interrupted`; later ones are real.
    LinkTransient(u32),
    /// The no-replace rename reports that it is unavailable.
    RenameUnavailable,
    /// The no-replace rename fails with this error kind.
    RenameKind(io::ErrorKind),
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static FAULTS: std::cell::RefCell<Vec<PublishFault>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static LINK_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Install this thread's [`PublishFault`]s (tests only); an empty list removes them.
/// Installing a non-empty list also restarts [`link_attempts`]; removing them keeps it,
/// so a test can read it afterwards.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn set_publish_faults(faults: &[PublishFault]) {
    FAULTS.with(|f| *f.borrow_mut() = faults.to_vec());
    if !faults.is_empty() {
        LINK_CALLS.with(|c| c.set(0));
    }
}

#[cfg(feature = "test-hooks")]
fn faults() -> Vec<PublishFault> {
    FAULTS.with(|f| f.borrow().clone())
}

/// Hard links attempted on this thread since the last [`set_publish_faults`] (tests only).
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn link_attempts() -> u32 {
    LINK_CALLS.with(|c| c.get())
}

/// Attempts at a hard link whose error may be transient.
const LINK_TRIES: u32 = 5;

/// An error worth retrying: interrupted or busy, or on Windows a sharing or lock
/// violation (another process, such as a virus scanner, briefly holding the file).
fn transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::ResourceBusy
    ) || (cfg!(windows) && matches!(e.raw_os_error(), Some(32) | Some(33)))
}

fn hard_link(tmp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(feature = "test-hooks")]
    {
        let n = LINK_CALLS.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        for f in faults() {
            match f {
                PublishFault::LinkKind(k) => {
                    return Err(io::Error::new(k, "injected hard-link fault"))
                }
                PublishFault::LinkRawOs(code) => return Err(io::Error::from_raw_os_error(code)),
                PublishFault::LinkTransient(m) if n <= m => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "injected transient hard-link fault",
                    ))
                }
                _ => {}
            }
        }
    }
    std::fs::hard_link(tmp, dest)
}

fn rename_noreplace_or_fault(tmp: &Path, dest: &Path) -> io::Result<bool> {
    #[cfg(feature = "test-hooks")]
    for f in faults() {
        match f {
            PublishFault::RenameUnavailable => return Ok(false),
            PublishFault::RenameKind(k) => return Err(io::Error::new(k, "injected rename fault")),
            _ => {}
        }
    }
    rename_noreplace(tmp, dest)
}

impl OwnedTemp {
    /// Create a new, empty temporary file beside `dest` (same directory), exclusively.
    pub(crate) fn create_beside(dest: &Path, tag: &str) -> io::Result<(OwnedTemp, File)> {
        let pid = std::process::id();
        Self::create_first_free((0..NAME_TRIES).map(|_| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            temp_name(dest, &format!("{pid}.{nanos}.{seq}.{tag}"))
        }))
    }

    /// The first of `candidates` that can be created exclusively. A candidate that exists
    /// is skipped and left alone.
    fn create_first_free(
        candidates: impl IntoIterator<Item = PathBuf>,
    ) -> io::Result<(OwnedTemp, File)> {
        let mut last = None;
        for path in candidates {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => {
                    let t = OwnedTemp { path, keep: false };
                    hook(PublishPoint::Created, &t.path);
                    return Ok((t, f));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other(format!(
            "no free temporary name beside the destination after {NAME_TRIES} tries (last: {})",
            last.map(|e| e.to_string()).unwrap_or_default()
        )))
    }

    /// The caller has written (and in a flushing mode flushed) the content.
    pub(crate) fn written(&self) {
        hook(PublishPoint::Written, &self.path);
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the file when this is dropped (the caller hands its path to the user).
    pub(crate) fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }

    /// Give the complete temporary file the name `dest`, never replacing an existing file.
    /// The caller has written and, in a flushing mode, flushed it, and closed every handle
    /// that must not stay open on the published file.
    pub(crate) fn publish(mut self, dest: &Path) -> io::Result<Published> {
        hook(PublishPoint::BeforePublish, &self.path);
        let mut link = match crate::dss7::write::copy_out_fault_is(&["hard_link", "hard_link_race"])
        {
            Some(step) => {
                if step == "hard_link_race" {
                    std::fs::write(dest, b"created by another process")?;
                }
                Err(io::Error::other(format!("injected fault at {step}")))
            }
            None => hard_link(&self.path, dest),
        };
        let mut tries = 1;
        while matches!(&link, Err(e) if transient(e)) && tries < LINK_TRIES {
            std::thread::sleep(std::time::Duration::from_millis(25 * u64::from(tries)));
            tries += 1;
            link = hard_link(&self.path, dest);
        }
        match link {
            Ok(()) => {
                hook(PublishPoint::Published, &self.path);
                let removed = std::fs::remove_file(&self.path);
                self.keep = true; // removed (or, if removal failed, left for the caller to see)
                if removed.is_ok() {
                    hook(PublishPoint::Cleaned, &self.path);
                }
                Ok(Published::Done)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Exists),
            // The no-replace rename is as safe as the link (atomic, never replaces), so it is
            // tried after any other link failure; nothing weaker ever is.
            Err(link_err) => match rename_noreplace_or_fault(&self.path, dest) {
                Ok(true) => {
                    self.keep = true; // renamed: the temporary name no longer exists
                    hook(PublishPoint::Published, &self.path);
                    hook(PublishPoint::Cleaned, &self.path);
                    Ok(Published::Done)
                }
                Ok(false) => Ok(Published::Refused(
                    self,
                    link_err,
                    RenameAttempt::Unavailable,
                )),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Published::Exists),
                Err(e) => Ok(Published::Refused(self, link_err, RenameAttempt::Failed(e))),
            },
        }
    }
}

impl Drop for OwnedTemp {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create the file `dest` holding exactly `bytes`, never replacing anything: written in
/// full (and flushed when `flush`) under a temporary name, then published (see the module
/// documentation). An existing `dest` is `WriteError::AlreadyExists`; a publication that
/// cannot be done safely is `WriteError::Unpublishable`, with the temporary file removed
/// and `dest` not created. `dest` is never opened or written directly.
pub(crate) fn create_file(
    dest: &Path,
    tag: &str,
    bytes: &[u8],
    flush: bool,
) -> Result<(), crate::dss7::write::WriteError> {
    use crate::dss7::write::WriteError;
    use std::io::Write as _;
    if exists(dest) {
        return Err(WriteError::AlreadyExists(dest.to_path_buf()));
    }
    let (t, mut f) = OwnedTemp::create_beside(dest, tag)?;
    f.write_all(bytes)?;
    if flush {
        crate::dss7::write::count_flush();
        f.sync_all()?;
    }
    drop(f);
    t.written();
    match t.publish(dest)? {
        Published::Done => Ok(()),
        Published::Exists => Err(WriteError::AlreadyExists(dest.to_path_buf())),
        Published::Refused(t, link_error, rename) => {
            drop(t); // removes the temporary file: an empty new file is cheap to make again
            Err(WriteError::Unpublishable(Box::new(PublishRefusal {
                dest: dest.to_path_buf(),
                link_error,
                rename,
                kept: None,
            })))
        }
    }
}

/// `.<dest name>.<suffix>` beside `dest`; the destination name is shortened (at a
/// character boundary) so the result stays short enough for most filesystems.
fn temp_name(dest: &Path, suffix: &str) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dss".into());
    let mut end = name.len().min(NAME_PART_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    dest.with_file_name(format!(".{}.{suffix}", &name[..end]))
}

/// Whether `path` names anything, a dangling symbolic link included.
pub(crate) fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Rename `tmp` to `dest` atomically, never replacing an existing file.
/// `Ok(true)`: renamed; `Ok(false)`: no such primitive here; an existing `dest` is
/// `ErrorKind::AlreadyExists`.
// nix exposes renameat2 only for glibc targets; musl and macOS rely on the hard link.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_noreplace(tmp: &Path, dest: &Path) -> io::Result<bool> {
    use nix::errno::Errno;
    use nix::fcntl::{renameat2, RenameFlags, AT_FDCWD};
    match renameat2(AT_FDCWD, tmp, AT_FDCWD, dest, RenameFlags::RENAME_NOREPLACE) {
        Ok(()) => Ok(true),
        Err(Errno::EEXIST) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(Errno::EINVAL) | Err(Errno::ENOSYS) | Err(Errno::EOPNOTSUPP) => Ok(false),
        Err(e) => Err(io::Error::from(e)),
    }
}

/// No atomic no-replace rename is available to this crate here: Windows `MoveFileExW`
/// and macOS `renameatx_np(RENAME_EXCL)` would need `unsafe` FFI, which `dss-core`
/// forbids.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn rename_noreplace(_tmp: &Path, _dest: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dss_publish_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A candidate name that already exists is skipped and left byte-identical; only
    /// the file this call created is removed when it is dropped.
    #[test]
    fn an_existing_candidate_is_skipped_and_kept() {
        let d = dir("skip");
        let (a, b) = (d.join(".x.a"), d.join(".x.b"));
        std::fs::write(&a, b"not ours").unwrap();
        let (t, _f) = OwnedTemp::create_first_free([a.clone(), b.clone()]).unwrap();
        assert_eq!(t.path(), b.as_path());
        drop(_f);
        drop(t);
        assert_eq!(std::fs::read(&a).unwrap(), b"not ours");
        assert!(!b.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// When every candidate exists, the call fails and removes nothing.
    #[test]
    fn all_candidates_taken_fails_and_removes_nothing() {
        let d = dir("taken");
        let names: Vec<_> = (0..3).map(|i| d.join(format!(".x.{i}"))).collect();
        for n in &names {
            std::fs::write(n, b"not ours").unwrap();
        }
        let e = OwnedTemp::create_first_free(names.clone()).unwrap_err();
        assert_ne!(
            e.kind(),
            io::ErrorKind::AlreadyExists,
            "must not read as 'destination exists'"
        );
        for n in &names {
            assert_eq!(std::fs::read(n).unwrap(), b"not ours");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Publication never replaces: an existing destination is kept and the temporary file
    /// removed.
    #[test]
    fn publish_refuses_an_existing_destination() {
        let d = dir("exists");
        let dest = d.join("out.dss");
        std::fs::write(&dest, b"theirs").unwrap();
        let (t, mut f) = OwnedTemp::create_beside(&dest, "test").unwrap();
        std::io::Write::write_all(&mut f, b"ours").unwrap();
        drop(f);
        let tmp = t.path().to_path_buf();
        assert!(matches!(t.publish(&dest).unwrap(), Published::Exists));
        assert_eq!(std::fs::read(&dest).unwrap(), b"theirs");
        assert!(!tmp.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn temp_names_are_unique_and_short() {
        let d = dir("names");
        let long = "é".repeat(120) + ".dss"; // 244 bytes
        let dest = d.join(&long);
        let (t1, _a) = OwnedTemp::create_beside(&dest, "t").unwrap();
        let (t2, _b) = OwnedTemp::create_beside(&dest, "t").unwrap();
        assert_ne!(t1.path(), t2.path());
        let n = t1.path().file_name().unwrap().to_string_lossy().len();
        assert!(n <= 200, "{n}");
        drop((_a, _b, t1, t2));
        let _ = std::fs::remove_dir_all(&d);
    }
}

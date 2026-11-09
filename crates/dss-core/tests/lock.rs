//! P19a: HEC lock-word detection (`dss_core::lock`), CI-safe.
//!
//! A child process (this test binary, re-run with `DSS_LOCK_HOLDER_FILE` set) takes the
//! same byte-range locks heclib takes — `LockFile` on Windows (what `_locking` calls,
//! `heclib_c/src/Dss6/lockdss.c` lines 28-42) and `fcntl(F_SETLK)` record locks on POSIX
//! (what `lockf` uses, lines 83-96) — on a temporary copy of a public fixture. The parent
//! asserts the exact reason and byte offset of each word class, that release clears it,
//! that reads work while every word is held, and that nothing is left after the child
//! exits. POSIX record locks never conflict within one process, hence the child.

#![cfg(not(target_arch = "wasm32"))]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use dss_core::dss6::{CatalogOptions as Catalog6, Dss6File};
use dss_core::dss7::Dss7File;
use dss_core::lock::{ensure_unlocked_for_write, lock_status, LockPolicy, LockReason, LockReport};

const HOLDER_ENV: &str = "DSS_LOCK_HOLDER_FILE";
const HOLDER_DENY_ENV: &str = "DSS_LOCK_HOLDER_DENY_SHARE";
const TIMEOUT: Duration = Duration::from_secs(30);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

/// Temporary copy of a public fixture, removed on drop.
struct TempCopy {
    dir: PathBuf,
    path: PathBuf,
}

impl TempCopy {
    fn new(name: &str) -> TempCopy {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dss_p19_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::copy(fixture(name), &path).unwrap();
        TempCopy { dir, path }
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Ranges = Vec<(LockReason, u64, u32)>;

/// Expected lock words, decoded here independently of `dss_core::lock`
/// (dss7.md §6.1; dss6.md §4.4/§9.1). Returns the ranges and `readers_detectable`.
fn expected(path: &Path) -> (Ranges, bool) {
    use LockReason::*;
    let b = std::fs::read(path).unwrap();
    if b[16] == b'7' {
        let w = |i: usize| i64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap()) as u64;
        if w(92) == 0 {
            // Old scheme: words 100, 101, 102 (zlockingOld.c).
            return (
                vec![
                    (PrimaryWrite, 800, 8),
                    (Request, 808, 8),
                    (Exclusive, 816, 8),
                ],
                false,
            );
        }
        let mut r = vec![
            (PrimaryWrite, w(14) * 8, 8),
            (Exclusive, (w(14) + 1) * 8, 8),
        ];
        r.extend((0..w(90)).map(|i| (WriteArray, (w(91) + i) * 8, 8)));
        r.extend((0..w(90)).map(|i| (ReadArray, (w(92) + i) * 8, 8)));
        (r, true)
    } else {
        // KLOCKB from KHASH (word 14) and KTABLE (word 15), zopen6Int.f lines 280-287:
        // address KHASH+NPERM+1 (or NPERM+1), 127 data words per 512-byte record,
        // rounded up to the next record unless it is word 1 of a record.
        let w = |i: usize| i32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap()) as u64;
        let size = if w(15) == 1 { w(14) + 70 } else { 70 };
        let mut rec = (size - 1) / 127 + 1;
        if (size - 1) % 127 != 0 {
            rec += 1;
        }
        let kb = (rec - 1) * 512;
        let mut r = vec![
            (PrimaryWrite, kb, 4),
            (Request, kb + 4, 4),
            (Exclusive, kb + 8, 4),
        ];
        r.extend((0..100).map(|k| (ReadArray, kb + 16 + 4 * k, 4)));
        // KVERNO = 60000 + 100*c3 + c4 (zopen6Int.f line 250); 6-K and later detect readers.
        let vers = 100 * u32::from(b[18]) + u32::from(b[19]);
        (r, vers >= 100 * u32::from(b'K'))
    }
}

// ---------------------------------------------------------------------------------
// Holder child
// ---------------------------------------------------------------------------------

struct Holder {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl Holder {
    fn spawn(path: &Path, deny_share: bool) -> Holder {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["holder_entry", "--exact", "--nocapture", "--test-threads=1"])
            .env(HOLDER_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if deny_share {
            cmd.env(HOLDER_DENY_ENV, "1");
        }
        let mut child = cmd.spawn().expect("spawn holder");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest may print "test holder_entry ... " on the same line.
                if let Some(i) = line.find("HOLDER ") {
                    if tx.send(line[i + 7..].to_string()).is_err() {
                        break;
                    }
                }
            }
        });
        let stdin = child.stdin.take();
        let mut h = Holder { child, stdin, rx };
        assert_eq!(h.reply(), "READY");
        h
    }

    fn reply(&mut self) -> String {
        self.rx
            .recv_timeout(TIMEOUT)
            .expect("holder did not answer in time")
    }

    fn cmd(&mut self, c: &str) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{c}").unwrap();
        s.flush().unwrap();
        let r = self.reply();
        assert_eq!(r, "OK", "holder command {c:?}");
    }

    fn lock(&mut self, off: u64, len: u32) {
        self.cmd(&format!("L {off} {len}"));
    }

    fn unlock(&mut self, off: u64, len: u32) {
        self.cmd(&format!("U {off} {len}"));
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Close the child's stdin (it exits without unlocking) and wait for it.
    fn exit(mut self) {
        drop(self.stdin.take());
        let end = Instant::now() + TIMEOUT;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                assert!(st.success(), "holder exit status {st:?}");
                return;
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                panic!("holder did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(windows)]
mod holder_sys {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    // kernel32 LockFile/UnlockFile: what the CRT's _locking calls. Test-only FFI;
    // dss-core itself stays free of unsafe code.
    #[link(name = "kernel32")]
    extern "system" {
        fn LockFile(h: *mut core::ffi::c_void, lo: u32, hi: u32, nlo: u32, nhi: u32) -> i32;
        fn UnlockFile(h: *mut core::ffi::c_void, lo: u32, hi: u32, nlo: u32, nhi: u32) -> i32;
    }

    pub fn open(path: &str, deny: bool) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true);
        if deny {
            // _SH_DENYRW (heclib_c/src/System/zopendisk.c line 85).
            o.share_mode(0);
        }
        o.open(path).unwrap()
    }

    pub fn set(f: &File, off: u64, len: u32, lock: bool) -> bool {
        let h = f.as_raw_handle();
        let (lo, hi) = (off as u32, (off >> 32) as u32);
        // SAFETY: valid open handle; plain integer arguments.
        unsafe {
            if lock {
                LockFile(h, lo, hi, len, 0) != 0
            } else {
                UnlockFile(h, lo, hi, len, 0) != 0
            }
        }
    }
}

#[cfg(unix)]
mod holder_sys {
    use nix::fcntl::{fcntl, FcntlArg};
    use nix::libc;
    use std::fs::File;

    pub fn open(path: &str, _deny: bool) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    /// Process-associated record lock, as `lockf(F_TLOCK)` takes.
    pub fn set(f: &File, off: u64, len: u32, lock: bool) -> bool {
        let fl = libc::flock {
            l_type: if lock { libc::F_WRLCK } else { libc::F_UNLCK } as _,
            l_whence: libc::SEEK_SET as _,
            l_start: off as libc::off_t,
            l_len: len as libc::off_t,
            l_pid: 0,
        };
        fcntl(f, FcntlArg::F_SETLK(&fl)).is_ok()
    }
}

/// Child entry point; a no-op unless run as a holder.
#[test]
fn holder_entry() {
    let Ok(path) = std::env::var(HOLDER_ENV) else {
        return;
    };
    let deny = std::env::var(HOLDER_DENY_ENV).is_ok();
    let file = holder_sys::open(&path, deny);
    let out = std::io::stdout();
    let say = |s: &str| {
        let mut o = out.lock();
        writeln!(o, "HOLDER {s}").unwrap();
        o.flush().unwrap();
    };
    say("READY");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            [op @ ("L" | "U"), off, len] => {
                let ok = holder_sys::set(
                    &file,
                    off.parse().unwrap(),
                    len.parse().unwrap(),
                    *op == "L",
                );
                say(if ok { "OK" } else { "ERR lock call failed" });
            }
            _ => say("ERR bad command"),
        }
    }
    // stdin closed: exit with the locks still held (the OS releases them).
}

// ---------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------

fn status(p: &Path) -> LockReport {
    lock_status(p).unwrap()
}

fn assert_clean(p: &Path) {
    let s = status(p);
    assert!(!s.locked(), "{s:?}");
}

/// After a holder exits the OS may take a moment to drop its locks: poll briefly.
fn assert_clean_soon(p: &Path) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let s = status(p);
        if !s.locked() {
            return;
        }
        assert!(
            Instant::now() < end,
            "still locked after holder exit: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read the whole catalog and every time series through the native reader.
fn read_everything(p: &Path) -> usize {
    let mut sig = [0u8; 20];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(p).unwrap();
        f.read_exact(&mut sig).unwrap();
    }
    if sig[16] == b'7' {
        let mut f = Dss7File::open(p).unwrap();
        let cat = f.catalog().unwrap();
        let mut n = 0;
        for e in cat.catalog.live() {
            let rec = f.lookup(&e.pathname).unwrap().unwrap();
            if (100..200).contains(&e.record_type) {
                f.read_ts_record(&rec).unwrap();
                n += 1;
            }
        }
        n
    } else {
        let f = Dss6File::open(p).unwrap();
        let walk = f.walk_bins(&Catalog6::default()).unwrap();
        let mut n = 0;
        for e in walk.live() {
            assert!(f.lookup(&e.pathname).unwrap().is_some());
            if (100..200).contains(&e.record_type) {
                f.read_ts(&e.pathname).unwrap();
                n += 1;
            }
        }
        n
    }
}

#[test]
fn public_fixtures_have_no_false_positives() {
    let dir = fixture("");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) != Some("dss") {
            continue;
        }
        let t = Instant::now();
        let s = status(&p);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        eprintln!(
            "{}: v{} {} ranges={} readers_detectable={} {:.2} ms",
            p.file_name().unwrap().to_string_lossy(),
            s.version,
            s.version_string,
            s.ranges_tested,
            s.readers_detectable,
            ms
        );
        assert!(!s.locked(), "{}: {s:?}", p.display());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // sample7.dss is 7-10, the old DSS7 lock scheme (zlockingOld.c).
        assert_eq!(s.readers_detectable, name != "sample7.dss", "{name}");
        n += 1;
    }
    assert!(n >= 15, "only {n} public fixtures");
}

/// Lock and release every expected word one at a time; returns the count.
fn each_word(h: &mut Holder, p: &Path, ranges: &Ranges) -> usize {
    assert_eq!(status(p).ranges_tested, ranges.len());
    for &(reason, off, len) in ranges {
        h.lock(off, len);
        let s = status(p);
        assert_eq!(s.held, vec![(reason, off, len)], "{reason} at {off}");
        #[cfg(unix)]
        assert_eq!(s.holder_pids, vec![h.pid()]);
        #[cfg(windows)]
        assert!(s.holder_pids.is_empty());
        assert!(ensure_unlocked_for_write(&s, &LockPolicy::default()).is_err());
        h.unlock(off, len);
        assert_clean(p);
    }
    ranges.len()
}

fn hold_all_then_exit(mut h: Holder, p: &Path, ranges: &Ranges) {
    for &(_, off, len) in ranges {
        h.lock(off, len);
    }
    let s = status(p);
    assert_eq!(&s.held, ranges);
    // Reads are unaffected (SPEC §2): catalog and every TS record.
    let n = read_everything(p);
    eprintln!(
        "read {n} TS records while all {} words were held",
        s.held.len()
    );
    h.exit();
    assert_clean_soon(p);
}

#[test]
fn dss7_new_scheme_every_word() {
    let t = TempCopy::new("synthetic_python7.dss");
    let before = std::fs::read(&t.path).unwrap();
    let (l, detectable) = expected(&t.path);
    assert!(detectable && status(&t.path).readers_detectable);
    // 7-IU: primary + exclusive + 25 write + 25 read words.
    assert_eq!(l.len(), 52);
    let count = |r| l.iter().filter(|x| x.0 == r).count();
    assert_eq!(count(LockReason::WriteArray), 25);
    assert_eq!(count(LockReason::ReadArray), 25);
    let mut h = Holder::spawn(&t.path, false);
    assert_clean(&t.path);
    assert_eq!(each_word(&mut h, &t.path, &l), 52);

    // An HEC exclusive hold covers 16 bytes from klockAddressWord (zlockActive.c
    // line 253): both the primary and the exclusive word report.
    let (_, prim, _) = l[0];
    h.lock(prim, 16);
    let s = status(&t.path);
    assert_eq!(
        s.reasons(),
        vec![LockReason::PrimaryWrite, LockReason::Exclusive]
    );
    h.unlock(prim, 16);
    assert_clean(&t.path);

    hold_all_then_exit(h, &t.path, &l);
    assert_eq!(std::fs::read(&t.path).unwrap(), before, "file modified");
}

#[test]
fn dss7_old_scheme_words() {
    let t = TempCopy::new("sample7.dss");
    let (l, detectable) = expected(&t.path);
    assert!(!detectable && !status(&t.path).readers_detectable);
    assert_eq!(
        l,
        vec![
            (LockReason::PrimaryWrite, 800, 8),
            (LockReason::Request, 808, 8),
            (LockReason::Exclusive, 816, 8),
        ]
    );
    let mut h = Holder::spawn(&t.path, false);
    assert_eq!(each_word(&mut h, &t.path, &l), 3);
    hold_all_then_exit(h, &t.path, &l);
    // Maintainer default (2026-10-28): the old scheme passes with a warning flag when
    // nothing is held (the writer holds the lock); a strict policy refuses it.
    let s = status(&t.path);
    let ok = ensure_unlocked_for_write(&s, &LockPolicy::default()).unwrap();
    assert!(ok.readers_undetected);
    let strict = LockPolicy {
        allow_undetectable_readers: false,
        ..LockPolicy::default()
    };
    let e = ensure_unlocked_for_write(&s, &strict).unwrap_err();
    assert!(e.readers_undetectable);
}

/// Every DSS6 word one at a time, the 12-byte exclusive hold, a read slot together with
/// the write word, reads with all words held, and release after exit.
fn dss6_words(name: &str, klockb: u64) {
    let t = TempCopy::new(name);
    let before = std::fs::read(&t.path).unwrap();
    let (l, detectable) = expected(&t.path);
    assert!(detectable && status(&t.path).readers_detectable);
    assert_eq!(l.len(), 103);
    let kb = l[0].1;
    assert_eq!(kb, klockb);
    assert_eq!(l[102], (LockReason::ReadArray, kb + 16 + 4 * 99, 4));
    let mut h = Holder::spawn(&t.path, false);
    assert_eq!(each_word(&mut h, &t.path, &l), 103);

    // Exclusive mode locks 12 bytes from KLOCKB (zmultu6.f lines 98-102).
    h.lock(kb, 12);
    assert_eq!(
        status(&t.path).reasons(),
        vec![
            LockReason::PrimaryWrite,
            LockReason::Request,
            LockReason::Exclusive
        ]
    );
    h.unlock(kb, 12);

    // An HEC writer: its read slot plus the write word (the Monolith "written" phase).
    h.lock(kb + 16, 4);
    h.lock(kb, 4);
    let s = status(&t.path);
    assert_eq!(
        s.held,
        vec![
            (LockReason::PrimaryWrite, kb, 4),
            (LockReason::ReadArray, kb + 16, 4)
        ]
    );
    assert!(read_everything(&t.path) > 0);
    h.unlock(kb, 4);
    h.unlock(kb + 16, 4);
    assert_clean(&t.path);

    hold_all_then_exit(h, &t.path, &l);
    assert_eq!(std::fs::read(&t.path).unwrap(), before, "file modified");
}

#[test]
fn dss6_every_word() {
    // castro (6-QF): lock record 3 at byte 1024 (dss6.md §4.4).
    dss6_words("castro.dss", 1024);
}

/// KHASH 8 puts KLOCKB at byte 512, inside the first 800 bytes. The header
/// read must stop at 276 bytes, so a held read slot is reported, not an I/O error.
#[test]
fn dss6_klockb_512_every_word() {
    dss6_words("synthetic_python6.dss", 512);
}

/// A pre-6-K file (version patched to 6-JE on a copy): readers_detectable is false, but a
/// read slot held by a current heclib is still reported, and the guard refuses.
#[test]
fn dss6_pre_6k_with_holder() {
    let t = TempCopy::new("castro.dss");
    let mut b = std::fs::read(&t.path).unwrap();
    b[16..20].copy_from_slice(b"6-JE");
    std::fs::write(&t.path, &b).unwrap();
    let (l, detectable) = expected(&t.path);
    assert!(!detectable);
    let s = status(&t.path);
    assert!(!s.readers_detectable && !s.locked());
    let ok = ensure_unlocked_for_write(&s, &LockPolicy::default()).unwrap();
    assert!(ok.readers_undetected);
    let strict = LockPolicy {
        allow_undetectable_readers: false,
        ..LockPolicy::default()
    };
    let e = ensure_unlocked_for_write(&s, &strict).unwrap_err();
    assert!(e.readers_undetectable && e.reasons.is_empty());
    let mut h = Holder::spawn(&t.path, false);
    let (_, slot, _) = l[3];
    h.lock(slot, 4);
    let s = status(&t.path);
    assert_eq!(s.held, vec![(LockReason::ReadArray, slot, 4)]);
    let allow = LockPolicy::default();
    let e = ensure_unlocked_for_write(&s, &allow).unwrap_err();
    assert_eq!(e.reasons, vec![LockReason::ReadArray]);
    h.exit();
    assert_clean_soon(&t.path);
    assert!(ensure_unlocked_for_write(&status(&t.path), &allow).is_ok());
}

/// A DSS6 copy truncated inside its lock record is BadLockLayout, never
/// "unlocked".
#[test]
fn dss6_truncated_lock_record_is_an_error() {
    let t = TempCopy::new("castro.dss");
    let b = std::fs::read(&t.path).unwrap();
    std::fs::write(&t.path, &b[..1100]).unwrap();
    assert!(matches!(
        lock_status(&t.path),
        Err(dss_core::lock::LockError::BadLockLayout { .. })
    ));
}

/// A DSS7 copy with a corrupt read-array pointer or file size is
/// BadLockLayout (no overflow, no wrap to offset 0).
#[test]
fn dss7_corrupt_lock_pointer_is_an_error() {
    let t = TempCopy::new("synthetic_python7.dss");
    let mut b = std::fs::read(&t.path).unwrap();
    for v in [i64::MAX, i64::MAX / 8 - 3, 1 << 40] {
        b[92 * 8..93 * 8].copy_from_slice(&v.to_le_bytes());
        std::fs::write(&t.path, &b).unwrap();
        assert!(
            matches!(
                lock_status(&t.path),
                Err(dss_core::lock::LockError::BadLockLayout { .. })
            ),
            "{v}"
        );
    }
    b[5 * 8..6 * 8].copy_from_slice(&0i64.to_le_bytes());
    std::fs::write(&t.path, &b).unwrap();
    assert!(matches!(
        lock_status(&t.path),
        Err(dss_core::lock::LockError::BadLockLayout { .. })
    ));
}

#[test]
fn dss6_stale_write_marker() {
    let t = TempCopy::new("castro.dss");
    let mut b = std::fs::read(&t.path).unwrap();
    // KFSIZE is DSS6 header word 5 (byte 20); -1357 = write-locked (zwlock6.f line 69).
    b[20..24].copy_from_slice(&(-1357i32).to_le_bytes());
    std::fs::write(&t.path, &b).unwrap();
    let s = status(&t.path);
    assert_eq!(s.held, vec![(LockReason::StaleWriteMarker, 20, 4)]);
    let e = ensure_unlocked_for_write(&s, &LockPolicy::default()).unwrap_err();
    assert_eq!(e.reasons, vec![LockReason::StaleWriteMarker]);
    assert!(e.to_string().contains("HEC-DSSVue"));
}

#[cfg(windows)]
#[test]
fn windows_sharing_violation() {
    let t = TempCopy::new("synthetic_python7.dss");
    let h = Holder::spawn(&t.path, true);
    let s = status(&t.path);
    assert_eq!(s.version, 0);
    assert_eq!(s.held, vec![(LockReason::SharingViolation, 0, 0)]);
    assert!(ensure_unlocked_for_write(&s, &LockPolicy::default()).is_err());
    h.exit();
    assert_clean_soon(&t.path);
}

#[test]
fn not_dss_is_an_error_and_missing_is_io() {
    let t = TempCopy::new("castro.dss");
    std::fs::write(&t.path, b"not a dss file at all, just some bytes").unwrap();
    assert!(lock_status(&t.path).is_err());
    assert!(matches!(
        lock_status(&t.dir.join("missing.dss")),
        Err(dss_core::lock::LockError::Io(_))
    ));
}

/// Opt-in: zero false positives over a directory of DSS copies.
#[test]
fn golden_dir_no_false_positives() {
    let Ok(dir) = std::env::var("DSS_LOCK_GOLDEN_DIR") else {
        eprintln!("DSS_LOCK_GOLDEN_DIR not set; skipping");
        return;
    };
    let mut n = 0;
    let mut undetectable = 0;
    let mut worst = 0.0f64;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) != Some("dss") {
            continue;
        }
        let t = Instant::now();
        match lock_status(&p) {
            Ok(s) => {
                worst = worst.max(t.elapsed().as_secs_f64() * 1e3);
                assert!(!s.locked(), "{}: {:?}", p.display(), s.held);
                undetectable += usize::from(!s.readers_detectable);
                n += 1;
            }
            Err(e) => eprintln!("{}: not tested: {e}", p.display()),
        }
    }
    eprintln!(
        "golden: {n} files unlocked, {undetectable} readers undetectable, worst {worst:.2} ms"
    );
}

//! When a new file cannot be published safely, `create_dss7`,
//! `create_dss6` and `repair_dss7` refuse (`WriteError::Unpublishable`) and never write
//! the destination directly.
//!
//! Each publication primitive is forced to fail through the per-thread test hook
//! `set_publish_faults`, and one real failure is provoked: NTFS allows at most 1024 names
//! per file, so extra hard links to the temporary file make the final link fail with an
//! ordinary OS error (1142) on a filesystem that does support hard links. In every case
//! the destination stays absent, nothing beside it is touched, create leaves no temporary
//! file, and repair keeps its verified copy and names it. Transient link errors are
//! retried; others are reported as they are, not retried and not downgraded.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use dss_core::dss6::write::create_dss6;
use dss_core::dss7::write::{
    create_dss7, link_attempts, repair_dss7, set_publish_faults, set_publish_hook, set_write_clock,
    write_rts, PublishFault, PublishPoint, PublishRefusal, RenameAttempt, RtsInput, RtsValues,
    WriteError, WriteOptions,
};

const CLOCK_MS: i64 = 1_614_834_367_000;
const OPS: [&str; 3] = ["create7", "create6", "repair7"];

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("publish_refuse_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_source(f: &Path) {
    create_dss7(f).unwrap();
    let vals: Vec<f32> = (0..48).map(|k| k as f32).collect();
    let input = RtsInput::new(
        b"/PR/A/FLOW/01JAN2020/1HOUR/SRC/",
        dss_core::time::ymd_to_julian(2020, 1, 1) * 86_400 + 3600,
        RtsValues::F32(&vals),
        b"CFS",
        b"INST-VAL",
    );
    write_rts(f, &input, &WriteOptions::default()).unwrap();
}

fn run_op(op: &str, src: &Path, dest: &Path) -> Result<(), WriteError> {
    match op {
        "create7" => create_dss7(dest),
        "create6" => create_dss6(dest),
        "repair7" => repair_dss7(src, dest).map(|_| ()),
        other => panic!("unknown op {other}"),
    }
}

fn names(d: &Path) -> BTreeSet<String> {
    std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// Run `op` with `faults` into a fresh directory holding a bystander file, and return the
/// refusal after checking the common guarantees.
fn refused(op: &str, faults: &[PublishFault], what: &str) -> PublishRefusal {
    set_write_clock(Some(CLOCK_MS));
    let d = tmp(&format!("{op}_{what}"));
    let src = d.join("source.dss");
    make_source(&src);
    let reference = d.join("reference.dss");
    run_op(op, &src, &reference).unwrap();
    let want = std::fs::read(&reference).unwrap();
    std::fs::write(d.join("bystander.dss"), b"untouched").unwrap();
    let before = names(&d);
    let dest = d.join("new.dss");
    set_publish_faults(faults);
    let r = run_op(op, &src, &dest);
    set_publish_faults(&[]);
    let ctx = format!("{op} {what}");
    let refusal = match r {
        Err(WriteError::Unpublishable(r)) => *r,
        other => panic!("{ctx}: expected a refusal, got {other:?}"),
    };
    // The destination was never created or written.
    assert!(
        std::fs::symlink_metadata(&dest).is_err(),
        "{ctx}: dest exists"
    );
    assert_eq!(refusal.dest, dest, "{ctx}");
    assert_eq!(
        std::fs::read(d.join("bystander.dss")).unwrap(),
        b"untouched",
        "{ctx}"
    );
    let after = names(&d);
    let new: Vec<_> = after.difference(&before).cloned().collect();
    if op == "repair7" {
        // Repair keeps its complete, verified copy and names it.
        let kept = refusal.kept.clone().expect("repair keeps its copy");
        assert_eq!(new.len(), 1, "{ctx}: {new:?}");
        assert_eq!(kept.file_name().unwrap().to_string_lossy(), new[0], "{ctx}");
        assert_eq!(
            std::fs::read(&kept).unwrap(),
            want,
            "{ctx}: kept copy is complete"
        );
        assert!(
            refusal.to_string().contains(&*kept.to_string_lossy()),
            "{ctx}"
        );
    } else {
        assert!(refusal.kept.is_none(), "{ctx}");
        assert!(new.is_empty(), "{ctx}: temporary file left: {new:?}");
    }
    let msg = refusal.to_string();
    assert!(
        msg.contains("not created") && msg.contains("copy it"),
        "{ctx}: {msg}"
    );
    let _ = std::fs::remove_dir_all(&d);
    refusal
}

/// No hard link and no no-replace rename (a FAT or exFAT volume): refused.
#[test]
fn no_safe_primitive_is_refused() {
    for op in OPS {
        let r = refused(
            op,
            &[
                PublishFault::LinkKind(ErrorKind::Unsupported),
                PublishFault::RenameUnavailable,
            ],
            "unsupported",
        );
        assert_eq!(r.link_error.kind(), ErrorKind::Unsupported, "{op}");
        assert!(matches!(r.rename, RenameAttempt::Unavailable), "{op}");
        assert_eq!(
            link_attempts(),
            1,
            "{op}: an unsupported link is not retried"
        );
    }
}

/// Ordinary link failures on a filesystem that has hard links (permissions, too many
/// links): reported as they are, once, never downgraded.
#[test]
fn ordinary_link_errors_are_reported_not_downgraded() {
    // Windows: 5 access denied, 1142 too many links. Linux and macOS: EACCES 13, EPERM 1,
    // EMLINK 31 (the same numbers on both).
    let codes: &[i32] = if cfg!(windows) {
        &[5, 1142]
    } else {
        &[13, 1, 31]
    };
    for op in OPS {
        for &code in codes {
            let r = refused(
                op,
                &[
                    PublishFault::LinkRawOs(code),
                    PublishFault::RenameUnavailable,
                ],
                &format!("os{code}"),
            );
            assert_eq!(r.link_error.raw_os_error(), Some(code), "{op} {code}");
            assert_eq!(link_attempts(), 1, "{op} {code}: not retried");
        }
        let r = refused(
            op,
            &[
                PublishFault::LinkKind(ErrorKind::PermissionDenied),
                PublishFault::RenameUnavailable,
            ],
            "permission",
        );
        assert_eq!(r.link_error.kind(), ErrorKind::PermissionDenied, "{op}");
    }
}

/// The no-replace rename was available and failed: refused, naming both errors.
#[test]
fn a_failed_rename_is_refused() {
    for op in OPS {
        let r = refused(
            op,
            &[
                PublishFault::LinkKind(ErrorKind::PermissionDenied),
                PublishFault::RenameKind(ErrorKind::PermissionDenied),
            ],
            "renamefail",
        );
        match &r.rename {
            RenameAttempt::Failed(e) => assert_eq!(e.kind(), ErrorKind::PermissionDenied),
            other => panic!("{op}: {other:?}"),
        }
        assert!(r.to_string().contains("no-replace rename failed"), "{op}");
    }
}

/// Transient link errors are retried and then publish; persistent ones end in a refusal
/// after a bounded number of attempts.
#[test]
fn transient_link_errors_are_retried() {
    for op in OPS {
        set_write_clock(Some(CLOCK_MS));
        let d = tmp(&format!("{op}_transient"));
        let src = d.join("source.dss");
        make_source(&src);
        let reference = d.join("reference.dss");
        run_op(op, &src, &reference).unwrap();
        let dest = d.join("new.dss");
        set_publish_faults(&[PublishFault::LinkTransient(2)]);
        let r = run_op(op, &src, &dest);
        let attempts = link_attempts();
        set_publish_faults(&[]);
        r.unwrap();
        assert_eq!(attempts, 3, "{op}");
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            std::fs::read(&reference).unwrap()
        );
        assert_eq!(
            names(&d),
            BTreeSet::from(["new.dss", "reference.dss", "source.dss"].map(String::from)),
            "{op}"
        );
        let _ = std::fs::remove_dir_all(&d);
        let r = refused(
            op,
            &[
                PublishFault::LinkTransient(100),
                PublishFault::RenameUnavailable,
            ],
            "persistent",
        );
        assert_eq!(r.link_error.kind(), ErrorKind::Interrupted, "{op}");
        assert_eq!(link_attempts(), 5, "{op}: bounded retries");
    }
}

/// Without a rename fault, a failed link is followed by the platform's no-replace rename:
/// it publishes on glibc Linux, and nothing weaker is tried anywhere else.
#[test]
fn the_rename_is_the_only_alternative_to_the_link() {
    for op in OPS {
        set_write_clock(Some(CLOCK_MS));
        let d = tmp(&format!("{op}_rename"));
        let src = d.join("source.dss");
        make_source(&src);
        let dest = d.join("new.dss");
        set_publish_faults(&[PublishFault::LinkKind(ErrorKind::Unsupported)]);
        let r = run_op(op, &src, &dest);
        set_publish_faults(&[]);
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            r.unwrap();
            assert!(dest.exists());
        } else {
            assert!(
                matches!(r, Err(WriteError::Unpublishable(_))),
                "{op}: {r:?}"
            );
            assert!(std::fs::symlink_metadata(&dest).is_err(), "{op}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

// ---------------------------------------------------------------------------------
// A real, ordinary link failure: too many names for the temporary file.
// ---------------------------------------------------------------------------------

thread_local! {
    static EXTRA_LINKS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Up to this many extra names; NTFS refuses the 1024th name of a file.
const LINK_CAP: usize = 1100;

fn exhaust_links(point: PublishPoint, tmp: &Path) {
    if point != PublishPoint::BeforePublish {
        return;
    }
    let dir = tmp.parent().unwrap();
    for i in 0..LINK_CAP {
        let extra = dir.join(format!("extra-link-{i}"));
        match std::fs::hard_link(tmp, &extra) {
            Ok(()) => EXTRA_LINKS.with(|l| l.borrow_mut().push(extra)),
            Err(_) => return, // the filesystem's link limit is reached
        }
    }
}

#[test]
fn a_real_too_many_links_failure_is_refused() {
    for op in OPS {
        set_write_clock(Some(CLOCK_MS));
        let d = tmp(&format!("{op}_mlink"));
        let src = d.join("source.dss");
        make_source(&src);
        let dest = d.join("new.dss");
        EXTRA_LINKS.with(|l| l.borrow_mut().clear());
        set_publish_hook(Some(exhaust_links));
        let r = run_op(op, &src, &dest);
        set_publish_hook(None);
        let extra = EXTRA_LINKS.with(|l| std::mem::take(&mut *l.borrow_mut()));
        let limited = extra.len() < LINK_CAP;
        if !limited {
            println!("SKIPPED {op}: no link limit below {LINK_CAP} names on this volume");
            r.unwrap();
        } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
            // The no-replace rename does not need a new name for the inode.
            r.unwrap();
            assert!(dest.exists(), "{op}");
        } else {
            println!("{op}: link limit reached after {} extra names", extra.len());
            match &r {
                Err(WriteError::Unpublishable(x)) => {
                    assert!(x.link_error.raw_os_error().is_some(), "{op}: {x}")
                }
                other => panic!("{op}: expected a refusal, got {other:?}"),
            }
            assert!(
                std::fs::symlink_metadata(&dest).is_err(),
                "{op}: dest created"
            );
        }
        // Every extra name was the temporary file's; for create they are its only copies.
        for x in &extra {
            std::fs::remove_file(x).unwrap();
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

//! Creating and publishing new files (`create_dss7`, `create_dss6`,
//! `repair_dss7`).
//!
//! Every new file is built under a unique temporary name the call creates exclusively,
//! then published by a hard link that fails if the destination exists (no-replace rename
//! where hard links are unavailable). See `dss_core::publish`.
//!
//! - **Killed-process matrix** (`*_killed_at_every_publication_point`): a child process
//!   runs one operation and blocks at the n-th [`PublishPoint`]; the parent kills it there
//!   (`TerminateProcess` / `SIGKILL`), for every n until the operation completes. After
//!   each kill the destination is absent or byte-identical to an uninterrupted reference
//!   made with the same fixed clock; the only other new names are this call's temporary
//!   names (allowed only from `Published` on as a second name of the complete file, or
//!   before it as a leftover that was never published); a file that existed before is untouched; and
//!   the operation can be run again: it refuses an existing destination, or completes.
//! - **Race**: a destination that appears just before publication is kept, never replaced.
//! - **Bystanders**: files beside the destination that the call did not create survive every
//!   outcome, including the deterministic temporary name of 0.1.2 and earlier.
//! - **Concurrent creates** of one destination: exactly one wins; no temporary file is left.
//! - **Case aliases**: on a case-insensitive volume (default APFS, NTFS), a
//!   destination that differs from an existing file only in case is refused, for create
//!   and for repair. Reported as skipped on a case-sensitive volume.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use dss_core::dss6::write::{create_dss6, create_dss6_with};
use dss_core::dss7::write::{
    create_dss7, create_dss7_with, repair_dss7, set_publish_hook, set_write_clock, write_rts,
    PublishPoint, RtsInput, RtsValues, SyncMode, WriteError, WriteOptions,
};

/// 2021-03-04 05:06:07 UTC: every file of this test is stamped with it.
const CLOCK_MS: i64 = 1_614_834_367_000;

const OPS: [&str; 4] = ["create7", "create7_none", "create6", "repair7"];

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("publish_kill_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A small DSS7 source for repair: three hourly series.
fn make_source(f: &Path) {
    create_dss7(f).unwrap();
    for (i, loc) in ["A", "B", "C"].iter().enumerate() {
        let p = format!("/PK/{loc}/FLOW/01JAN2020/1HOUR/SRC/");
        let vals: Vec<f32> = (0..48).map(|k| (k + 10 * i) as f32).collect();
        let input = RtsInput::new(
            p.as_bytes(),
            dss_core::time::ymd_to_julian(2020, 1, 1) * 86_400 + 3600,
            RtsValues::F32(&vals),
            b"CFS",
            b"INST-VAL",
        );
        write_rts(f, &input, &WriteOptions::default()).unwrap();
    }
}

fn run_op(op: &str, src: &Path, dest: &Path) -> Result<(), WriteError> {
    match op {
        "create7" => create_dss7(dest),
        "create7_none" => create_dss7_with(dest, SyncMode::None),
        "create6" => create_dss6(dest),
        "repair7" => {
            let r = repair_dss7(src, dest)?;
            assert!(r.complete(), "{r:?}");
            Ok(())
        }
        other => panic!("unknown op {other}"),
    }
}

// ---------------------------------------------------------------------------------
// The child process.
// ---------------------------------------------------------------------------------

static KILL_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
static SEEN: AtomicUsize = AtomicUsize::new(0);

fn pause_hook(point: PublishPoint, _tmp: &Path) {
    let i = SEEN.fetch_add(1, Ordering::SeqCst);
    if i == KILL_AT.load(Ordering::SeqCst) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "PK-PAUSED {i} {point:?}");
        let _ = out.flush();
        drop(out);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}

/// Child half: runs one operation and blocks at publication point `PK_AT`.
#[test]
#[ignore = "child process of the publication kill matrix"]
fn publish_kill_child() {
    let (Ok(op), Ok(src), Ok(dest), Ok(at)) = (
        std::env::var("PK_OP"),
        std::env::var("PK_SRC"),
        std::env::var("PK_DEST"),
        std::env::var("PK_AT"),
    ) else {
        return;
    };
    set_write_clock(Some(CLOCK_MS));
    KILL_AT.store(at.parse().unwrap(), Ordering::SeqCst);
    set_publish_hook(Some(pause_hook));
    let r = run_op(&op, Path::new(&src), Path::new(&dest));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "PK-DONE {} {:?}", SEEN.load(Ordering::SeqCst), r.err());
    let _ = out.flush();
}

enum Outcome {
    Killed(String),
    Done(usize),
}

fn kill_at(op: &str, src: &Path, dest: &Path, at: usize) -> Outcome {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "publish_kill_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PK_OP", op)
        .env("PK_SRC", src)
        .env("PK_DEST", dest)
        .env("PK_AT", at.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let outcome = loop {
        let Some(line) = lines.next() else {
            let st = child.wait().unwrap();
            panic!("{op} at {at}: child ended without a report ({st})");
        };
        let line = line.unwrap();
        if let Some(rest) = line.split_once("PK-PAUSED ").map(|x| x.1) {
            child.kill().unwrap();
            break Outcome::Killed(rest.to_string());
        }
        if let Some(rest) = line.split_once("PK-DONE ").map(|x| x.1) {
            let (n, err) = rest.split_once(' ').unwrap();
            assert_eq!(err, "None", "{op}: the uninterrupted child failed: {err}");
            break Outcome::Done(n.parse().unwrap());
        }
    };
    let st = child.wait().unwrap();
    if matches!(outcome, Outcome::Done(_)) {
        assert!(st.success(), "{op}: child exit {st}");
    }
    outcome
}

fn names(d: &Path) -> BTreeSet<String> {
    std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// One operation, killed at every publication point.
fn kill_matrix(op: &str) {
    set_write_clock(Some(CLOCK_MS));
    let d = tmp(op);
    let src = d.join("source.dss");
    if op == "repair7" {
        make_source(&src);
    }
    // The uninterrupted reference, with the same clock.
    let reference = d.join("reference.dss");
    run_op(op, &src, &reference).unwrap();
    let want = std::fs::read(&reference).unwrap();
    let work = d.join("work");
    let mut points = BTreeSet::new();
    let mut at = 0;
    loop {
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let dest = work.join("new.dss");
        // A bystander with the deterministic temporary name of 0.1.2 and earlier, and another
        // hidden sibling: neither is ever touched.
        let old_tmp = work.join(format!(".new.dss.{}.dss6-create", std::process::id()));
        let sibling = work.join(".new.dss.someone-else.partial");
        std::fs::write(&old_tmp, b"bystander 1").unwrap();
        std::fs::write(&sibling, b"bystander 2").unwrap();
        let before = names(&work);
        match kill_at(op, &src, &dest, at) {
            Outcome::Done(n) => {
                assert_eq!(n, at, "{op}: the child passed {n} points, expected {at}");
                assert_eq!(std::fs::read(&dest).unwrap(), want, "{op}: completed file");
                assert_eq!(
                    names(&work).len(),
                    before.len() + 1,
                    "{op}: leftovers after success"
                );
                break;
            }
            Outcome::Killed(what) => {
                let point = what.split_once(' ').unwrap().1.to_string();
                points.insert(point.clone());
                let ctx = format!("{op} killed at {at} ({point})");
                // The destination is absent or complete.
                let published = dest.exists();
                if published {
                    assert_eq!(
                        std::fs::read(&dest).unwrap(),
                        want,
                        "{ctx}: incomplete dest"
                    );
                }
                let reached_publish = matches!(point.as_str(), "Published" | "Cleaned");
                assert_eq!(
                    published, reached_publish,
                    "{ctx}: dest present = {published}"
                );
                // Bystanders untouched.
                assert_eq!(std::fs::read(&old_tmp).unwrap(), b"bystander 1", "{ctx}");
                assert_eq!(std::fs::read(&sibling).unwrap(), b"bystander 2", "{ctx}");
                // Only this call's own temporary names are new.
                let new: Vec<String> = names(&work).difference(&before).cloned().collect();
                for n in &new {
                    if n == "new.dss" {
                        continue;
                    }
                    assert!(n.starts_with(".new.dss."), "{ctx}: unexpected file {n}");
                    if published {
                        // A second name of the complete file.
                        assert_eq!(std::fs::read(work.join(n)).unwrap(), want, "{ctx}: {n}");
                    }
                }
                // Restartable: the next run refuses an existing destination or completes.
                match run_op(op, &src, &dest) {
                    Err(WriteError::AlreadyExists(_)) => assert!(published, "{ctx}"),
                    Ok(()) => {
                        assert!(!published, "{ctx}");
                        assert_eq!(std::fs::read(&dest).unwrap(), want, "{ctx}: rerun");
                    }
                    Err(e) => panic!("{ctx}: rerun failed: {e}"),
                }
                at += 1;
            }
        }
    }
    for p in [
        "Created",
        "Written",
        "BeforePublish",
        "Published",
        "Cleaned",
    ] {
        assert!(points.contains(p), "{op}: never killed at {p}: {points:?}");
    }
    println!("{op}: {at} kills at {points:?}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn create7_killed_at_every_publication_point() {
    kill_matrix(OPS[0]);
}

#[test]
fn create7_nosync_killed_at_every_publication_point() {
    kill_matrix(OPS[1]);
}

#[test]
fn create6_killed_at_every_publication_point() {
    kill_matrix(OPS[2]);
}

#[test]
fn repair7_killed_at_every_publication_point() {
    kill_matrix(OPS[3]);
}

// ---------------------------------------------------------------------------------
// In-process checks (the hook is per thread).
// ---------------------------------------------------------------------------------

thread_local! {
    static RACE_DEST: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn race_hook(point: PublishPoint, _tmp: &Path) {
    if point == PublishPoint::BeforePublish {
        RACE_DEST.with(|d| {
            if let Some(d) = d.borrow().as_ref() {
                std::fs::write(d, b"created by another process").unwrap();
            }
        });
    }
}

/// A destination that appears just before publication is kept; the temporary file is
/// removed; bystanders survive.
#[test]
fn a_destination_that_appears_before_publication_is_never_replaced() {
    let d = tmp("race");
    let src = d.join("source.dss");
    make_source(&src);
    for op in OPS {
        let work = d.join(op);
        std::fs::create_dir_all(&work).unwrap();
        let dest = work.join("new.dss");
        let old_tmp = work.join(format!(".new.dss.{}.dss6-create", std::process::id()));
        std::fs::write(&old_tmp, b"bystander").unwrap();
        RACE_DEST.with(|r| *r.borrow_mut() = Some(dest.clone()));
        set_publish_hook(Some(race_hook));
        let r = run_op(op, &src, &dest);
        set_publish_hook(None);
        assert!(
            matches!(r, Err(WriteError::AlreadyExists(_))),
            "{op}: {r:?}"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"created by another process",
            "{op}"
        );
        assert_eq!(std::fs::read(&old_tmp).unwrap(), b"bystander", "{op}");
        assert_eq!(
            names(&work),
            BTreeSet::from([
                "new.dss".to_string(),
                old_tmp.file_name().unwrap().to_string_lossy().into_owned()
            ]),
            "{op}: the temporary file must be removed"
        );
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A file at the deterministic temporary name of 0.1.2 and earlier
/// survives a create that fails, and one that succeeds.
#[test]
fn create6_never_removes_a_file_it_did_not_create() {
    let d = tmp("f1");
    let dest = d.join("new.dss");
    let old_tmp = d.join(format!(".new.dss.{}.dss6-create", std::process::id()));
    std::fs::write(&old_tmp, b"sentinel").unwrap();
    std::fs::write(&dest, b"existing").unwrap();
    assert!(matches!(
        create_dss6(&dest),
        Err(WriteError::AlreadyExists(_))
    ));
    assert_eq!(std::fs::read(&old_tmp).unwrap(), b"sentinel");
    std::fs::remove_file(&dest).unwrap();
    create_dss6_with(&dest, SyncMode::None).unwrap();
    assert_eq!(std::fs::read(&old_tmp).unwrap(), b"sentinel");
    assert_eq!(names(&d).len(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

/// Many threads create one destination: exactly one succeeds, the file is complete, and
/// no temporary file is left.
#[test]
fn concurrent_creates_of_one_destination() {
    for (v, op) in [(7, "create7"), (6, "create6")] {
        let d = tmp(&format!("concurrent{v}"));
        let dest = d.join("new.dss");
        let results: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..16)
                .map(|_| s.spawn(|| run_op(op, Path::new(""), &dest)))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok, 1, "{op}: {results:?}");
        assert!(results
            .iter()
            .all(|r| r.is_ok() || matches!(r, Err(WriteError::AlreadyExists(_)))));
        assert_eq!(names(&d), BTreeSet::from(["new.dss".to_string()]), "{op}");
        if v == 7 {
            dss_core::dss7::Dss7File::open(&dest).unwrap();
        } else {
            assert!(dss_core::dss6::write::check_file6(&dest)
                .unwrap()
                .is_clean());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Whether the volume holding `d` treats names that differ only in case as one name.
fn case_insensitive(d: &Path) -> bool {
    let probe = d.join("CaseProbe.tmp");
    std::fs::write(&probe, b"x").unwrap();
    let alias = d.join("caseprobe.tmp").exists();
    std::fs::remove_file(&probe).unwrap();
    alias
}

/// On a case-insensitive volume a destination that differs from an existing file only
/// in case is the existing file: create and repair refuse it and change nothing.
#[test]
fn case_alias_destinations_are_refused() {
    let d = tmp("case");
    if !case_insensitive(&d) {
        println!("SKIPPED: {} is on a case-sensitive volume", d.display());
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    println!("case-insensitive volume: {}", d.display());
    let src = d.join("Source.dss");
    make_source(&src);
    let src_bytes = std::fs::read(&src).unwrap();
    for (op, existing, alias) in [
        ("create7", "Model.dss", "MODEL.dss"),
        ("create6", "Model6.dss", "model6.DSS"),
        ("repair7", "Out.dss", "out.dss"),
    ] {
        let existing = d.join(existing);
        std::fs::write(&existing, b"keep me").unwrap();
        let r = run_op(op, &src, &d.join(alias));
        assert!(
            matches!(r, Err(WriteError::AlreadyExists(_))),
            "{op}: {r:?}"
        );
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me", "{op}");
    }
    // Repair onto a case alias of its own source.
    let r = repair_dss7(&src, &d.join("SOURCE.DSS"));
    assert!(matches!(r, Err(WriteError::AlreadyExists(_))), "{r:?}");
    assert_eq!(std::fs::read(&src).unwrap(), src_bytes);
    // Case variants of one destination name share no temporary file: two creates of
    // aliases, one after the other, leave one file and no temporary file.
    create_dss7(&d.join("Alias.dss")).unwrap();
    assert!(matches!(
        create_dss7(&d.join("ALIAS.dss")),
        Err(WriteError::AlreadyExists(_))
    ));
    assert!(
        !names(&d).iter().any(|n| n.starts_with(".")),
        "{:?}",
        names(&d)
    );
    let _ = std::fs::remove_dir_all(&d);
}

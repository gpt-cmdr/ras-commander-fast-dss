//! P05 acceptance check 8: the reader works while another process holds HEC-style byte
//! locks on the lock record, and never reads the lock record itself.
//!
//! Set `DSS6_LOCKED_FILE` to a DSS6 file (a scratch copy) that another process holds open
//! with Windows region locks on the lock record, the way `heclib_c/src/Dss6/lockdss.c`
//! does with `_locking`: the write word, request word and exclusive word at `KLOCKB`,
//! and a read slot at `KLOCKB + 16` (dss6.md §9.1). Without the variable the test is a
//! no-op.

use dss_core::dss6::{CatalogOptions, Dss6File, FileSource, InfoCheck, ReadAt};

#[test]
fn reads_while_lock_record_is_held() {
    let Ok(path) = std::env::var("DSS6_LOCKED_FILE") else {
        eprintln!("DSS6_LOCKED_FILE not set; skipping");
        return;
    };
    let before = std::fs::metadata(&path).unwrap();
    let f = Dss6File::open(&path).unwrap();
    let (lo, _) = f.header().lock_byte_range();

    // The holder's locks are mandatory: a raw read of the locked bytes fails.
    let raw = FileSource::open(&path).unwrap();
    let mut buf = [0u8; 4];
    let locked_read = raw.read_exact_at(lo, &mut buf);
    eprintln!(
        "raw read of lock record at byte {lo}: {:?}",
        locked_read.as_ref().map(|_| ())
    );
    let held = locked_read.is_err();

    let walk = f
        .walk_bins(&CatalogOptions {
            verify_info: InfoCheck::All,
            ..Default::default()
        })
        .unwrap();
    assert!(walk.anomalies.is_empty(), "{:?}", walk.anomalies);
    assert_eq!(walk.live().count() as i32, f.header().number_records);
    for e in walk.live() {
        assert!(f.lookup(&e.pathname).unwrap().is_some());
    }
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    eprintln!(
        "lock held by other process: {held}; catalogued {} live records, {} lookups, file unchanged",
        walk.live().count(),
        walk.live().count()
    );
    assert!(
        held,
        "expected the lock record to be locked by the holder process"
    );
}

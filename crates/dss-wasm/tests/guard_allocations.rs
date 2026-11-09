//! Regressions: limits are checked before allocation, on every spelling of the E
//! part and every route, and count expanded quality and notes. A counting allocator measures
//! the largest single allocation of a refused read. Only public fixtures, patched in memory.
#![cfg(not(target_arch = "wasm32"))]

use dss_core::dss7::{Dss7File, RecordRef};
use dss_wasm::reader::{lossy, DssBuffer, ErrorKind, DEFAULT_MAX_PROFILE_CELLS};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct Meter;
static LARGEST: AtomicUsize = AtomicUsize::new(0);
/// The meter is process-wide; tests that read it take turns.
static SERIAL: Mutex<()> = Mutex::new(());

// SAFETY: forwards to `System`; only records the requested size.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LARGEST.fetch_max(l.size(), Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LARGEST.fetch_max(n, Ordering::Relaxed);
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static METER: Meter = Meter;

fn largest_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.store(0, Ordering::Relaxed);
    let r = f();
    (r, LARGEST.load(Ordering::Relaxed))
}

const BLOCKS: u64 = 100_000;
const CELLS: u64 = DEFAULT_MAX_PROFILE_CELLS;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public")
            .join(name),
    )
    .unwrap()
}

/// Live blocks of the first dataset of `types`: its pathname and every block's record.
fn dataset(bytes: &[u8], types: &[i32]) -> (String, Vec<RecordRef>) {
    let mut f = Dss7File::new(Cursor::new(bytes.to_vec())).unwrap();
    let cat = f.catalog().unwrap();
    let live = cat.resolved_live();
    let i = *live
        .iter()
        .find(|&&i| types.contains(&cat.catalog.entries[i].record_type))
        .unwrap();
    let p = lossy(&cat.catalog.entries[i].pathname);
    let key = |s: &str| {
        let q: Vec<&str> = s.split('/').collect();
        (
            q[1].to_owned(),
            q[2].to_owned(),
            q[3].to_owned(),
            q[5].to_owned(),
            q[6].to_owned(),
        )
    };
    let mut recs = Vec::new();
    for &j in &live {
        let e = &cat.catalog.entries[j];
        if key(&lossy(&e.pathname)) == key(&p) {
            recs.push(f.lookup(&e.pathname).unwrap().unwrap());
        }
    }
    (p, recs)
}

fn set_bin_last(bytes: &mut [u8], recs: &[RecordRef], days: i32) {
    let at = recs[0].bin_entry_address as usize * 8 + 6 * 8;
    let first = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    for r in recs {
        let at = r.bin_entry_address as usize * 8 + 6 * 8;
        bytes[at + 4..at + 8].copy_from_slice(&(first + days).to_le_bytes());
    }
}

fn set_header(bytes: &mut [u8], r: &RecordRef, word: usize, v: i32) {
    let at = r.info.internal_header.address as usize * 8 + word * 4;
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn with_e(p: &str, e: &str) -> String {
    let mut q: Vec<&str> = p.split('/').collect();
    q[5] = e;
    q.join("/")
}

/// Finding 1: `dss-core` reads any spelling of an interval as the canonical E part.
#[test]
fn noncanonical_e_parts_do_not_bypass_whole_read_limits() {
    let _g = SERIAL.lock().unwrap();
    let mut bytes = fixture("p20_hec_rts7.dss");
    let (p, recs) = dataset(&bytes, &[100, 105]);
    set_bin_last(&mut bytes, &recs, 5);
    let alias = with_e(&p, "1MinuteJUNK");
    let mut b = DssBuffer::new(bytes.clone()).unwrap();
    // The alias really reads the data, so the refusals below are the guard's.
    assert_eq!(
        b.read_dataset(&alias, None, false).unwrap().ts.times.len(),
        8_640
    );
    let e = b
        .read_dataset_limited(&alias, None, false, 1, CELLS)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("maxBlocks"), "{e}");
    let e = b
        .read_dataset_limited(&alias, None, false, BLOCKS, 1)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("maxRows"), "{e}");

    // The reported file at default limits, through the alias, in both modes.
    set_bin_last(&mut bytes, &recs, 36_500);
    for mut b in [
        DssBuffer::new(bytes.clone()).unwrap(),
        DssBuffer::new_recovery(bytes.clone()).unwrap(),
    ] {
        let (r, big) = largest_during(|| b.read_dataset(&alias, None, false));
        assert_eq!(r.unwrap_err().kind, ErrorKind::InvalidArgument);
        assert!(big < 1 << 20, "largest allocation {big}");
    }

    let bytes = fixture("synthetic_profile7.dss");
    let (p, _) = dataset(&bytes, &[102, 107]);
    let alias = with_e(&p, "1HourJUNK");
    let mut b = DssBuffer::new(bytes).unwrap();
    assert!(b.read_profile(&alias, None, true, BLOCKS, CELLS).is_ok());
    let e = b.read_profile(&alias, None, true, 1, CELLS).unwrap_err();
    assert!(e.message.contains("maxBlocks"), "{e}");
    let e = b.read_profile(&alias, None, true, BLOCKS, 1).unwrap_err();
    assert!(e.message.contains("maxCells"), "{e}");
}

/// DSS6 looks blocks up under the E part as given, so an alias finds nothing to expand.
#[test]
fn dss6_noncanonical_e_part_finds_nothing() {
    let mut b = DssBuffer::new(fixture("synthetic_python6.dss")).unwrap();
    let c = b.catalog().unwrap();
    let p = (0..c.len())
        .map(|i| c.stored_pathname[i].clone())
        .find(|p| p.to_uppercase().contains("/1HOUR/") && !p.to_uppercase().contains("TS-P"))
        .unwrap();
    let alias = with_e(&p, "1HOURJUNK");
    assert!(b.read_dataset(&alias, None, false).is_err());
}

/// Finding 2: the time-series route refuses a profile without assembling it. A header
/// claiming 1,000 depths took a 5,760,000-byte allocation before the fix.
#[test]
fn read_dataset_refuses_profiles_before_assembling() {
    let _g = SERIAL.lock().unwrap();
    let mut bytes = fixture("synthetic_profile7.dss");
    let (p, recs) = dataset(&bytes, &[102, 107]);
    for r in &recs {
        set_header(&mut bytes, r, 3, 1_000);
    }
    let mut b = DssBuffer::new(bytes).unwrap();
    let whole = with_e(&p, "1Hour");
    let (r, big) = largest_during(|| b.read_dataset(&whole, None, true));
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
    assert!(e.message.contains("profile record"), "{e}");
    assert!(big < 1 << 20, "largest allocation {big}");
    // The same through a window.
    let day = 86_400;
    let t0 = 44_000 * day;
    let (r, big) = largest_during(|| b.read_dataset(&whole, Some((t0, t0 + 30 * day)), true));
    assert!(big < 1 << 20, "largest allocation {big}");
    drop(r);
}

/// Finding 3: quality and note element sizes multiply every row, stored or not.
#[test]
fn quality_and_notes_count_against_the_limits() {
    let _g = SERIAL.lock().unwrap();
    let mut bytes = fixture("p20_hec_rts7.dss");
    let (p, recs) = dataset(&bytes, &[100, 105]);
    for r in &recs {
        // Quality and integer-note element sizes, with nothing stored.
        set_header(&mut bytes, r, 11, 64);
        set_header(&mut bytes, r, 14, 64);
    }
    set_bin_last(&mut bytes, &recs, 5);
    let mut b = DssBuffer::new(bytes.clone()).unwrap();
    assert_eq!(
        b.read_dataset(&p, None, false).unwrap().ts.times.len(),
        8_640
    );
    // 8,640 rows fit a 100,000-row limit, but 129 elements per row do not.
    let e = b
        .read_dataset_limited(&p, None, false, BLOCKS, 100_000)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidArgument, "{e}");
    assert!(e.message.contains("x 129 elements"), "{e}");

    // 11,797,920 rows are under the default row count; 6 GiB of metadata is not.
    set_bin_last(&mut bytes, &recs, 8_192);
    let mut b = DssBuffer::new(bytes.clone()).unwrap();
    let (r, big) = largest_during(|| b.read_dataset(&p, None, false));
    let e = r.unwrap_err();
    assert!(e.message.contains("maxRows"), "{e}");
    assert!(big < 1 << 20, "largest allocation {big}");

    // Character notes add a string per row.
    let mut bytes = fixture("p20_hec_rts7.dss");
    for r in &recs {
        set_header(&mut bytes, r, 16, 1);
    }
    let mut b = DssBuffer::new(bytes).unwrap();
    let e = b
        .read_dataset_limited(&p, None, false, BLOCKS, 1)
        .unwrap_err();
    assert!(e.message.contains("x 4 elements"), "{e}");

    // Profile routes count them too, on the record as well as the dataset.
    let mut bytes = fixture("synthetic_profile_f32_7.dss");
    let (p, recs) = dataset(&bytes, &[102, 107]);
    let mut clean = DssBuffer::new(bytes.clone()).unwrap();
    let got = clean.read_profile(&p, None, false, BLOCKS, CELLS).unwrap();
    let depths = got.profile.depths.len() as u64;
    for r in &recs {
        set_header(&mut bytes, r, 11, 64);
        set_header(&mut bytes, r, 14, 64);
    }
    let want = format!("x {} elements", depths + 128);
    let mut b = DssBuffer::new(bytes).unwrap();
    let e = b.read_profile(&p, None, false, BLOCKS, 1).unwrap_err();
    assert!(e.message.contains(&want), "{e}");
    let e = b.read_profile_record(&p, 1).unwrap_err();
    assert!(e.message.contains(&want), "{e}");
}

/// Irregular rows are the stored values, and quality and notes multiply them too.
#[test]
fn irregular_quality_counts_against_the_limit() {
    let mut bytes = fixture("p21_hec_its7.dss");
    let (p, recs) = dataset(&bytes, &[110, 115]);
    let mut clean = DssBuffer::new(bytes.clone()).unwrap();
    let rows = clean.read_dataset(&p, None, false).unwrap().ts.times.len() as u64;
    assert!(rows > 0);
    assert!(clean
        .read_dataset_limited(&p, None, false, BLOCKS, rows)
        .is_ok());
    for r in &recs {
        set_header(&mut bytes, r, 11, 64);
    }
    let mut b = DssBuffer::new(bytes).unwrap();
    let e = b
        .read_dataset_limited(&p, None, false, BLOCKS, rows)
        .unwrap_err();
    assert!(e.message.contains("x 65 elements"), "{e}");
}

/// DSS6 keeps a quality word per row once any block has quality (`zrrtsi.f`).
#[test]
fn dss6_quality_counts_against_the_limit() {
    let mut checked = 0;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    for name in ["synthetic_hec_variants6.dss", "p22_hec_ts6.dss"] {
        let path = dir.join(name);
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut b = DssBuffer::new(bytes).unwrap();
        let c = b.catalog().unwrap();
        for i in 0..c.len() {
            let p = c.stored_pathname[i].clone();
            if c.status[i] != "live" || p.contains("TS-P") || p.contains("/IR-") {
                continue;
            }
            let Ok(d) = b.read_dataset(&p, None, false) else {
                continue;
            };
            if d.ts.quality.is_some() {
                let e = b
                    .read_dataset_limited(&p, None, false, BLOCKS, 1)
                    .unwrap_err();
                assert!(e.message.contains("x 2 elements"), "{name} {p}: {e}");
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no regular DSS6 quality fixture found");
}

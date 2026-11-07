//! DSS7 regular profile reads (packet P14): `Dss7File::read_ts_profile` (dataset) and
//! `read_ts_profile_record` (one block).
//!
//! `fixtures/public/synthetic_profile7.dss` is written by HEC Monolith 3.3.27
//! (`evidence/P14/scripts/ProfileMonolith.java`), so the expected
//! values below are that program's inputs, computed with the same IEEE double
//! arithmetic. Monolith always stores profiles as doubles (107; javaHeclib
//! `Hec_ztsStore.c`), so the float (102) case uses an in-memory synthetic record built
//! here from the DSS7 layout (`Internal/ztsInternalHeaderPack.c`,
//! `Internal/ztsStoreReg7.c`): no HEC oracle exists for it.

mod dss7_common;

use dss7_common::{build, public_fixture, set_word, Rec, Spec};
use dss_core::dss7::info::key;
use dss_core::dss7::{Dss7File, TsError, TsReadOptions, TsWindow};
use dss_core::time::{hec_seconds, ymd_to_julian};
use dss_core::TsValues;

const DBL: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP//1HOUR/SYNTH-DBL/";
const DAY: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP//1DAY/SYNTH-DAY/";
const GAP: &[u8] = b"/PUBLIC/LAKE/DEPTH-DO//1DAY/SYNTH-GAP/";
const IRR: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP//IR-MONTH/SYNTH-IRR/";
const TS: &[u8] = b"/PUBLIC/LAKE/FLOW//1HOUR/SYNTH-TS/";
const UNDEF: f64 = -3.4028234663852886e38;

/// The committed Monolith fixture; a missing file fails the test (never a vacuous pass).
fn fixture() -> std::path::PathBuf {
    public_fixture("-", "synthetic_profile7.dss")
        .expect("committed fixture fixtures/public/synthetic_profile7.dss is missing")
}

fn f64s(v: &TsValues) -> &[f64] {
    match v {
        TsValues::F64(x) => x,
        TsValues::F32(_) => panic!("expected doubles"),
    }
}

fn f32s(v: &TsValues) -> &[f32] {
    match v {
        TsValues::F32(x) => x,
        TsValues::F64(_) => panic!("expected floats"),
    }
}

fn whole(trim: bool) -> TsReadOptions {
    TsReadOptions {
        window: None,
        trim_missing: trim,
    }
}

/// `ProfileMonolith.write` dataset A: 10 hourly rows from 31Jan2020 20:00, 4 depths.
fn expected_dbl() -> Vec<f64> {
    let mut v = Vec::new();
    for i in 0..10 {
        for k in 0..4 {
            let (fi, fk) = (i as f64, k as f64);
            v.push(20.0 - 0.75 * fk + 0.125 * fi + 1.0e-9 * (fi * 7.0 + fk));
        }
    }
    v[2 * 4 + 1] = UNDEF;
    v[3 * 4 + 3] = -901.0;
    for k in 0..4 {
        v[5 * 4 + k] = UNDEF;
    }
    v
}

#[test]
fn monolith_double_profile_whole_trimmed() {
    let p = fixture();
    let before = std::fs::read(&p).unwrap();
    let mut f = Dss7File::open(&p).unwrap();
    let r = f.read_ts_profile(DBL, &whole(true)).unwrap();
    assert_eq!(r.record_type, 107);
    assert_eq!(f64s(&r.depths), &[0.0, 1.5, 3.0, 4.5]);
    let want = expected_dbl();
    let got = f64s(&r.values);
    assert_eq!(got.len(), want.len());
    for (a, b) in got.iter().zip(&want) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
    let t0 = hec_seconds(ymd_to_julian(2020, 1, 31), 20 * 3600);
    let times: Vec<i64> = (0..10).map(|i| t0 + i * 3600).collect();
    assert_eq!(r.times, times);
    assert_eq!(r.depth_units, "m");
    assert_eq!(r.value_units, "deg C");
    assert_eq!(r.data_type, "INST-VAL");
    assert_eq!(r.time_zone.as_deref(), Some("UTC"));
    assert_eq!(r.meta.blocks_found, 2);
    drop(f);
    assert_eq!(
        std::fs::read(&p).unwrap(),
        before,
        "reader modified the file"
    );
}

#[test]
fn untrimmed_and_windowed_reads_agree_with_records() {
    let p = fixture();
    let mut f = Dss7File::open(&p).unwrap();
    let all = f.read_ts_profile(DBL, &whole(false)).unwrap();
    // Bin dates: 31Jan2020 01:00 .. 01Feb2020 24:00 (whole days, HEC "retrieve all").
    assert_eq!(all.times.len(), 48);
    let trimmed = f.read_ts_profile(DBL, &whole(true)).unwrap();
    let off = all
        .times
        .iter()
        .position(|&t| t == trimmed.times[0])
        .unwrap();
    assert_eq!(
        &f64s(&all.values)[off * 4..(off + 10) * 4],
        f64s(&trimmed.values)
    );
    assert!(f64s(&all.values)[..off * 4].iter().all(|&v| v == UNDEF));
    // Mid-dataset window: rows 1..=8.
    let w = TsWindow {
        start: trimmed.times[1],
        end: trimmed.times[8],
    };
    let r = f
        .read_ts_profile(
            DBL,
            &TsReadOptions {
                window: Some(w),
                trim_missing: false,
            },
        )
        .unwrap();
    assert_eq!(r.times, &trimmed.times[1..9]);
    assert_eq!(f64s(&r.values), &f64s(&trimmed.values)[4..36]);
    // Reversed window.
    let bad = TsWindow {
        start: w.end,
        end: w.start,
    };
    assert!(matches!(
        f.read_ts_profile(
            DBL,
            &TsReadOptions {
                window: Some(bad),
                trim_missing: false
            }
        ),
        Err(TsError::BadWindow)
    ));
    // The dataset is the two block records stitched together.
    let cat = f.catalog().unwrap();
    let mut rows = 0;
    for e in &cat.catalog.entries {
        if e.pathname.ends_with(b"/1Hour/SYNTH-DBL/") {
            let rec = f.lookup(&e.pathname).unwrap().unwrap();
            let one = f.read_ts_profile_record(&rec).unwrap();
            assert_eq!(f64s(&one.depths), f64s(&all.depths));
            rows += one.times.len();
        }
    }
    assert_eq!(rows, 744 + 696); // January + February 2020 hourly blocks
}

#[test]
fn missing_block_reads_as_missing_rows() {
    let p = fixture();
    let mut f = Dss7File::open(&p).unwrap();
    let r = f.read_ts_profile(GAP, &whole(true)).unwrap();
    // 30Dec2018 .. 02Jan2020 daily; the 2019 block does not exist.
    assert_eq!(r.times.len(), 370);
    assert_eq!(r.meta.blocks_in_window, 3);
    assert_eq!(r.meta.blocks_found, 2);
    let v = f64s(&r.values);
    assert_eq!(&v[..4], &[5.5, 4.25, 5.75, 4.5]);
    assert_eq!(&v[v.len() - 4..], &[6.0, 4.75, 6.25, 5.0]);
    assert_eq!(v.iter().filter(|&&x| x == UNDEF).count(), 366 * 2);
    assert_eq!(r.value_units, "mg/l");
    assert_eq!(r.time_zone, None);
}

#[test]
fn non_profiles_are_refused_with_typed_errors() {
    let p = fixture();
    let mut f = Dss7File::open(&p).unwrap();
    for (path, t) in [(IRR, 117), (TS, 100)] {
        match f.read_ts_profile(path, &whole(true)) {
            Err(TsError::UnsupportedRecordType { record_type, .. }) => assert_eq!(record_type, t),
            other => panic!("expected UnsupportedRecordType, got {other:?}"),
        }
    }
    // The plain time-series reads keep refusing profiles.
    for path in [DBL, DAY] {
        assert!(matches!(
            f.read_ts(path, &whole(true)),
            Err(TsError::UnsupportedRecordType { .. })
        ));
    }
    assert!(matches!(
        f.read_ts_profile(b"/PUBLIC/LAKE/NOPE//1DAY/X/", &whole(true)),
        Err(TsError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------------------------
// Monolith fixture: quality and windows between stamps.
// ---------------------------------------------------------------------------------

const QUAL: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP//1HOUR/SYNTH-QUAL/";
const QUAL_REC: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP/01Jun2020/1Hour/SYNTH-QUAL/";
const DBL_TS: &[u8] = b"/PUBLIC/LAKE/TEMP-SURF//1HOUR/SYNTH-DBL-TS/";

#[test]
fn quality_is_returned_by_record_and_dataset_reads() {
    let p = fixture();
    let mut f = Dss7File::open(&p).unwrap();
    let ds = f.read_ts_profile(QUAL, &whole(true)).unwrap();
    assert_eq!(ds.meta.quality_element_size, 1);
    assert_eq!(
        ds.meta.quality_words.as_deref(),
        Some(&[3, 5, 9, 17, 33][..])
    );
    let rec = f.lookup(QUAL_REC).unwrap().unwrap();
    let one = f.read_ts_profile_record(&rec).unwrap();
    assert_eq!(one.meta.quality_element_size, 1);
    let q = one
        .meta
        .quality_words
        .as_ref()
        .expect("record read keeps quality");
    assert_eq!(q.len(), one.times.len());
    let pos = one.times.iter().position(|&t| t == ds.times[0]).unwrap();
    assert_eq!(&q[pos..pos + 5], &[3, 5, 9, 17, 33]);
}

#[test]
fn windows_between_stamps_match_the_scalar_series() {
    let p = fixture();
    let mut f = Dss7File::open(&p).unwrap();
    let t0 = hec_seconds(ymd_to_julian(2020, 1, 31), 20 * 3600);
    // Bounds at :30 and :59:59 between hourly stamps, on both sides of the block edge.
    let cases = [
        (t0 + 1800, t0 + 4 * 3600 + 1800),
        (t0 - 1, t0 + 3599),
        (t0 + 3 * 3600 + 1, t0 + 9 * 3600 - 1),
    ];
    for (start, end) in cases {
        for trim_missing in [false, true] {
            let o = TsReadOptions {
                window: Some(TsWindow { start, end }),
                trim_missing,
            };
            let pr = f.read_ts_profile(DBL, &o).unwrap();
            let ts = f.read_ts(DBL_TS, &o).unwrap();
            assert_eq!(
                pr.times, ts.ts.times,
                "window {start}..{end} trim {trim_missing}"
            );
            let col0: Vec<u64> = f64s(&pr.values).chunks(4).map(|r| r[0].to_bits()).collect();
            let scalar: Vec<u64> = f64s(&ts.ts.values).iter().map(|v| v.to_bits()).collect();
            if !trim_missing {
                assert_eq!(col0, scalar);
            }
        }
    }
}

#[test]
fn pattern_pathnames_are_refused() {
    let p = public_fixture("-", "synthetic_python7.dss").expect("committed fixture missing");
    let mut f = Dss7File::open(&p).unwrap();
    match f.read_ts_profile(b"/A/B/FLOW/TS-PATTERN/1HOUR/PAT/", &whole(true)) {
        Err(TsError::UnsupportedRecordType { record_type, .. }) => assert_eq!(record_type, 101),
        other => panic!("expected UnsupportedRecordType, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------
// Synthetic float profiles (102), no HEC oracle.
// ---------------------------------------------------------------------------------

const F32_PATH: &str = "/PUBLIC/LAKE/DEPTH-TEMP/01Jan2021/1Day/SYNTH-F32/";
const F32_DS: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP//1DAY/SYNTH-F32/";
const MB_DS: &[u8] = b"/PUBLIC/LAKE/DEPTH-TEMP//1DAY/SYNTH-MB/";

fn f32_values() -> Vec<f32> {
    let mut v = Vec::new();
    for i in 0..6 {
        for k in 0..3 {
            v.push(60.1f32 + 1.3 * i as f32 - 2.7 * k as f32);
        }
    }
    v[4 * 3] = f32::MIN; // -FLT_MAX (UNDEFINED)
    v[2 * 3 + 2] = -901.0;
    v[3 * 3 + 1] = -902.0;
    v
}

/// One synthetic 102 block record: `values` row-major (`n × depths.len()`), stored from
/// block position `start_pos` (0-based) of its 1DAY year block, uncompressed.
struct Blk {
    path: String,
    /// Bin dates (first, last julian).
    julian: (i64, i64),
    start_pos: i32,
    depths: Vec<f32>,
    values: Vec<f32>,
    units: &'static [u8],
    precision: i32,
}

/// Build a DSS7 file of 102 block records from the DSS7 layout.
fn profile_file(blocks: &[Blk]) -> Vec<u8> {
    let recs = blocks
        .iter()
        .map(|b| {
            let mut r = Rec::live(&b.path, 102);
            r.julian = (b.julian.0 as i32, b.julian.1 as i32);
            r
        })
        .collect();
    let built = build(&Spec {
        recs,
        ..Default::default()
    });
    let mut bytes = built.bytes;
    // Areas start on int64 words; lengths count int32 words (zwriteNew.c).
    let eof = bytes.len() / 8 - 1;
    bytes.truncate(eof * 8);
    let area = |bytes: &mut Vec<u8>, words: &[i32]| -> (usize, usize) {
        let at = bytes.len() / 8;
        for w in words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        (at, words.len())
    };
    let mut sets = Vec::new();
    for b in blocks {
        let nd = b.depths.len() as i32;
        let n = b.values.len() as i32 / nd;
        // ztsInternalHeaderPack(): granularity, precision, offset, depths, block start
        // and end positions, values number / size / element size / compression,
        // quality and notes (none), cnotes length, then int 17 blank padding and the
        // strings (depth units, value units, type, zone).
        let mut ih: Vec<i32> = vec![
            60,
            b.precision,
            0,
            nd,
            b.start_pos,
            b.start_pos + n - 1,
            n * nd,
            nd,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        ih.push(i32::from_le_bytes(*b"    "));
        let mut s = b.units.to_vec();
        while s.len() % 4 != 0 {
            s.push(0);
        }
        for c in s.chunks(4) {
            ih.push(i32::from_le_bytes(c.try_into().unwrap()));
        }
        let values: Vec<i32> = b.values.iter().map(|v| v.to_bits() as i32).collect();
        let depths: Vec<i32> = b.depths.iter().map(|v| v.to_bits() as i32).collect();
        let a1 = area(&mut bytes, &ih);
        let a2 = area(&mut bytes, &values);
        let a3 = area(&mut bytes, &depths);
        sets.push((a1, a2, a3, n));
    }
    let new_eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    for (i, ((ih_at, ih_n), (v1_at, v1_n), (v2_at, v2_n), n)) in sets.into_iter().enumerate() {
        let info = built.info_addr[i] as usize;
        set_word(&mut bytes, info + key::INTERNAL_HEAD_ADDRESS, ih_at as i64);
        set_word(&mut bytes, info + key::INTERNAL_HEAD_NUMBER, ih_n as i64);
        set_word(&mut bytes, info + key::VALUES1_ADDRESS, v1_at as i64);
        set_word(&mut bytes, info + key::VALUES1_NUMBER, v1_n as i64);
        set_word(&mut bytes, info + key::VALUES2_ADDRESS, v2_at as i64);
        set_word(&mut bytes, info + key::VALUES2_NUMBER, v2_n as i64);
        set_word(&mut bytes, info + key::NUMBER_DATA, i64::from(n));
        set_word(&mut bytes, info + key::LOGICAL_NUMBER, i64::from(n));
        set_word(&mut bytes, info + key::ALLOCATED_SIZE, (v1_n + v2_n) as i64);
    }
    set_word(&mut bytes, 5, new_eof as i64);
    bytes
}

/// One 102 record: 6 daily rows from 03Mar2021 24:00 (block positions 61..66 of the
/// 2021 year block), 3 depths, precision 2, zone UTC.
fn f32_blk() -> Blk {
    Blk {
        path: F32_PATH.to_string(),
        julian: (ymd_to_julian(2021, 3, 3), ymd_to_julian(2021, 3, 9)),
        start_pos: 61,
        depths: vec![0.5, 2.25, 10.0],
        values: f32_values(),
        units: b"ft\0deg F\0PER-AVER\0UTC\0",
        precision: 2,
    }
}

fn synthetic_f32() -> Vec<u8> {
    profile_file(&[f32_blk()])
}

/// A 102 dataset over two year blocks: 30Dec2020 24:00 and 31Dec2020 24:00 (positions
/// 364..365 of 2020, a leap year) then 01Jan2021 24:00 and 02Jan2021 24:00 (positions
/// 0..1 of 2021). `depths_b` and `values_b` describe the second block.
fn mb_blks(depths_a: &[f32], depths_b: &[f32]) -> (Vec<Blk>, Vec<f32>, Vec<f32>) {
    let mk = |nd: usize, base: f32| -> Vec<f32> {
        (0..2 * nd).map(|i| base + 0.5 * i as f32 + 0.001).collect()
    };
    let va = mk(depths_a.len(), 11.0);
    let mut vb = mk(depths_b.len(), 21.0);
    vb[1] = -901.0;
    let units: &'static [u8] = b"m\0deg C\0INST-VAL\0";
    let blks = vec![
        Blk {
            path: "/PUBLIC/LAKE/DEPTH-TEMP/01Jan2020/1Day/SYNTH-MB/".to_string(),
            julian: (ymd_to_julian(2020, 12, 30), ymd_to_julian(2020, 12, 31)),
            start_pos: 364,
            depths: depths_a.to_vec(),
            values: va.clone(),
            units,
            precision: -1,
        },
        Blk {
            path: "/PUBLIC/LAKE/DEPTH-TEMP/01Jan2021/1Day/SYNTH-MB/".to_string(),
            julian: (ymd_to_julian(2021, 1, 1), ymd_to_julian(2021, 1, 2)),
            start_pos: 0,
            depths: depths_b.to_vec(),
            values: vb.clone(),
            units,
            precision: -1,
        },
    ];
    (blks, va, vb)
}

fn two_blocks(depths_a: &[f32], depths_b: &[f32]) -> (Vec<u8>, Vec<f32>, Vec<f32>) {
    let (blks, va, vb) = mb_blks(depths_a, depths_b);
    (profile_file(&blks), va, vb)
}

/// `fixtures/public/synthetic_profile_f32_7.dss`: the single-block SYNTH-F32 record and
/// the two-block SYNTH-MB dataset (3 depths in both blocks).
fn python_fixture() -> Vec<u8> {
    let (mut blks, _, _) = mb_blks(&[0.5, 1.0, 2.5], &[0.5, 1.0, 2.5]);
    blks.insert(0, f32_blk());
    profile_file(&blks)
}

#[test]
fn python_float_fixture_reads_and_regenerates() {
    let b = python_fixture();
    let mut f = Dss7File::from_bytes(&b).unwrap();
    let one = f.read_ts_profile(F32_DS, &whole(true)).unwrap();
    assert_eq!(one.values, TsValues::F32(f32_values()));
    let mb = f.read_ts_profile(MB_DS, &whole(true)).unwrap();
    assert_eq!(mb.meta.blocks_found, 2);
    // Regeneration only: with DSS_PY_FIXTURE_OUT set, write the committed fixture.
    // The tests never read DSS_PY_FIXTURE_OUT output.
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_profile_f32_7.dss"), &b).unwrap();
    }
}

#[test]
fn synthetic_float_profile_keeps_float32() {
    let b = synthetic_f32();
    let mut f = Dss7File::from_bytes(&b).unwrap();
    let rec = f.lookup(F32_PATH.as_bytes()).unwrap().unwrap();
    let one = f.read_ts_profile_record(&rec).unwrap();
    assert_eq!(one.record_type, 102);
    assert_eq!(one.times.len(), 365);
    let ds = f.read_ts_profile(F32_DS, &whole(true)).unwrap();
    assert_eq!(f32s(&ds.depths), &[0.5, 2.25, 10.0]);
    let want = f32_values();
    let got = f32s(&ds.values);
    assert_eq!(got.len(), want.len());
    for (a, b) in got.iter().zip(&want) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
    let t0 = hec_seconds(ymd_to_julian(2021, 3, 4), 0);
    assert_eq!(
        ds.times,
        (0..6).map(|i| t0 + i * 86_400).collect::<Vec<_>>()
    );
    assert_eq!(ds.depth_units, "ft");
    assert_eq!(ds.value_units, "deg F");
    assert_eq!(ds.data_type, "PER-AVER");
    assert_eq!(ds.time_zone.as_deref(), Some("UTC"));
    assert_eq!(ds.meta.precision, 2);
    // The record read covers the whole year block with the same stored rows.
    let pos = one.times.iter().position(|&t| t == t0).unwrap();
    assert_eq!(&f32s(&one.values)[pos * 3..(pos + 6) * 3], got);
}

#[test]
fn float_profile_stitched_across_two_blocks_is_bit_exact() {
    let depths = [0.5f32, 1.0, 2.5];
    let (b, va, vb) = two_blocks(&depths, &depths);
    let mut f = Dss7File::from_bytes(&b).unwrap();
    let ds = f.read_ts_profile(MB_DS, &whole(true)).unwrap();
    assert_eq!(ds.record_type, 102);
    assert_eq!(ds.meta.blocks_found, 2);
    assert_eq!(f32s(&ds.depths), &depths);
    let t0 = hec_seconds(ymd_to_julian(2020, 12, 31), 0);
    assert_eq!(
        ds.times,
        (0..4).map(|i| t0 + i * 86_400).collect::<Vec<_>>()
    );
    let want: Vec<u32> = va.iter().chain(&vb).map(|v| v.to_bits()).collect();
    let got: Vec<u32> = f32s(&ds.values).iter().map(|v| v.to_bits()).collect();
    assert_eq!(got, want);
    // An explicit window over the block edge reads the same rows.
    let o = TsReadOptions {
        window: Some(TsWindow {
            start: t0 + 86_400,
            end: t0 + 2 * 86_400,
        }),
        trim_missing: false,
    };
    let w = f.read_ts_profile(MB_DS, &o).unwrap();
    let got: Vec<u32> = f32s(&w.values).iter().map(|v| v.to_bits()).collect();
    assert_eq!(got, &want[3..9]);
}

#[test]
fn blocks_with_different_depths_are_refused() {
    let four = [0.5f32, 1.0, 2.5, 4.0];
    let three = [0.5f32, 1.0, 2.5];
    let moved = [0.5f32, 1.25, 2.5];
    for (a, b, what) in [
        (
            &four[..],
            &three[..],
            "profile depth count differs between blocks",
        ),
        (
            &three[..],
            &four[..],
            "profile depth count differs between blocks",
        ),
        (
            &three[..],
            &moved[..],
            "profile depth values differ between blocks",
        ),
    ] {
        let (bytes, _, _) = two_blocks(a, b);
        let mut f = Dss7File::from_bytes(&bytes).unwrap();
        for trim_missing in [true, false] {
            let o = TsReadOptions {
                window: None,
                trim_missing,
            };
            match f.read_ts_profile(MB_DS, &o) {
                Err(TsError::Corrupt { what: w, .. }) => assert_eq!(w, what),
                other => panic!("expected Corrupt({what}), got {other:?}"),
            }
        }
        // Each block alone is still readable as a record.
        let cat = f.catalog().unwrap();
        for e in &cat.catalog.entries {
            let rec = f.lookup(&e.pathname).unwrap().unwrap();
            assert!(f.read_ts_profile_record(&rec).is_ok());
        }
    }
}

#[test]
fn hostile_profile_bytes_never_panic() {
    let depths = [0.5f32, 1.0, 2.5];
    for base in [synthetic_f32(), two_blocks(&depths, &depths).0] {
        let mut rng = dss7_common::Rng(0x5eed_1234_abcd_0001);
        let n = base.len() / 8;
        for _ in 0..2_000 {
            let mut b = base.clone();
            for _ in 0..1 + rng.below(4) {
                let w = rng.below(n as u64) as usize;
                let v = rng.next() as i64;
                set_word(&mut b, w, v);
            }
            let Ok(mut f) = Dss7File::from_bytes(&b) else {
                continue;
            };
            for ds in [F32_DS, MB_DS] {
                let _ = f.read_ts_profile(ds, &whole(true));
                let _ = f.read_ts_profile(ds, &whole(false));
            }
            if let Ok(cat) = f.catalog() {
                for e in cat.catalog.entries.iter().take(4) {
                    if let Ok(Some(rec)) = f.lookup(&e.pathname) {
                        let _ = f.read_ts_profile_record(&rec);
                    }
                }
            }
        }
    }
}

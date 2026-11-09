//! P25: pattern time-origin facts.
//!
//! Pattern records (types 101/106 regular, 111/116 irregular; D part `TS-...`) store no
//! dates. The readers put the first regular value at 01Jan3000 00:00 on both versions
//! and never apply the stored offset; `pattern_origin()` reports the raw stored offset
//! and the start heclib C `Public/ztsRetrievePattern.c` (lines 360-372) reports.
//!
//! `pattern_offset7.dss` (fixtures/public) is generated here. Neither hecdss 0.1.33
//! (`hec_dss_tsStoreRegular`) nor Monolith 3.3.27 (`Heclib.Hec_ztsStore`) can store a
//! nonzero DSS7 pattern offset: `Internal/ztsProcessTimes.c` (lines 268-289) returns for a
//! `TS-P` D part before it derives `timeOffsetSeconds`, and neither binding sets it, so
//! `SemiPublic/ztsStorePattern.c` line 139 writes 0. To regenerate, set
//! `DSS_P25_FIXTURE_OUT` to a scratch directory and run this test.

mod dss6_ts_support;
mod dss7_common;

use dss6_ts_support::{f32w, its_header, rts_header, secs, ts_file, Payload, Rec};
use dss_core::dss6::{DatasetOptions, Dss6File, TsLayout};
use dss_core::dss7::ts::pattern_heclib_first_seconds;
use dss_core::dss7::{info::key, Dss7File, TsReadOptions};
use dss_core::timeseries::{PatternOffsetSource, PATTERN_TIME_CONVENTION};
use dss_core::TsValues;

/// (pathname, record type, stored INT_HEAD_timeOffset seconds, values).
const P7: [(&str, i32, i32, &[f64]); 3] = [
    (
        "//P25 SYNTH/FLOW-UNIT GRAPH/TS-PATTERN/15Minute/OFFSET180/",
        101,
        180,
        &[0.0, 10.0, 25.0, 12.5, 4.0, 0.0],
    ),
    (
        "//P25 SYNTH/FLOW-UNIT GRAPH/TS-PATTERN/1Hour/OFFSET1800/",
        106,
        1800,
        &[1.0, 2.0, 3.0, 2.0],
    ),
    (
        "//P25 SYNTH/FLOW-UNIT GRAPH/TS-PATTERN/15Minute/OFFSET0/",
        101,
        0,
        &[5.0, 6.0, 7.0],
    ),
];

/// A DSS7 TS internal header as `Internal/ztsInternalHeaderPack.c` lays it out
/// (`headers/zStructAllocation.h`): granularity, precision, time offset, ... at ints
/// 0-16, int 17 padding (7-HA and later), then the NUL-terminated units, type and time
/// zone strings.
fn dss7_internal_header(offset: i32, n: usize, elem: i32) -> Vec<i32> {
    let mut h = vec![0i32; 18];
    h[0] = 60; // INT_HEAD_timeGranularity: MINUTE_GRANULARITY
    h[1] = -1; // INT_HEAD_precision: not set
    h[2] = offset; // INT_HEAD_timeOffset
    h[6] = n as i32; // INT_HEAD_valuesNumber
    h[7] = elem; // INT_HEAD_valueSize
    h[8] = elem; // INT_HEAD_valueElementSize
    let mut s = b"CFS\0INST-VAL\0\0".to_vec();
    s.resize(s.len().div_ceil(4) * 4, 0);
    h.extend(
        s.chunks(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])),
    );
    h
}

fn ints_to_bytes(w: &[i32]) -> Vec<u8> {
    w.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn synthetic_pattern_offset7() -> Vec<u8> {
    let recs: Vec<(&str, i32, Vec<i32>, &[f64])> = P7
        .iter()
        .map(|(p, t, offset, values)| {
            let elem = if *t == 106 { 2 } else { 1 };
            (
                *p,
                *t,
                dss7_internal_header(*offset, values.len(), elem),
                *values,
            )
        })
        .collect();
    build_dss7_patterns(&recs)
}

/// DSS7 file of regular pattern records `(pathname, type, internal header, values)`.
fn build_dss7_patterns(recs: &[(&str, i32, Vec<i32>, &[f64])]) -> Vec<u8> {
    use dss7_common::{build, set_word, Rec, Spec};
    let built = build(&Spec {
        recs: recs.iter().map(|(p, t, _, _)| Rec::live(p, *t)).collect(),
        ..Default::default()
    });
    let mut bytes = built.bytes;
    // Replace the EOF word with the areas; addresses count int64 words, area lengths
    // count int32 words (Internal/zwriteNew.c:zwriteNew, zinit.c:zinit).
    bytes.truncate(bytes.len() - 8);
    for (i, (_, rtype, ih, values)) in recs.iter().enumerate() {
        let info = built.info_addr[i] as usize;
        let elem = if *rtype == 106 { 2 } else { 1 };
        let ih_addr = bytes.len() / 8;
        bytes.extend(ints_to_bytes(ih));
        bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        // Internal/ztsRetrievePattern.c: values1 holds the raw floats (101) or doubles
        // (106), numberData values.
        let v_addr = bytes.len() / 8;
        let vwords: Vec<i32> = if elem == 2 {
            values
                .iter()
                .flat_map(|v| {
                    let b = v.to_bits();
                    [(b & 0xffff_ffff) as u32 as i32, (b >> 32) as u32 as i32]
                })
                .collect()
        } else {
            values.iter().map(|&v| f32w(v as f32)).collect()
        };
        bytes.extend(ints_to_bytes(&vwords));
        bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        set_word(
            &mut bytes,
            info + key::INTERNAL_HEAD_ADDRESS,
            ih_addr as i64,
        );
        set_word(
            &mut bytes,
            info + key::INTERNAL_HEAD_NUMBER,
            ih.len() as i64,
        );
        set_word(&mut bytes, info + key::VALUES1_ADDRESS, v_addr as i64);
        set_word(&mut bytes, info + key::VALUES1_NUMBER, vwords.len() as i64);
        set_word(&mut bytes, info + key::NUMBER_DATA, values.len() as i64);
        set_word(&mut bytes, info + key::LOGICAL_NUMBER, values.len() as i64);
        let alloc = (bytes.len() / 8 - ih_addr) as i64;
        set_word(&mut bytes, info + key::ALLOCATED_SIZE, alloc);
    }
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(&mut bytes, 5, eof as i64);
    bytes
}

fn committed_fixture() -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/pattern_offset7.dss");
    std::fs::read(&p).unwrap_or_else(|e| panic!("committed fixture {} missing: {e}", p.display()))
}

#[test]
fn heclib_first_rule() {
    // ztsRetrievePattern: 01Jan3000 00:00 + one interval; a nonzero offset moves it to
    // that offset within the interval (ztsOffsetAdjustToOffset).
    assert_eq!(
        pattern_heclib_first_seconds(900, 0),
        Some(secs(3000, 1, 1, 0, 15))
    );
    assert_eq!(
        pattern_heclib_first_seconds(900, 180),
        Some(secs(3000, 1, 1, 0, 3))
    );
    assert_eq!(
        pattern_heclib_first_seconds(3600, 180),
        Some(secs(3000, 1, 1, 0, 3))
    );
    assert_eq!(
        pattern_heclib_first_seconds(3600, 1800),
        Some(secs(3000, 1, 1, 0, 30))
    );
    assert_eq!(
        pattern_heclib_first_seconds(360, 0),
        Some(secs(3000, 1, 1, 0, 6))
    );
    assert_eq!(
        pattern_heclib_first_seconds(86_400, 0),
        Some(secs(3000, 1, 2, 0, 0))
    );
    assert_eq!(pattern_heclib_first_seconds(0, 0), None);
}

#[test]
fn dss7_stored_offset_is_reported_not_applied() {
    let bytes = synthetic_pattern_offset7();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    for (path, rtype, offset, values) in P7 {
        let step = if path.contains("/1Hour/") { 3600 } else { 900 };
        for r in [
            f.read_ts(path.as_bytes(), &TsReadOptions::default())
                .unwrap(),
            {
                let rec = f.lookup(path.as_bytes()).unwrap().unwrap();
                f.read_ts_record(&rec).unwrap()
            },
        ] {
            assert!(r.meta.pattern);
            assert_eq!(r.ts.record_type, rtype);
            assert_eq!(r.ts.times.len(), values.len());
            // First value at 01Jan3000 00:00 regardless of the stored offset.
            let t0 = secs(3000, 1, 1, 0, 0);
            let expect: Vec<i64> = (0..values.len() as i64).map(|i| t0 + i * step).collect();
            assert_eq!(r.ts.times, expect, "{path}");
            assert_eq!(r.meta.time_offset_seconds, 0, "applied offset");
            let o = r.pattern_origin().expect("pattern origin");
            assert_eq!(o.time_convention(), PATTERN_TIME_CONVENTION);
            assert_eq!(o.stored_offset_seconds, Some(i64::from(offset)));
            assert_eq!(
                o.stored_offset_source,
                PatternOffsetSource::Dss7IntHeadTimeOffset
            );
            assert_eq!(o.stored_offset_source.as_str(), "dss7_int_head_time_offset");
            let heclib = pattern_heclib_first_seconds(step, i64::from(offset)).unwrap();
            assert_eq!(o.heclib_first_seconds, Some(heclib));
            match r.ts.values {
                TsValues::F32(v) => {
                    assert_eq!(rtype, 101);
                    assert_eq!(v, values.iter().map(|&x| x as f32).collect::<Vec<_>>());
                }
                TsValues::F64(v) => {
                    assert_eq!(rtype, 106);
                    assert_eq!(v, values.to_vec());
                }
            }
        }
    }
    // Expected heclib starts (checked against HEC's C library).
    let mut first = |p: &str| {
        f.read_ts(p.as_bytes(), &TsReadOptions::default())
            .unwrap()
            .pattern_origin()
            .unwrap()
            .heclib_first_seconds
    };
    assert_eq!(first(P7[0].0), Some(secs(3000, 1, 1, 0, 3)));
    assert_eq!(first(P7[1].0), Some(secs(3000, 1, 1, 0, 30)));
    assert_eq!(first(P7[2].0), Some(secs(3000, 1, 1, 0, 15)));

    if let Some(out) = std::env::var_os("DSS_P25_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("pattern_offset7.dss"), &bytes).unwrap();
    }
    // Always checked: a missing fixture fails instead of skipping the staleness check.
    assert!(
        committed_fixture() == bytes,
        "fixtures/public/pattern_offset7.dss is stale"
    );
}

/// HEC seconds of 01Jan3000 00:00.
fn t3000() -> i64 {
    secs(3000, 1, 1, 0, 0)
}

fn dss7_origin(ih: Vec<i32>) -> dss_core::timeseries::PatternOrigin {
    let path = "//P25 EDGE/FLOW/TS-PATTERN/15Minute/EDGE/";
    let name = format!(
        "edge7_{}.dss",
        ih.iter()
            .take(3)
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join("_")
    );
    let bytes = build_dss7_patterns(&[(path, 101, ih, &[1.0, 2.0, 3.0])]);
    // Optional copies for reading with HEC software: DSS_P25_FIXTURE_OUT=<scratch dir>.
    if let Some(out) = std::env::var_os("DSS_P25_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(name), &bytes).unwrap();
    }
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let r = f
        .read_ts(path.as_bytes(), &TsReadOptions::default())
        .unwrap();
    // Times never move, whatever the header holds.
    assert_eq!(r.ts.times, vec![t3000(), t3000() + 900, t3000() + 1800]);
    assert_eq!(r.meta.time_offset_seconds, 0);
    r.pattern_origin().unwrap()
}

#[test]
fn dss7_negative_offset() {
    // ztsOffsetAdjustToOffset: 00:15 -> back one period (00:00) -> -180 s = 2999-12-31 23:57.
    let o = dss7_origin(dss7_internal_header(-180, 3, 1));
    assert_eq!(o.stored_offset_seconds, Some(-180));
    assert_eq!(o.heclib_first_seconds, Some(secs(2999, 12, 31, 23, 57)));
    assert_eq!(pattern_heclib_first_seconds(900, -180), Some(t3000() - 180));
}

#[test]
fn dss7_offset_larger_than_interval() {
    // C adds the offset after stepping back one period, then cleanTime(): 00:00 + 3600 s.
    let o = dss7_origin(dss7_internal_header(3600, 3, 1));
    assert_eq!(o.stored_offset_seconds, Some(3600));
    assert_eq!(o.heclib_first_seconds, Some(secs(3000, 1, 1, 1, 0)));
}

#[test]
fn dss7_short_internal_headers() {
    // INT_HEAD_timeOffset is int 2: headers of 1 or 2 ints have no offset word.
    // heclib diverges on 2 ints: ztsInternalHeaderUnpack.c line 76 only rejects < 2, so it
    // reads one int past the header (pydsstools/Monolith showed garbage).
    for (ih, stored) in [
        (vec![60], None),
        (vec![60, -1], None),
        (vec![60, -1, 180], Some(180)),
    ] {
        let o = dss7_origin(ih);
        assert_eq!(o.stored_offset_seconds, stored);
        let applied = stored.unwrap_or(0);
        let expect = if applied == 0 {
            secs(3000, 1, 1, 0, 15)
        } else {
            secs(3000, 1, 1, 0, 3)
        };
        assert_eq!(o.heclib_first_seconds, Some(expect));
    }
}

const P6_WRAP: &str = "/A/B/FLOW/TS-PATTERN/15MIN/WRAP/";
const P6_EMPTY: &str = "/A/B/FLOW/TS-PATTERN/15MIN/EMPTY/";
const P6_IRR: &str = "/A/B/FLOW/TS-PATTERN/IR-DAY/IRR/";

fn dss6_edge_file() -> Vec<u8> {
    let recs = [
        Rec::live(P6_WRAP, 101),
        Rec::live(P6_EMPTY, 101),
        Rec::live(P6_IRR, 111),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    let vals: Vec<i32> = [1.0f32, 2.0, 3.0].iter().map(|&v| f32w(v)).collect();
    // 40,000,000 minutes x 60 = 2.4e9 s, which wraps C int (ztsRetrievePattern.c line 282).
    f.attach(
        P6_WRAP,
        &Payload {
            internal_header: rts_header(40_000_000, "CFS", "INST-VAL", 5),
            data: vals.clone(),
            kilnda: 3,
            ..Default::default()
        },
    );
    f.attach(
        P6_EMPTY,
        &Payload {
            internal_header: Vec::new(),
            data: vals,
            kilnda: 3,
            ..Default::default()
        },
    );
    // zritsi6.f: irregular header word 0 is the capacity, word 1 the pair count;
    // pattern pairs are (minutes, value).
    let pairs: Vec<i32> = [(0, 1.0f32), (30, 2.0), (90, 3.0)]
        .iter()
        .flat_map(|&(t, v)| [t, f32w(v)])
        .collect();
    f.attach(
        P6_IRR,
        &Payload {
            internal_header: its_header(3, 3, 0, 0, "CFS", "INST-VAL"),
            data: pairs,
            kilnda: 3,
            ..Default::default()
        },
    );
    f.finish()
}

#[test]
fn dss6_word0_wraps_like_c_int() {
    let b = dss6_edge_file();
    let f = Dss6File::from_bytes(&b).unwrap();
    let r = f.read_ts_record(P6_WRAP.as_bytes()).unwrap();
    assert_eq!(r.times, vec![900, 1800, 2700]);
    let o = r.pattern_origin().unwrap();
    // Reported raw (no wrap); heclib C applies the wrapped int: 2.4e9 - 2^32.
    assert_eq!(o.stored_offset_seconds, Some(2_400_000_000));
    let wrapped: i64 = 2_400_000_000 - (1i64 << 32);
    assert_eq!(wrapped, -1_894_967_296);
    // 00:15 -> back one period (00:00) -> + wrapped offset.
    assert_eq!(o.heclib_first_seconds, Some(t3000() + wrapped));
}

#[test]
fn dss6_empty_header_and_irregular_pattern() {
    let b = dss6_edge_file();
    let f = Dss6File::from_bytes(&b).unwrap();
    let e = f.read_ts_record(P6_EMPTY.as_bytes()).unwrap();
    assert_eq!(e.layout, TsLayout::RegularPattern);
    let o = e.pattern_origin().unwrap();
    assert_eq!(o.stored_offset_seconds, None);
    // heclib C applies nothing with fewer than 5 header words.
    assert_eq!(o.heclib_first_seconds, Some(secs(3000, 1, 1, 0, 15)));

    let i = f.read_ts_record(P6_IRR.as_bytes()).unwrap();
    assert_eq!(i.layout, TsLayout::IrregularPattern);
    assert_eq!(i.times, vec![0, 1800, 5400]);
    let o = i.pattern_origin().unwrap();
    assert_eq!(o.stored_offset_seconds, None);
    assert_eq!(o.heclib_first_seconds, None);
    assert_eq!(
        o.stored_offset_source,
        PatternOffsetSource::Dss6InternalHeaderWord0Minutes
    );
    let d = f
        .read_ts_dataset(P6_IRR.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(d.report.pattern_origin, Some(o));
}

#[test]
fn non_pattern_records_have_no_origin() {
    let b6 = dss6_pattern_file();
    let f6 = Dss6File::from_bytes(&b6).unwrap();
    let r = f6.read_ts_record(ORDINARY6.as_bytes()).unwrap();
    assert!(r.pattern_origin().is_none());
    let d = f6
        .read_ts_dataset(ORDINARY6.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert!(d.report.pattern_origin.is_none());
}

const PATTERN6: &str = "/A/B/FLOW/TS-PATTERN/15MIN/JUNK3/";
const PATTERN6_SHORT: &str = "/A/B/FLOW/TS-PATTERN/15MIN/SHORT/";
const ORDINARY6: &str = "/A/B/FLOW/01JAN2000/1HOUR/F/";

/// DSS6 patterns: word 0 = 3 minutes (like the junk `IOFSET` of a private
/// HEC-written test file), and a 1-word header
/// (heclib C applies word 0 only when the header has at least 5 words).
fn dss6_pattern_file() -> Vec<u8> {
    let recs = [
        Rec::live(PATTERN6, 101),
        Rec::live(PATTERN6_SHORT, 101),
        Rec::live(ORDINARY6, 100),
    ];
    let mut f = ts_file(&recs, *b"6-YN");
    let vals: Vec<i32> = [1.0f32, 2.0, 3.0].iter().map(|&v| f32w(v)).collect();
    f.attach(
        PATTERN6,
        &Payload {
            internal_header: rts_header(3, "CFS", "INST-VAL", 5),
            data: vals.clone(),
            kilnda: 3,
            ..Default::default()
        },
    );
    f.attach(
        PATTERN6_SHORT,
        &Payload {
            internal_header: vec![3],
            data: vals,
            kilnda: 3,
            ..Default::default()
        },
    );
    f.attach(
        ORDINARY6,
        &Payload {
            internal_header: rts_header(0, "CFS", "INST-VAL", 24),
            data: (0..744).map(|i| f32w(i as f32)).collect(),
            kilnda: 744,
            ..Default::default()
        },
    );
    f.finish()
}

#[test]
fn dss6_word0_is_reported_not_applied() {
    let b6 = dss6_pattern_file();
    let f = Dss6File::from_bytes(&b6).unwrap();
    let r = f.read_ts_record(PATTERN6.as_bytes()).unwrap();
    assert_eq!(r.layout, TsLayout::RegularPattern);
    // The DSS6 reader numbers patterns k * interval (the Python layer moves them to
    // 01JAN3000 00:00); the stored word is not applied.
    assert_eq!(r.times, vec![900, 1800, 2700]);
    assert_eq!(r.time_offset_minutes, 0);
    let o = r.pattern_origin().unwrap();
    assert_eq!(o.time_convention(), PATTERN_TIME_CONVENTION);
    assert_eq!(o.stored_offset_seconds, Some(180));
    assert_eq!(
        o.stored_offset_source,
        PatternOffsetSource::Dss6InternalHeaderWord0Minutes
    );
    assert_eq!(
        o.stored_offset_source.as_str(),
        "dss6_internal_header_word0_minutes"
    );
    // heclib C applies word 0 (on that private file pydsstools and Monolith start at 00:03).
    assert_eq!(o.heclib_first_seconds, Some(secs(3000, 1, 1, 0, 3)));
    let d = f
        .read_ts_dataset(PATTERN6.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(d.report.pattern_origin, Some(o));
    assert_eq!(d.series.times, r.times);

    // Short header: word 0 is reported, but heclib C does not apply it.
    let s = f.read_ts_record(PATTERN6_SHORT.as_bytes()).unwrap();
    let o = s.pattern_origin().unwrap();
    assert_eq!(o.stored_offset_seconds, Some(180));
    assert_eq!(o.heclib_first_seconds, Some(secs(3000, 1, 1, 0, 15)));
}

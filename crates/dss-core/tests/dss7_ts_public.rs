//! DSS7 time-series reads on the public fixtures (packet P08a). Expected values come from
//! the `hecdss` oracle dumps (P02 golden set); the tests skip when a fixture is absent.

mod dss7_common;

use dss7_common::{bald_eagle, sample7};
use dss_core::dss7::{Dss7File, TsError, TsReadOptions, TsWindow};
use dss_core::time::{hec_seconds, ymd_to_julian};
use dss_core::{TsKind, TsValues};

fn f32s(v: &TsValues) -> &[f32] {
    match v {
        TsValues::F32(x) => x,
        TsValues::F64(_) => panic!("expected float values"),
    }
}

#[test]
fn sample7_7_10_record_with_compressed_quality() {
    let Some(p) = sample7() else { return };
    let mut f = Dss7File::open(&p).unwrap();
    // 7-10 file: deprecated values1 layout, units at int 17.
    assert!(f.header().fver < dss_core::dss7::ts::FVER_7_HA);
    let rec = f
        .lookup(b"//SACRAMENTO/TEMP-MAX/01Jan1878/1Day/OBS/")
        .unwrap()
        .unwrap();
    let r = f.read_ts_record(&rec).unwrap();
    assert_eq!(r.ts.len(), 365);
    assert_eq!(&f32s(&r.ts.values)[..4], &[51.0, 50.0, 46.0, 46.0]);
    // First value at 01Jan1878 24:00.
    assert_eq!(r.ts.times[0], hec_seconds(ymd_to_julian(1878, 1, 2), 0));
    assert_eq!(r.ts.times[1] - r.ts.times[0], 86_400);
    let q = r.ts.quality.as_ref().unwrap();
    assert_eq!(&q[..4], &[3, 3, 3, 3]);
    assert_eq!(
        r.ts.kind,
        TsKind::Regular {
            interval_seconds: 86_400
        }
    );
    assert!(!r.meta.internal_header.units_padded);
}

#[test]
fn sample7_hourly_block_expands_to_the_whole_month() {
    let Some(p) = sample7() else { return };
    let mut f = Dss7File::open(&p).unwrap();
    let rec = f
        .lookup(b"/GREEN RIVER/GLENFIR/FLOW/01Apr1992/1Hour/OBS/")
        .unwrap()
        .unwrap();
    let r = f.read_ts_record(&rec).unwrap();
    assert_eq!(r.ts.len(), 720);
    assert_eq!(r.ts.units, "CFS");
    assert_eq!(r.ts.data_type, "INST-VAL");
    let v = f32s(&r.ts.values);
    // Leading positions are not stored: −FLT_MAX, as hecdss returns them.
    assert_eq!(v[0].to_bits(), (-f32::MAX).to_bits());
    assert_eq!(v[719], 1128.0);
    assert_eq!(r.ts.times[0], hec_seconds(ymd_to_julian(1992, 4, 1), 3600));
    assert_eq!(r.ts.times[719], hec_seconds(ymd_to_julian(1992, 5, 1), 0));
}

#[test]
fn sample7_paired_data_is_not_a_time_series() {
    let Some(p) = sample7() else { return };
    let mut f = Dss7File::open(&p).unwrap();
    let cat = f.catalog().unwrap();
    let pd = cat.catalog.live().find(|e| e.record_type == 200).unwrap();
    let rec = f.lookup(&pd.pathname).unwrap().unwrap();
    assert!(matches!(
        f.read_ts_record(&rec),
        Err(TsError::UnsupportedRecordType {
            record_type: 200,
            ..
        })
    ));
}

const BE: &[u8] = b"/BCLINE/BaldEagleCr: DS2NormalD/FLOW//1Minute/Grid Precip Infiltration/";

#[test]
fn bald_eagle_dataset_reads_match_hecdss_counts() {
    let Some(p) = bald_eagle() else { return };
    let before = std::fs::read(&p).unwrap();
    let mut f = Dss7File::open(&p).unwrap();
    // Whole dataset (hec_dss_tsRetrieve with NULL dates): 8 daily blocks, 11,520 values.
    let all = f.read_ts(BE, &TsReadOptions::default()).unwrap();
    assert_eq!(all.ts.len(), 11_520);
    assert_eq!(all.meta.blocks_found, 8);
    // Trimmed (ztsTrim; HEC Monolith HecDss.get(path, true)).
    let trimmed = f
        .read_ts(
            BE,
            &TsReadOptions {
                window: None,
                trim_missing: true,
            },
        )
        .unwrap();
    assert_eq!(trimmed.ts.len(), 9_973);
    // A window starting and ending mid-block: 08Sep2018 05:17 to 11Sep2018 12:58.
    let j = ymd_to_julian(2018, 9, 8);
    let w = TsWindow {
        start: hec_seconds(j, 5 * 3600 + 17 * 60),
        end: hec_seconds(j + 3, 12 * 3600 + 58 * 60),
    };
    let mid = f
        .read_ts(
            BE,
            &TsReadOptions {
                window: Some(w),
                trim_missing: false,
            },
        )
        .unwrap();
    assert_eq!(mid.ts.len(), 4_782);
    assert_eq!(mid.ts.times[0], w.start);
    assert_eq!(*mid.ts.times.last().unwrap(), w.end);
    // The stitched window equals the corresponding slice of the whole dataset.
    let k = all.ts.times.iter().position(|&t| t == w.start).unwrap();
    assert_eq!(
        &all.ts.values.to_f64()[k..k + 4_782],
        &mid.ts.values.to_f64()[..]
    );
    // Reversed window.
    let bad = TsWindow {
        start: w.end,
        end: w.start,
    };
    assert!(matches!(
        f.read_ts(
            BE,
            &TsReadOptions {
                window: Some(bad),
                trim_missing: false
            }
        ),
        Err(TsError::BadWindow)
    ));
    // Unknown dataset.
    assert!(matches!(
        f.read_ts(
            b"/X/Y/Z//1Hour/F/",
            &TsReadOptions {
                window: Some(w),
                trim_missing: false
            }
        ),
        Err(TsError::NotFound { .. })
    ));
    drop(f);
    assert_eq!(
        before,
        std::fs::read(&p).unwrap(),
        "reader must not modify the file"
    );
}

#[test]
fn every_public_ts_record_decodes() {
    for p in [sample7(), bald_eagle()].into_iter().flatten() {
        let mut f = Dss7File::open(&p).unwrap();
        let cat = f.catalog().unwrap();
        let mut n = 0;
        for e in cat
            .catalog
            .live()
            .filter(|e| (100..200).contains(&e.record_type))
        {
            let rec = f.lookup(&e.pathname).unwrap().unwrap();
            let r = f.read_ts_record(&rec).unwrap();
            assert_eq!(r.ts.times.len(), r.ts.values.len());
            assert_eq!(r.ts.len() as i64, rec.info.logical_number);
            n += 1;
        }
        assert!(n > 100);
    }
}

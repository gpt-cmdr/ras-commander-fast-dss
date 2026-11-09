//! DSS6 time-series reads on synthetic files. No external files needed.

mod dss6_ts_support;

use dss6_ts_support::{sample_file, secs};
use dss_core::dss6::{DatasetOptions, Dss6File, TsError, TsLayout, TsReadOptions};
use dss_core::{TsKind, TsValues};

fn f32s(v: &TsValues) -> &[f32] {
    match v {
        TsValues::F32(x) => x,
        _ => panic!("expected f32"),
    }
}

#[test]
fn regular_float_block() {
    let bytes = sample_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let r = f.read_ts_record(b"/A/B/FLOW/01JAN2000/1HOUR/F32/").unwrap();
    assert_eq!(r.len(), 744);
    assert_eq!(r.layout, TsLayout::Regular);
    // First value 01JAN 01:00, last 31JAN 24:00 (= 01FEB 00:00).
    assert_eq!(r.times[0], secs(2000, 1, 1, 1, 0));
    assert_eq!(r.times[743], secs(2000, 2, 1, 0, 0));
    let v = f32s(&r.values);
    assert_eq!(v[0], -901.0);
    assert_eq!(v[10], 5.0);
    assert_eq!(
        (r.units.as_str(), r.data_type.as_str()),
        ("CFS", "INST-VAL")
    );
    assert!(!r.nonstandard_block);
    let ts = r.to_timeseries();
    assert_eq!(
        ts.kind,
        TsKind::Regular {
            interval_seconds: 3600
        }
    );
    // Lookup is case-insensitive like HEC's.
    assert!(f.read_ts(b"/a/b/flow/01jan2000/1hour/f32/").is_ok());
}

#[test]
fn quality_double_offset_pattern() {
    let bytes = sample_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let q = f
        .read_ts_record(b"/A/B/FLOW/01JAN2000/1HOUR/QUAL/")
        .unwrap();
    assert_eq!(q.quality.as_ref().unwrap()[8], 1);
    assert_eq!(f32s(&q.values)[8], 8.0);
    let d = f.read_ts_record(b"/A/B/FLOW/01JAN2000/1DAY/F64Q/").unwrap();
    match &d.values {
        TsValues::F64(v) => assert_eq!(v[3], 1.0e6 + 1.0),
        _ => panic!(),
    }
    assert_eq!(d.quality.as_ref().unwrap()[365], 365 * 3);
    assert_eq!(d.times[365], secs(2001, 1, 1, 0, 0));
    let o = f
        .read_ts_record(b"/A/B/FLOW/01JAN2000/1DAY/OFFSET/")
        .unwrap();
    assert_eq!(o.time_offset_minutes, 480);
    assert_eq!(o.times[0], secs(2000, 1, 1, 8, 0));
    let p = f
        .read_ts_record(b"/A/B/FLOW/TS-PATTERN/1HOUR/PAT/")
        .unwrap();
    assert_eq!(p.layout, TsLayout::RegularPattern);
    // KINDAT (24) sizes patterns, not the stale KILNDA (99).
    assert_eq!(p.len(), 24);
}

#[test]
fn compression_and_refusals() {
    let bytes = sample_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let c = f
        .read_ts_record(b"/A/B/FLOW/01JAN2000/1HOUR/COMP3/")
        .unwrap();
    assert_eq!(c.layout, TsLayout::Compressed(3));
    let v = f32s(&c.values);
    assert_eq!(&v[0..4], &[10.0; 4]);
    assert_eq!(v[4], 10.0 + 0.1f32);
    assert_eq!(v[12], -901.0);
    assert_eq!(v[15], -901.0);
    // P33: schemes 4/5 decode by default; `allow_sig_digits = false` refuses them.
    let off = TsReadOptions {
        allow_sig_digits: false,
        ..TsReadOptions::default()
    };
    assert!(matches!(
        f.read_ts_record_with(b"/A/B/FLOW/01JAN2000/1HOUR/COMP4/", &off),
        Err(TsError::UnsupportedCompression { scheme: 4 })
    ));
    let s = f
        .read_ts_record(b"/A/B/FLOW/01JAN2000/1HOUR/COMP4/")
        .unwrap();
    assert!(f32s(&s.values).iter().all(|&x| x == 0.0));
    assert!(matches!(
        f.read_ts_record(b"/A/B/FLOW/01JAN2000/1HOUR/PASSWD/"),
        Err(TsError::PasswordProtected)
    ));
    assert!(matches!(
        f.read_ts_record(b"/A/B/CURVE///PD/"),
        Err(TsError::UnsupportedRecordType { record_type: 200 })
    ));
    assert!(matches!(
        f.read_ts_record(b"/A/B/NOPE/01JAN2000/1HOUR/F32/"),
        Err(TsError::NotFound { .. })
    ));
}

#[test]
fn irregular_and_pseudo() {
    let bytes = sample_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let r = f
        .read_ts_record(b"/A/B/STAGE/01JAN2000/IR-MONTH/ITS/")
        .unwrap();
    // NPAIRS (5), not KINDAT/2 (spare capacity, O18).
    assert_eq!(r.len(), 5);
    assert_eq!(r.irregular_capacity, Some(8));
    assert_eq!(r.times[1], secs(2000, 1, 4, 1, 30));
    let d = f
        .read_ts_record(b"/A/B/STAGE/01JAN2000/IR-YEAR/ITDQ/")
        .unwrap();
    assert_eq!(d.quality.as_deref(), Some(&[100, 101, 102][..]));
    match &d.values {
        TsValues::F64(v) => assert_eq!(v[2], 5.0),
        _ => panic!(),
    }
    // A `~1DAY` pathname finds the stored IR-YEAR record.
    let p = f
        .read_ts_record(b"/A/B/FLOW/01JAN2000/~1DAY/PSEUDO/")
        .unwrap();
    assert_eq!(p.pseudo_interval_minutes, Some(1440));
    let ds = f
        .read_ts_dataset(b"/A/B/FLOW//~1DAY/PSEUDO/", &DatasetOptions::whole())
        .unwrap();
    assert_eq!(ds.series.len(), 10);
}

#[test]
fn regular_dataset_stitch_window_trim() {
    let bytes = sample_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    // Whole dataset: Jan, (Feb absent -> -902), Mar; leading -901 trimmed.
    let w = f
        .read_ts_dataset(b"/A/B/FLOW//1HOUR/F32/", &DatasetOptions::whole())
        .unwrap();
    let v = f32s(&w.series.values);
    assert_eq!(w.report.blocks_found, 2);
    assert_eq!(w.report.blocks_missing, 1);
    assert_eq!(w.report.trimmed, 10);
    assert_eq!(v.len(), 744 - 10 + 696 + 744);
    assert_eq!(v[734], -902.0);
    assert_eq!(w.series.times[0], secs(2000, 1, 1, 11, 0));
    // Window off the grid: start and end move up to the next stamp.
    let o = DatasetOptions::window(secs(2000, 1, 31, 22, 30), secs(2000, 2, 1, 1, 10));
    let x = f.read_ts_dataset(b"/A/B/FLOW/XXXX/1HOUR/F32/", &o).unwrap();
    assert_eq!(
        x.series.times,
        vec![
            secs(2000, 1, 31, 23, 0),
            secs(2000, 2, 1, 0, 0),
            secs(2000, 2, 1, 1, 0),
            secs(2000, 2, 1, 2, 0)
        ]
    );
    assert_eq!(f32s(&x.series.values)[2], -902.0);
    // Window wholly inside the absent February block.
    let e = DatasetOptions::window(secs(2000, 2, 10, 0, 0), secs(2000, 2, 11, 0, 0));
    assert!(matches!(
        f.read_ts_dataset(b"/A/B/FLOW//1HOUR/F32/", &e),
        Ok(_) | Err(TsError::NotFound { .. })
    ));
    // Irregular window, inclusive at both ends, across blocks.
    let i = DatasetOptions::window(secs(2000, 1, 13, 1, 30), secs(2000, 2, 4, 1, 30));
    let r = f.read_ts_dataset(b"/A/B/STAGE//IR-MONTH/ITS/", &i).unwrap();
    assert_eq!(r.series.len(), 3);
    // Catalog-blocks mode reads the same stored values.
    let mut cb = DatasetOptions::whole();
    cb.catalog_blocks = true;
    let c = f.read_ts_dataset(b"/A/B/FLOW//1HOUR/F32/", &cb).unwrap();
    assert_eq!(c.series.values, w.series.values);
    assert_eq!(c.series.times, w.series.times);
}

#[test]
fn offset_window_selects_standard_stamps() {
    let bytes = sample_file();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    // Standard stamps 03JAN 24:00 .. 05JAN 24:00 move to 03JAN 08:00 .. 05JAN 08:00.
    let o = DatasetOptions::window(secs(2000, 1, 4, 0, 0), secs(2000, 1, 6, 0, 0));
    let r = f.read_ts_dataset(b"/A/B/FLOW//1DAY/OFFSET/", &o).unwrap();
    assert_eq!(r.series.times.first(), Some(&secs(2000, 1, 3, 8, 0)));
    assert_eq!(r.series.len(), 3);
    assert_eq!(r.report.time_offset_minutes, Some(480));
}

//! Paired-data reads on synthetic DSS6 and DSS7 files (packet P10): PDD, padded units,
//! time zone, labels, long DSS6 labels and inconsistent sizes. No external files.

mod pd_support;

use dss_core::dss6::{Dss6File, LabelStyle};
use dss_core::dss7::{Dss7File, PdError};
use dss_core::{PdValues, ValueWidth};
use pd_support::{dss7_file, f32_ints, ints, pd7_header, standard_dss6, standard_dss7, Pd7};

#[test]
fn dss7_float_record_with_labels_units_and_time_zone() {
    let bytes = standard_dss7();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let r = f.read_pd(b"/BASIN/LOC/STAGE-DAMAGE///F/").unwrap();
    let p = &r.pd;
    assert_eq!((p.number_ordinates, p.number_curves), (3, 3));
    assert_eq!(p.width(), ValueWidth::F32);
    assert_eq!(p.ordinates, PdValues::F32(vec![1.0, 2.0, 3.5]));
    // Missing marker kept as stored.
    assert_eq!(p.curve_f64(2).unwrap(), vec![-901.0, 22.0, 32.0]);
    assert_eq!(p.labels, vec!["Total", "Residential", "Commercial"]);
    assert_eq!(r.meta.labels_raw, b"Total\0Residential\0Commercial\0");
    assert_eq!(
        (
            p.units_independent.as_str(),
            p.type_independent.as_str(),
            p.units_dependent.as_str(),
            p.type_dependent.as_str()
        ),
        ("FEET", "Linear", "DOLLARS", "Log")
    );
    assert_eq!(p.time_zone.as_deref(), Some("UTC"));
    assert!(r.meta.units_padded);
    assert_eq!((p.x_precision, p.y_precision), (2, 1));
    assert_eq!(p.independent_is_x_axis, 1);
    assert_eq!(p.user_header, vec![7, 8]);
    assert_eq!(p.parameters(), ("STAGE".into(), "DAMAGE".into()));
}

#[test]
fn dss7_double_record_keeps_bits() {
    let bytes = standard_dss7();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let p = f.read_pd(b"/BASIN/LOC/FLOW-STAGE///DOUBLE/").unwrap().pd;
    assert_eq!(p.record_type, 205);
    assert_eq!(p.width(), ValueWidth::F64);
    assert_eq!(p.ordinates, PdValues::F64(vec![0.1, 1e300]));
    match &p.values {
        PdValues::F64(v) => {
            assert_eq!(v[0].to_bits(), f64::MIN_POSITIVE.to_bits());
            assert_eq!(v[1].to_bits(), (-0.0f64).to_bits());
        }
        _ => panic!(),
    }
    assert!(p.labels.is_empty());
    assert_eq!((p.x_precision, p.y_precision), (-1, -1));
    assert_eq!(p.time_zone, None);
    assert!(matches!(
        f.read_pd(b"/BASIN/LOC/FLOW/01Jan2020/1Hour/TS/"),
        Err(PdError::UnsupportedRecordType {
            record_type: 100,
            ..
        })
    ));
}

#[test]
fn dss7_record_at_matches_lookup() {
    let bytes = standard_dss7();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    let cat = f.catalog().unwrap();
    let i = cat.find(b"/BASIN/LOC/STAGE-DAMAGE///F/").unwrap();
    let rec = f.record_at(&cat, i).unwrap();
    let a = f.read_pd_record(&rec).unwrap();
    let b = f.read_pd(b"/basin/loc/stage-damage///f/").unwrap();
    assert_eq!(a, b);
}

fn corrupt_case(p: Pd7) -> Result<dss_core::dss7::Dss7Pd, PdError> {
    let bytes = dss7_file(&[("/A/B/X-Y///F/", 200, Some(p))]);
    let mut f = Dss7File::from_bytes(&bytes).unwrap();
    f.read_pd(b"/A/B/X-Y///F/")
}

fn base() -> Pd7 {
    Pd7 {
        ih: pd7_header(2, 1, 0, -1, &["FT", "UNT", "CFS", "UNT"]),
        values1: f32_ints(&[1.0, 2.0]),
        values2: f32_ints(&[3.0, 4.0]),
        ..Pd7::default()
    }
}

#[test]
fn dss7_inconsistent_sizes_are_corrupt() {
    assert!(corrupt_case(base()).is_ok());
    let mut p = base();
    p.ih_n = Some(4);
    assert!(matches!(corrupt_case(p), Err(PdError::Corrupt { .. })));
    let mut p = base();
    p.ih[0] = 3; // 3 ordinates declared, 2 stored
    assert!(matches!(corrupt_case(p), Err(PdError::Corrupt { .. })));
    let mut p = base();
    p.ih[1] = 2; // 2 curves declared, 1 stored
    assert!(matches!(corrupt_case(p), Err(PdError::Corrupt { .. })));
    let mut p = base();
    p.ih[0] = -1;
    assert!(matches!(corrupt_case(p), Err(PdError::Corrupt { .. })));
    let mut p = base();
    p.v2_n = Some(1 << 40);
    assert!(matches!(corrupt_case(p), Err(PdError::Corrupt { .. })));
    let mut p = base();
    p.v1_n = Some(1 << 20); // inside the bounds check but past the file
    p.ih[0] = 1 << 19;
    p.ih[1] = 0;
    assert!(matches!(corrupt_case(p), Err(PdError::Corrupt { .. })));
    // More stored values than declared: extra ignored, as HEC does.
    let mut p = base();
    p.values2 = f32_ints(&[3.0, 4.0, 5.0, 6.0]);
    let r = corrupt_case(p).unwrap();
    assert!(r.meta.value_count_mismatch);
    assert_eq!(r.pd.values, PdValues::F32(vec![3.0, 4.0]));
    // Zero ordinates: an empty record, not an error.
    let mut p = base();
    p.ih[0] = 0;
    let r = corrupt_case(p).unwrap();
    assert!(r.pd.ordinates.is_empty() && r.pd.values.is_empty());
    // Labels: header2 present with labelsLength 0 gives HEC's single empty label.
    let mut p = base();
    p.header2 = ints(b"abc\0");
    assert_eq!(corrupt_case(p).unwrap().pd.labels, vec![String::new()]);
}

#[test]
fn dss6_short_and_long_labels_and_doubles() {
    let bytes = standard_dss6();
    let f = Dss6File::from_bytes(&bytes).unwrap();
    let r = f.read_pd(b"/A/B/STAGE-FLOW///SHORT/").unwrap();
    assert_eq!(r.meta.label_style, LabelStyle::Short);
    assert_eq!(r.pd.labels, vec!["  5%LIMIT", "MEDIAN"]);
    assert_eq!(r.pd.ordinates, PdValues::F32(vec![1.0, 2.0, 3.0]));
    assert_eq!(
        r.pd.values,
        PdValues::F32(vec![10.0, 20.0, 30.0, 11.0, 21.0, 31.0])
    );
    assert_eq!(r.meta.extra_data_words, 1);
    assert!(r.meta.logical_values_stale);
    assert_eq!(
        (
            r.pd.units_independent.as_str(),
            r.pd.type_dependent.as_str()
        ),
        ("FEET", "UNT")
    );
    let r = f.read_pd(b"/A/B/ELEV-STOR///LONG/").unwrap();
    assert_eq!(r.meta.label_style, LabelStyle::Long { words_per_label: 4 });
    assert_eq!(r.pd.labels, vec!["A first long lab", "Second"]);
    assert_eq!(r.pd.width(), ValueWidth::F64);
    assert_eq!(r.pd.ordinates, PdValues::F64(vec![100.0, 200.5]));
    assert_eq!(r.pd.curve_f64(1).unwrap(), vec![3.0, 4.0]);
    assert_eq!(r.pd.units_dependent, "ACRE-FT");
    assert!(!r.meta.logical_values_stale);
    assert!(matches!(
        f.read_pd(b"/A/B/FLOW/01JAN2000/1HOUR/TS/"),
        Err(dss_core::dss6::PdError::UnsupportedRecordType { record_type: 100 })
    ));
}

#[test]
fn readers_do_not_need_write_access() {
    // Open the committed fixtures through a read-only handle and check nothing changes.
    for name in ["sample7.dss", "castro.dss"] {
        let p = pd_support::fixture(name);
        let before = std::fs::metadata(&p).unwrap();
        if name.ends_with("7.dss") {
            let mut f = Dss7File::open(&p).unwrap();
            let _ = f.read_pd(b"/TEST/COWLITZ/FREQ-FLOW/EXAMPLE///").unwrap();
        } else {
            let f = Dss6File::open(&p).unwrap();
            let _ = f.read_pd(b"//TABLE 1/STORAGE-FLOW///TABLE/").unwrap();
        }
        let after = std::fs::metadata(&p).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    }
}

/// Public synthetic fixtures for the Python binding (P12): `standard_dss6()` and
/// `standard_dss7()` as `fixtures/public/synthetic_pd{6,7}.dss` (PDD, DSS6 long labels,
/// DSS7 labels and time zone, plus one time-series record). To regenerate, set
/// DSS_PY_FIXTURE_OUT to a scratch directory, run this test, and copy both files into
/// fixtures/public/. When the committed files exist the test checks they still match.
#[test]
fn synthetic_pd_fixtures() {
    let files = [
        ("synthetic_pd6.dss", standard_dss6()),
        ("synthetic_pd7.dss", standard_dss7()),
    ];
    let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    for (name, bytes) in &files {
        if let Ok(committed) = std::fs::read(public.join(name)) {
            assert!(
                committed == *bytes,
                "{name} differs from the generator; regenerate it"
            );
        }
    }
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        for (name, bytes) in &files {
            std::fs::write(out.join(name), bytes).unwrap();
        }
    }
}

//! Paired-data reads on the public fixtures in `fixtures/public/`, so PD
//! coverage runs in CI. Expected values come from HEC reads of the same files: `hecdss`
//! for the DSS7 fixtures and HEC Monolith for the DSS6 fixtures. The
//! checksum is FNV-1a 64 over every PD record sorted by pathname bytes: pathname, then
//! ordinates, then values, little-endian in the stored width (computed from the arrays
//! HEC's readers returned).

mod pd_support;

use dss_core::dss6::{Dss6File, InfoCheck};
use dss_core::dss7::Dss7File;
use dss_core::{PairedData, PdValues, ValueWidth};
use pd_support::fixture;

fn fnv(mut h: u64, b: &[u8]) -> u64 {
    for &x in b {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

fn le(v: &PdValues) -> Vec<u8> {
    match v {
        PdValues::F32(x) => x.iter().flat_map(|f| f.to_le_bytes()).collect(),
        PdValues::F64(x) => x.iter().flat_map(|f| f.to_le_bytes()).collect(),
    }
}

/// Every live PD record of a fixture, sorted by pathname.
fn all_pd(name: &str) -> Vec<PairedData> {
    let p = fixture(name);
    let bytes = std::fs::read(&p).unwrap();
    let mut out = Vec::new();
    if bytes.get(16) == Some(&b'7') {
        let mut f = Dss7File::from_bytes(&bytes).unwrap();
        let cat = f.catalog().unwrap();
        for e in cat.catalog.live() {
            if e.record_type == 200 || e.record_type == 205 {
                out.push(f.read_pd(&e.pathname).unwrap().pd);
            }
        }
    } else {
        let f = Dss6File::from_bytes(&bytes).unwrap();
        let opts = dss_core::dss6::CatalogOptions {
            verify_info: InfoCheck::None,
            ..Default::default()
        };
        for e in f.walk_bins(&opts).unwrap().live() {
            if e.record_type == 200 || e.record_type == 205 {
                let info = f
                    .read_info(e.info_address.into(), e.pathname.len() as i32)
                    .unwrap();
                out.push(f.decode_pd_info(&info).unwrap().pd);
            }
        }
    }
    out.sort_by(|a, b| a.pathname.cmp(&b.pathname));
    out
}

fn checksum(pds: &[PairedData]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325;
    for p in pds {
        h = fnv(h, &p.pathname);
        h = fnv(h, &le(&p.ordinates));
        h = fnv(h, &le(&p.values));
    }
    h
}

fn f32s(v: &PdValues) -> &[f32] {
    match v {
        PdValues::F32(x) => x,
        PdValues::F64(_) => panic!("expected f32"),
    }
}

struct Expect {
    file: &'static str,
    pd: usize,
    multi_curve: usize,
    labeled: usize,
    fnv: u64,
}

const EXPECT: &[Expect] = &[
    Expect {
        file: "sample7.dss",
        pd: 4,
        multi_curve: 1,
        labeled: 1,
        fnv: 0xfd56_78c6_bb0c_fb28,
    },
    Expect {
        file: "sample6.dss",
        pd: 5,
        multi_curve: 1,
        labeled: 2,
        fnv: 0x45dc_d639_5852_dce7,
    },
    Expect {
        file: "castro.dss",
        pd: 1,
        multi_curve: 0,
        labeled: 0,
        fnv: 0xe897_95b6_47a2_8702,
    },
    Expect {
        file: "Muncie.dss",
        pd: 714,
        multi_curve: 0,
        labeled: 0,
        fnv: 0xa498_7153_2358_2b41,
    },
    Expect {
        file: "sample_vb.dss",
        pd: 1,
        multi_curve: 0,
        labeled: 0,
        fnv: 0xe4d9_54b8_ee5c_b50f,
    },
    Expect {
        file: "BaldEagleDamBrk.dss",
        pd: 0,
        multi_curve: 0,
        labeled: 0,
        fnv: 0xcbf2_9ce4_8422_2325,
    },
];

#[test]
fn public_fixtures_match_oracle_checksums() {
    for e in EXPECT {
        let pds = all_pd(e.file);
        assert_eq!(pds.len(), e.pd, "{}", e.file);
        let multi = pds.iter().filter(|p| p.number_curves > 1).count();
        let labeled = pds.iter().filter(|p| !p.labels.is_empty()).count();
        assert_eq!((multi, labeled), (e.multi_curve, e.labeled), "{}", e.file);
        for p in &pds {
            assert_eq!(p.width(), ValueWidth::F32, "{}", e.file);
            assert_eq!(p.ordinates.len(), p.number_ordinates);
            assert_eq!(p.values.len(), p.number_ordinates * p.number_curves);
        }
        assert_eq!(checksum(&pds), e.fnv, "{}", e.file);
    }
}

#[test]
fn sample7_four_curve_frequency_record() {
    let mut f = Dss7File::open(fixture("sample7.dss")).unwrap();
    let r = f
        .read_pd(b"/NE ANACOSTIA/RIVERDALE/FREQ-FLOW/MAX ANALYTICAL//1969-01 H33(MAX)/")
        .unwrap();
    let p = &r.pd;
    assert_eq!((p.number_ordinates, p.number_curves), (12, 4));
    assert_eq!(
        (
            p.units_independent.as_str(),
            p.type_independent.as_str(),
            p.units_dependent.as_str(),
            p.type_dependent.as_str()
        ),
        ("PERCENT", "FREQ", "CFS", "FLOW")
    );
    // hecdss keeps the leading blank of " 5%LIMIT" (zpdRetrieve() copies header2 as is).
    assert_eq!(
        p.labels,
        vec!["COMPUTED", "EXP PROB", " 5%LIMIT", "95%LIMIT"]
    );
    assert_eq!(
        f32s(&p.ordinates),
        &[0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 80.0, 90.0, 95.0]
    );
    let v = f32s(&p.values);
    assert_eq!(v[0], 20912.982);
    assert_eq!(v[11], 2652.7788);
    assert_eq!(v[12], 24993.01);
    assert_eq!(v[47], 2153.5723);
    assert_eq!(p.curve_f64(3).unwrap()[0], f64::from(16020.89f32));
    assert_eq!(p.parameters(), ("FREQ".into(), "FLOW".into()));
    assert_eq!(p.time_zone, None);
    // 7-10 file: deprecated (unpadded) unit strings from int 5.
    assert!(!r.meta.units_padded);
}

#[test]
fn sample7_empty_first_unit_hides_the_rest() {
    // zpdHeadToUnits() stops at the first empty string; hecdss returns all four empty
    // for this record although the DSS6 original (sample6.dss) stores types.
    let mut f = Dss7File::open(fixture("sample7.dss")).unwrap();
    let p = f
        .read_pd(b"/FISHKILL CREEK/BEACON NY/FREQ-FLOW///USGS/")
        .unwrap()
        .pd;
    assert_eq!((p.number_ordinates, p.number_curves), (24, 1));
    assert!(p.units_independent.is_empty() && p.type_independent.is_empty());
    assert!(p.units_dependent.is_empty() && p.type_dependent.is_empty());
    assert_eq!(f32s(&p.ordinates)[0], 0.04);
    assert_eq!(f32s(&p.values)[23], 780.0);

    let f6 = Dss6File::open(fixture("sample6.dss")).unwrap();
    let q = f6
        .read_pd(b"/FISHKILL CREEK/BEACON NY/FREQ-FLOW///USGS/")
        .unwrap()
        .pd;
    assert_eq!(
        (
            q.units_independent.as_str(),
            q.type_independent.as_str(),
            q.units_dependent.as_str(),
            q.type_dependent.as_str()
        ),
        ("", "PROB", "cfs", "LOG")
    );
    assert_eq!(le(&p.ordinates), le(&q.ordinates));
    assert_eq!(le(&p.values), le(&q.values));
}

#[test]
fn sample6_short_labels_and_stale_klnda() {
    let f = Dss6File::open(fixture("sample6.dss")).unwrap();
    let r = f
        .read_pd(b"/NE ANACOSTIA/RIVERDALE/FREQ-FLOW/MAX ANALYTICAL//1969-01 H33(MAX)/")
        .unwrap();
    // Same record as in sample7.dss. HEC's C layer keeps the leading blank (CHRLNB trims
    // trailing blanks only); Monolith's Java layer trims it to "5%LIMIT".
    assert_eq!(
        r.pd.labels,
        vec!["COMPUTED", "EXP PROB", " 5%LIMIT", "95%LIMIT"]
    );
    assert_eq!(r.meta.label_style, dss_core::dss6::LabelStyle::Short);
    assert_eq!((r.pd.x_precision, r.pd.y_precision), (0, 2));
    let r = f
        .read_pd(b"/AMERICAN/FOLSOM/FLOW-RES IN - FLOW-RES OUT///OBS/")
        .unwrap();
    assert_eq!(r.pd.labels, vec!["FLOW-RES IN"]);
    assert_eq!(r.pd.number_ordinates, 31);
    // KILNDA is 365 although the record holds 31 * 2 values (`docs/format/dss6.md` §12, item O9).
    assert_eq!((r.meta.logical_values, r.meta.data_words), (365, 62));
    assert!(r.meta.logical_values_stale);
    assert_eq!(f32s(&r.pd.ordinates)[0], 7406.0);
    assert_eq!(f32s(&r.pd.values)[30], 32804.0);
}

#[test]
fn castro_storage_outflow_table() {
    let f = Dss6File::open(fixture("castro.dss")).unwrap();
    let r = f.read_pd(b"//TABLE 1/STORAGE-FLOW///TABLE/").unwrap();
    let p = &r.pd;
    assert_eq!(p.record_type, 200);
    assert_eq!(
        f32s(&p.ordinates),
        &[0.0, 0.2, 0.5, 0.8, 1.0, 1.5, 2.7, 4.5, 750.0, 5000.0]
    );
    assert_eq!(
        f32s(&p.values),
        &[0.0, 2.0, 10.0, 20.0, 30.0, 50.0, 80.0, 120.0, 1500.0, 3000.0]
    );
    assert_eq!(
        (p.units_independent.as_str(), p.units_dependent.as_str()),
        ("ACRE-FT", "CFS")
    );
    assert_eq!(
        (p.type_independent.as_str(), p.type_dependent.as_str()),
        ("UNT", "UNT")
    );
    assert!(p.labels.is_empty());
    assert_eq!(p.parameters(), ("STORAGE".into(), "FLOW".into()));
    // castro: KINDAT 20, KILNDA 144 (dss6.md §5.2).
    assert_eq!((r.meta.data_words, r.meta.logical_values), (20, 144));
}

#[test]
fn muncie_profile_record() {
    let f = Dss6File::open(fixture("Muncie.dss")).unwrap();
    let p = f
        .read_pd(b"/WHITE MUNCIE//LOCATION-ELEV//01JAN1900 2400/2D 50FT GRID/")
        .unwrap()
        .pd;
    assert_eq!((p.number_ordinates, p.number_curves), (61, 1));
    assert_eq!(
        (p.units_independent.as_str(), p.units_dependent.as_str()),
        ("Riv Sta", "FEET")
    );
    assert_eq!(f64::from(f32s(&p.ordinates)[0]), 15696.240234375);
    assert_eq!(f64::from(f32s(&p.ordinates)[60]), 237.64549255371094);
    assert_eq!(f64::from(f32s(&p.values)[0]), 950.8037719726562);
    assert_eq!(f64::from(f32s(&p.values)[60]), 920.3788452148438);
}

#[test]
fn non_pd_records_are_typed_errors() {
    let mut f = Dss7File::open(fixture("sample7.dss")).unwrap();
    match f.read_pd(b"//SACRAMENTO/TEMP-MAX/01Jan1878/1Day/OBS/") {
        Err(dss_core::dss7::PdError::UnsupportedRecordType { record_type, .. }) => {
            assert_eq!(record_type, 100)
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        f.read_pd(b"/NO/SUCH/RECORD///X/"),
        Err(dss_core::dss7::PdError::NotFound { .. })
    ));
    let f6 = Dss6File::open(fixture("castro.dss")).unwrap();
    let cat = f6.catalog().unwrap();
    let ts = cat
        .entries
        .iter()
        .find(|e| e.is_live() && e.record_type == 100)
        .unwrap();
    assert!(matches!(
        f6.read_pd(&ts.pathname),
        Err(dss_core::dss6::PdError::UnsupportedRecordType { record_type: 100 })
    ));
    assert!(matches!(
        f6.read_pd_entry(ts),
        Err(dss_core::dss6::PdError::UnsupportedRecordType { record_type: 100 })
    ));
    assert!(matches!(
        f6.read_pd(b"/NO/SUCH/RECORD///X/"),
        Err(dss_core::dss6::PdError::NotFound { .. })
    ));
}

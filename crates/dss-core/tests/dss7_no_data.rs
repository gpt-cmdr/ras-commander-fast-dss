//! DSS7 is outside HEC's DSS6 "no data" rule. `heclib_c/src/Internal/
//! ztsRetrieveReg7.c` (hec-dss fork `main`, `6591930`) sets `foundOne = 1` for a block read
//! whatever its values (line 641) and returns `STATUS_RECORD_FOUND` (lines 699-700).
//!
//! `fixtures/public/synthetic_no_data7.dss` is generated here (no HEC software; regenerate
//! with `DSS_PY_FIXTURE_OUT=<dir>`; the test fails when the committed bytes differ). It
//! holds one DSS7 regular float block (type 100) whose values are all -901 or -902, so the
//! Python test (`test_no_data6.py::test_dss7_all_missing_block_is_unaffected`) can show that
//! `no_data="refuse"` does not reach DSS7. The layout follows `python_regression_fixtures.rs`
//! (values1 raw floats sized by `NUMBER_DATA`; a 17-word internal header without strings or compression). All
//! names and values are invented.

mod dss7_common;

use dss7_common::{build, set_word, Rec, Spec};
use dss_core::dss7::{info::key, Dss7File};
use dss_core::timeseries::TsValues;
use std::path::PathBuf;

pub const PATH7: &str = "/P29/GAGE D7/FLOW/01JAN2000/1DAY/OBS/";
const N: usize = 366;

fn value(i: usize) -> f32 {
    if i % 2 == 0 {
        -901.0
    } else {
        -902.0
    }
}

fn file() -> Vec<u8> {
    // Bin first/last dates: 01JAN2000 and 31DEC2000 (HEC julian days from 31DEC1899).
    let mut rec = Rec::live(PATH7, 100);
    rec.julian = (36526, 36526 + 365);
    let built = build(&Spec {
        recs: vec![rec],
        ..Default::default()
    });
    let mut bytes = built.bytes;
    let info = built.info_addr[0] as usize;
    // heclib_c/src/Internal/zwriteNew.c:zwriteNew and zinit.c:zinit: info addresses count
    // int64 words; area lengths count int32 words.
    let ih_addr = bytes.len() / 8 - 1; // replace the old EOF with the payload
    bytes.truncate(ih_addr * 8);
    // Internal header (headers/zStructAllocation.h INT_HEAD_*; 17 words, no strings):
    // block first/last valid positions 0 and N-1, N values of one 4-byte word, no
    // compression, quality or notes.
    let mut ih = [0i32; 18];
    ih[5] = N as i32 - 1;
    ih[6] = N as i32;
    ih[7] = 1;
    ih[8] = 1;
    for w in ih {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let data_addr = bytes.len() / 8;
    for i in 0..N {
        bytes.extend_from_slice(&value(i).to_le_bytes());
    }
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(
        &mut bytes,
        info + key::INTERNAL_HEAD_ADDRESS,
        ih_addr as i64,
    );
    set_word(&mut bytes, info + key::INTERNAL_HEAD_NUMBER, 17);
    set_word(&mut bytes, info + key::VALUES1_ADDRESS, data_addr as i64);
    set_word(&mut bytes, info + key::VALUES1_NUMBER, N as i64);
    set_word(&mut bytes, info + key::NUMBER_DATA, N as i64);
    set_word(&mut bytes, info + key::LOGICAL_NUMBER, N as i64);
    set_word(&mut bytes, info + key::ALLOCATED_SIZE, N.div_ceil(2) as i64);
    set_word(&mut bytes, 5, eof as i64);
    bytes
}

#[test]
fn dss7_all_missing_block_is_read() {
    let bytes = file();
    let path = std::env::temp_dir().join(format!("p29_no_data7_{}.dss", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    let mut f = Dss7File::open(&path).unwrap();
    let rec = f.lookup(PATH7.as_bytes()).unwrap().expect("record");
    let ts = f.read_ts_record(&rec).unwrap();
    let v = match &ts.ts.values {
        TsValues::F32(v) => v.clone(),
        other => panic!("unexpected width {other:?}"),
    };
    assert_eq!(v.len(), N);
    assert!(v.iter().enumerate().all(|(i, &x)| x == value(i)));
    drop(f);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn committed_fixture_matches_generator() {
    let bytes = file();
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_no_data7.dss"), &bytes).unwrap();
    }
    let committed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_no_data7.dss");
    let on_disk = std::fs::read(&committed).expect("public fixture missing");
    assert_eq!(
        on_disk, bytes,
        "committed fixture differs from the generator"
    );
}

//! Regression for P09a defect B1 (packet P09c): a DSS6 dataset whose blocks disagree in
//! units, data type or time zone reports the LAST block it reads, as HEC does.
//!
//! HEC re-reads the internal header for every block (`zrrtsi.f`, `zritsi6.f`), so the
//! values left in the caller's arguments are the last block's. P09a found this on the
//! private corpus file g54; these tests need no external data.
//!
//! - `fixtures/public/b1_multiblock6.dss` is written by HEC Monolith 3.3.27
//!   (`oracle/synth_b1.py`). `oracle/read_b1_monolith.py` reads it back with Monolith;
//!   those answers are the expected values below (`evidence/P09c`).
//! - A second file is built in memory by `dss6_ts_support` (no HEC software) so the
//!   header rule is also covered when the fixture is absent.

mod dss6_ts_support;

use dss6_ts_support::{f32w, its_header, jul, rts_header, secs, ts_file, Payload, Rec};
use dss_core::dss6::{DatasetOptions, Dss6File, ReadAt};
use dss_core::TsValues;

/// (units, data type) per month block, all distinct so first, middle and last differ.
const HEADERS: [(&str, &str); 3] = [
    ("CFS", "INST-VAL"),
    ("cms", "PER-AVER"),
    ("ft3/s", "INST-CUM"),
];
const SAME: [(&str, &str); 3] = [("CFS", "INST-VAL"); 3];
/// Days in Jan, Feb, Mar 2001.
const DAYS: [u32; 3] = [31, 28, 31];
const MONTHS: [&str; 3] = ["JAN", "FEB", "MAR"];

const REG: &str = "/B1/REG/FLOW//1HOUR/MULTI/";
const UNI: &str = "/B1/UNI/FLOW//1HOUR/UNIFORM/";
/// Irregular block capacity in values; HEC allocates spare room in each block.
const CAP: i32 = 200;
const IRR: &str = "/B1/IRR/STAGE//IR-MONTH/MULTI/";

fn block_path(whole: &str, month: &str) -> String {
    // Blank D part -> the block's own D part.
    let mut parts: Vec<&str> = whole.split('/').collect();
    let d = format!("01{month}2001");
    parts[4] = &d;
    parts.join("/")
}

fn build_file() -> Vec<u8> {
    let mut recs = Vec::new();
    for (whole, code) in [(REG, 100), (UNI, 100), (IRR, 110)] {
        for m in MONTHS {
            recs.push(Rec::live(&block_path(whole, m), code));
        }
    }
    let mut f = ts_file(&recs, *b"6-YO");
    for (whole, headers) in [(REG, HEADERS), (UNI, SAME)] {
        for (k, m) in MONTHS.iter().enumerate() {
            let n = (DAYS[k] * 24) as i32;
            let (units, dtype) = headers[k];
            f.attach(
                &block_path(whole, m),
                &Payload {
                    internal_header: rts_header(0, units, dtype, 24),
                    data: (0..n)
                        .map(|i| f32w(100.0 * (k + 1) as f32 + i as f32 * 0.5))
                        .collect(),
                    kilnda: n,
                    ..Default::default()
                },
            );
        }
    }
    for (k, m) in MONTHS.iter().enumerate() {
        let (units, dtype) = HEADERS[k];
        let j0 = jul(2001, k as u32 + 1, 1) as i32;
        let mut d = Vec::new();
        let mut last = 0;
        for i in 0..40 {
            // Minutes into the block, then the value (ITS layout, dss6.md).
            last = i * 397 + (i % 3) * 11 + 183;
            d.push(last);
            d.push(f32w(10.0 * (k + 1) as f32 + i as f32 * 0.25));
        }
        // Word 4 is the time of the last value; HEC sizes whole-dataset reads from it
        // (zritsi6.f lines 724-746).
        let mut header = its_header(CAP, 40, j0, 0, units, dtype);
        header[3] = last;
        // Spare capacity, as HEC allocates (dss6.md section 7.3): never read.
        d.resize(2 * CAP as usize, 0x7777_7777);
        f.attach(
            &block_path(IRR, m),
            &Payload {
                internal_header: header,
                data: d,
                kilnda: 40,
                ..Default::default()
            },
        );
    }
    f.finish()
}

fn differs(warnings: &[String]) -> usize {
    warnings
        .iter()
        .filter(|w| w.contains("differs from the first block"))
        .count()
}

fn f32s(v: &TsValues) -> &[f32] {
    match v {
        TsValues::F32(x) => x,
        _ => panic!("expected f32"),
    }
}

/// The header rule, on any file laid out like `b1_multiblock6.dss`.
fn check_headers<S: ReadAt>(f: &Dss6File<S>) {
    // Regular, whole dataset: three blocks; the last block's header wins.
    let w = f
        .read_ts_dataset(REG.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(w.report.blocks_found, 3);
    assert_eq!(
        (w.series.units.as_str(), w.series.data_type.as_str()),
        ("ft3/s", "INST-CUM"),
        "regular whole dataset must report the last block's header"
    );
    assert_eq!(differs(&w.report.warnings), 2, "{:?}", w.report.warnings);
    // Values are untouched by the header choice: 744 + 672 + 744, block k starts at 100k.
    let v = f32s(&w.series.values);
    assert_eq!(v.len(), 744 + 672 + 744);
    assert_eq!(v[0], 100.0);
    assert_eq!(v[744], 200.0);
    assert_eq!(v[744 + 672], 300.0);

    // Window over January and February only: February is the last block read.
    let o = DatasetOptions::window(secs(2001, 1, 31, 22, 0), secs(2001, 2, 2, 0, 0));
    let x = f.read_ts_dataset(REG.as_bytes(), &o).unwrap();
    assert_eq!(x.report.blocks_found, 2);
    assert_eq!(
        (x.series.units.as_str(), x.series.data_type.as_str()),
        ("cms", "PER-AVER"),
        "windowed read reports the last block it read, not the last in the file"
    );
    assert_eq!(differs(&x.report.warnings), 1, "{:?}", x.report.warnings);

    // Window inside January: one block, nothing differs.
    let j = DatasetOptions::window(secs(2001, 1, 5, 0, 0), secs(2001, 1, 6, 0, 0));
    let j = f.read_ts_dataset(REG.as_bytes(), &j).unwrap();
    assert_eq!(
        (j.series.units.as_str(), j.series.data_type.as_str()),
        ("CFS", "INST-VAL")
    );
    assert_eq!(differs(&j.report.warnings), 0);

    // catalog_blocks mode reads the same blocks through the index.
    let mut cb = DatasetOptions::whole();
    cb.catalog_blocks = true;
    let c = f.read_ts_dataset(REG.as_bytes(), &cb).unwrap();
    assert_eq!(
        (c.series.units.as_str(), c.series.data_type.as_str()),
        ("ft3/s", "INST-CUM"),
        "catalog_blocks must also report the last block's header"
    );
    assert_eq!(c.series.values, w.series.values);
    assert_eq!(c.series.times, w.series.times);
    assert_eq!(differs(&c.report.warnings), 2, "{:?}", c.report.warnings);

    // Irregular, whole dataset (index path).
    let iw = f
        .read_ts_dataset(IRR.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(iw.series.len(), 120);
    assert_eq!(
        (iw.series.units.as_str(), iw.series.data_type.as_str()),
        ("ft3/s", "INST-CUM"),
        "irregular whole dataset must report the last block's header"
    );
    assert_eq!(differs(&iw.report.warnings), 2, "{:?}", iw.report.warnings);

    // Irregular, windowed over January and February.
    let o = DatasetOptions::window(secs(2001, 1, 20, 0, 0), secs(2001, 2, 10, 0, 0));
    let ix = f.read_ts_dataset(IRR.as_bytes(), &o).unwrap();
    assert_eq!(
        (ix.series.units.as_str(), ix.series.data_type.as_str()),
        ("cms", "PER-AVER"),
        "irregular windowed read reports the last block it read"
    );

    // Control: three blocks with identical headers report them and warn about nothing.
    let u = f
        .read_ts_dataset(UNI.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(
        (u.series.units.as_str(), u.series.data_type.as_str()),
        ("CFS", "INST-VAL")
    );
    assert_eq!(differs(&u.report.warnings), 0);
}

/// Counts and stamps HEC Monolith 3.3.27 returns for the same requests on the committed
/// fixture (`evidence/P09c/monolith_b1_read.json`).
fn check_monolith_numbers<S: ReadAt>(f: &Dss6File<S>) {
    let first_last = |s: &dss_core::TimeSeries| (s.times[0], *s.times.last().unwrap());
    let w = f
        .read_ts_dataset(REG.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(w.series.len(), 2160);
    assert_eq!(
        first_last(&w.series),
        (secs(2001, 1, 1, 1, 0), secs(2001, 4, 1, 0, 0))
    );
    let o = DatasetOptions::window(secs(2001, 1, 31, 22, 0), secs(2001, 2, 2, 0, 0));
    let x = f.read_ts_dataset(REG.as_bytes(), &o).unwrap();
    assert_eq!(x.series.len(), 27);
    assert_eq!(
        first_last(&x.series),
        (secs(2001, 1, 31, 22, 0), secs(2001, 2, 2, 0, 0))
    );
    let o = DatasetOptions::window(secs(2001, 1, 5, 0, 0), secs(2001, 1, 6, 0, 0));
    assert_eq!(
        f.read_ts_dataset(REG.as_bytes(), &o).unwrap().series.len(),
        25
    );
    let iw = f
        .read_ts_dataset(IRR.as_bytes(), &DatasetOptions::whole())
        .unwrap();
    assert_eq!(
        first_last(&iw.series),
        (secs(2001, 1, 2, 3, 7), secs(2001, 3, 12, 21, 10))
    );
    let o = DatasetOptions::window(secs(2001, 1, 20, 0, 0), secs(2001, 2, 10, 0, 0));
    let ix = f.read_ts_dataset(IRR.as_bytes(), &o).unwrap();
    assert_eq!(ix.series.len(), 29);
    assert_eq!(
        first_last(&ix.series),
        (secs(2001, 2, 2, 3, 7), secs(2001, 2, 9, 20, 34))
    );
}

#[test]
fn last_block_header_wins_on_synthetic_file() {
    let bytes = build_file();
    check_headers(&Dss6File::from_bytes(&bytes).unwrap());
}

#[test]
fn last_block_header_wins_on_monolith_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/b1_multiblock6.dss");
    let bytes = std::fs::read(&path).expect("fixtures/public/b1_multiblock6.dss");
    let f = Dss6File::from_bytes(&bytes).unwrap();
    check_headers(&f);
    check_monolith_numbers(&f);
}

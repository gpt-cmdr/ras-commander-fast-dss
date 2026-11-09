//! DSS6 catalog and time-series reads on the public fixtures committed in
//! `fixtures/public/`, so DSS6 coverage runs in CI. Expected counts come from the
//! HEC Monolith catalogs and record reads of the same files.

use dss_core::dss6::{Dss6File, TsReadOptions};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

struct Expect {
    file: &'static str,
    live: usize,
    ts: usize,
    compressed_min: usize,
}

const EXPECT: &[Expect] = &[
    // HEC-DSSVue sample (6-SB): quality flags, stored -902.
    Expect {
        file: "sample6.dss",
        live: 595,
        ts: 590,
        compressed_min: 0,
    },
    // HEC-HMS 4.3 sample (6-QF): smallest TS + paired-data file.
    Expect {
        file: "castro.dss",
        live: 3,
        ts: 2,
        compressed_min: 0,
    },
    // HEC-HMS 4.3 sample (6-ND): compression schemes 1 and 3.
    Expect {
        file: "green_river.dss",
        live: 30,
        ts: 30,
        compressed_min: 30,
    },
    // RasExamples Muncie (6-UF): 5MIN daily blocks, RTD, all-missing blocks.
    Expect {
        file: "Muncie.dss",
        live: 1164,
        ts: 450,
        compressed_min: 0,
    },
    // hec-dss repository test/VB/sample.dss (6-LA): compression scheme 2.
    Expect {
        file: "sample_vb.dss",
        live: 32,
        ts: 31,
        compressed_min: 9,
    },
];

fn is_ts(record_type: i32) -> bool {
    (100..200).contains(&record_type)
}

#[test]
fn public_dss6_catalogs_and_ts_records_decode() {
    for e in EXPECT {
        let f = Dss6File::open(fixture(e.file)).unwrap_or_else(|err| panic!("{}: {err}", e.file));
        let cat = f.catalog().unwrap();
        let live: Vec<_> = cat.live().collect();
        assert_eq!(live.len(), e.live, "{}: live entries", e.file);
        assert!(cat.anomalies.is_empty(), "{}: {:?}", e.file, cat.anomalies);

        let mut ts = 0;
        let mut compressed = 0;
        for entry in live.iter().filter(|x| is_ts(x.record_type)) {
            let r = f
                .read_ts_entry(entry, &TsReadOptions::default())
                .unwrap_or_else(|err| panic!("{} {}: {err}", e.file, entry.pathname_lossy()));
            assert_eq!(r.times.len(), r.values.len(), "{}", entry.pathname_lossy());
            if r.compression != 0 {
                compressed += 1;
            }
            ts += 1;
        }
        assert_eq!(ts, e.ts, "{}: time-series records", e.file);
        assert!(
            compressed >= e.compressed_min,
            "{}: compressed {compressed}",
            e.file
        );
    }
}

#[test]
fn synthetic_hec_written_dss6_decodes() {
    // Written by HEC Monolith through its Java API: calendar intervals,
    // offsets, weekly, quality, IR-MONTH and a pseudo-regular ~1DAY dataset.
    let f = Dss6File::open(fixture("synth6.dss")).unwrap();
    let cat = f.catalog().unwrap();
    assert!(cat.anomalies.is_empty(), "{:?}", cat.anomalies);
    let mut n = 0;
    for entry in cat.live().filter(|x| is_ts(x.record_type)) {
        f.read_ts_entry(entry, &TsReadOptions::default())
            .unwrap_or_else(|err| panic!("{}: {err}", entry.pathname_lossy()));
        n += 1;
    }
    assert!(n > 0);
}

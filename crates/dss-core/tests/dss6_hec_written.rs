//! P33: DSS6 variants written by HEC software, checked against values HEC read back.
//!
//! Fixtures (written by HEC Monolith 3.3.27, javaHeclib `7-IU`, through its Java API;
//! not regenerated here):
//! - `synthetic_hec_compress6.dss`: regular float blocks stored through `Heclib.zsrtsx` with
//!   compression scheme `JCOMP` 1-5 (`zsrtsx.f` -> `zsrtsi6.f` lines 588-598, 821-888),
//!   including the 3-significant-digit schemes 4 and 5 that no corpus file holds;
//! - `synthetic_hec_variants6.dss`: quality on 100/105/110/115, IR-CENTURY, IR-DECADE and
//!   far-date irregular data, an irregular pattern (type 111) and pseudo-regular paths.
//!
//! Reference values: `synthetic_hec6_reference.tsv`, recorded from Monolith 3.3.27 on
//! disposable copies: `RAW` lines are the REAL*4 bits heclib's
//! `zrrtsx` returns after `DUREAL`, `SERIES` lines what `HecTimeSeries.read` returns
//! (-901/-902 reported as -FLT_MAX). All names and values are synthetic.

use dss_core::dss6::{DatasetOptions, Dss6File, TsError, TsLayout, TsReadOptions};
use std::path::PathBuf;

const COMPRESS: &str = "synthetic_hec_compress6.dss";
const VARIANTS: &str = "synthetic_hec_variants6.dss";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

fn reference() -> Vec<Vec<String>> {
    std::fs::read_to_string(fixture("synthetic_hec6_reference.tsv"))
        .expect("reference missing")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split('\t').map(str::to_owned).collect())
        .collect()
}

fn hex_list(s: &str) -> Vec<u64> {
    if s.is_empty() || s == "-" {
        return Vec::new();
    }
    s.split(',')
        .map(|h| u64::from_str_radix(h, 16).unwrap())
        .collect()
}

/// "ddMMMyyyy HHmm" to seconds since 1899-12-31 00:00 (2400 allowed).
fn hec_seconds(s: &str) -> i64 {
    const M: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let day: i64 = s[0..2].parse().unwrap();
    let mon = M
        .iter()
        .position(|m| s[2..5].eq_ignore_ascii_case(m))
        .unwrap() as i64
        + 1;
    let year: i64 = s[5..9].parse().unwrap();
    let hh: i64 = s[10..12].parse().unwrap();
    let mm: i64 = s[12..14].parse().unwrap();
    days_from_civil(year, mon, day) * 86400 + hh * 3600 + mm * 60
}

/// Days since 1899-12-31 (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 693900
}

#[test]
fn calendar_helper() {
    assert_eq!(days_from_civil(1899, 12, 31), 0);
    assert_eq!(days_from_civil(1900, 1, 1), 1);
    assert_eq!(hec_seconds("02JAN1900 2400"), 3 * 86400);
}

/// RAW: every regular REAL*4 record decodes to the bits heclib returns, compression
/// schemes 0-5 included; the layout reports the stored scheme.
#[test]
fn records_equal_heclib_bits() {
    let mut schemes = [0usize; 6];
    for r in reference().iter().filter(|r| r[0] == "RAW") {
        let f = Dss6File::open(fixture(&r[1])).unwrap();
        let rec = f.read_ts_record(r[2].as_bytes()).unwrap();
        let scheme: i32 = r[3].parse().unwrap();
        let want_layout = if scheme == 0 {
            TsLayout::Regular
        } else {
            TsLayout::Compressed(scheme)
        };
        assert_eq!(rec.layout, want_layout, "{}", r[2]);
        let bits: Vec<u64> = match &rec.values {
            dss_core::TsValues::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
            other => panic!("{}: not REAL*4: {other:?}", r[2]),
        };
        assert_eq!(bits, hex_list(&r[5]), "{}", r[2]);
        schemes[scheme as usize] += 1;
    }
    // Schemes 0 (quality and the variants), 1, 2, 3, 4 (5 records), 5 (2 records).
    assert_eq!(schemes, [4, 1, 1, 1, 5, 2]);
}

/// Schemes 4/5 decode by default since P33; `allow_sig_digits = false` refuses them.
#[test]
fn sig_digits_on_by_default_and_refusable() {
    assert!(TsReadOptions::default().allow_sig_digits);
    let f = Dss6File::open(fixture(COMPRESS)).unwrap();
    let off = TsReadOptions {
        allow_sig_digits: false,
        ..TsReadOptions::default()
    };
    for (p, s) in [
        ("/P33/SIG4 SPAN/FLOW/01JAN2000/1DAY/S4/", 4),
        ("/P33/SIG5 RUNS/PRECIP-INC/01JAN2000/1HOUR/S5/", 5),
    ] {
        match f.read_ts_record_with(p.as_bytes(), &off) {
            Err(TsError::UnsupportedCompression { scheme }) => assert_eq!(scheme, s),
            other => panic!("{p}: {other:?}"),
        }
        assert!(f.read_ts_record(p.as_bytes()).is_ok());
    }
}

/// HEC writes the sig-digit S-section size byte from a stale argument (0 here, and 2 for
/// records written after the delta-high record): both decode.
#[test]
fn sig_digit_size_byte_is_not_checked() {
    let f = Dss6File::open(fixture(COMPRESS)).unwrap();
    for p in [
        "/P33/SIG4 SPAN/FLOW/01JAN2000/1DAY/S4/",
        "/P33/SIG4 NAN/FLOW/01JAN2000/1DAY/S4N/",
    ] {
        let rec = f.read_ts_record(p.as_bytes()).unwrap();
        assert_eq!(rec.layout, TsLayout::Compressed(4), "{p}");
    }
    // R2SD3 turns a NaN into the positive-underflow code 1002 (1.0e-37 on read), and
    // -FLT_MAX into the negative-overflow code 1005 (-1.0e37): HEC changes these values.
    let nan = f
        .read_ts_record(b"/P33/SIG4 NAN/FLOW/01JAN2000/1DAY/S4N/")
        .unwrap();
    assert_eq!(nan.values.to_f64()[100], f64::from(1.0e-37f32));
    let span = f
        .read_ts_record(b"/P33/SIG4 SPAN/FLOW/01JAN2000/1DAY/S4/")
        .unwrap();
    assert_eq!(span.values.to_f64()[42], f64::from(-1.0e37f32));
}

fn read_series<S: dss_core::dss6::ReadAt>(
    f: &Dss6File<S>,
    mode: &str,
    path: &str,
    a: &str,
    b: &str,
) -> dss_core::TimeSeries {
    match mode {
        "REC" => f.read_ts_record(path.as_bytes()).unwrap().to_timeseries(),
        "ALL" => {
            f.read_ts_dataset(path.as_bytes(), &DatasetOptions::whole())
                .unwrap()
                .series
        }
        _ => {
            let opts = DatasetOptions {
                trim_missing: true,
                ..DatasetOptions::window(hec_seconds(a), hec_seconds(b))
            };
            f.read_ts_dataset(path.as_bytes(), &opts).unwrap().series
        }
    }
}

/// SERIES and SUMMARY: record, dataset and window reads equal Monolith's times, values
/// (HEC missing markers compared as Monolith's -FLT_MAX), quality and counts.
#[test]
fn series_equal_monolith() {
    let undef = f64::from(-f32::MAX);
    let mut n = [0usize; 2];
    for r in reference()
        .iter()
        .filter(|r| r[0] == "SERIES" || r[0] == "SUMMARY")
    {
        let bytes = std::fs::read(fixture(&r[1])).unwrap();
        let f = Dss6File::from_bytes(&bytes).unwrap();
        let ts = read_series(&f, &r[2], &r[3], &r[4], &r[5]);
        let what = format!("{} {} {}", r[2], r[3], r[4]);
        let count: usize = r[6].parse().unwrap();
        assert_eq!(ts.len(), count, "{what}");
        let mins: Vec<i64> = ts.times.iter().map(|t| t.div_euclid(60)).collect();
        assert!(ts.times.iter().all(|t| t % 60 == 0), "{what}");
        let vals: Vec<u64> = ts
            .values
            .to_f64()
            .iter()
            .map(|&v| {
                if v == -901.0 || v == -902.0 {
                    undef.to_bits()
                } else {
                    v.to_bits()
                }
            })
            .collect();
        if r[0] == "SERIES" {
            let want: Vec<i64> = if count == 0 {
                Vec::new()
            } else {
                r[7].split(',').map(|x| x.parse().unwrap()).collect()
            };
            assert_eq!(mins, want, "{what}");
            assert_eq!(vals, hex_list(&r[8]), "{what}");
            let q = ts.quality.clone().filter(|q| q.iter().any(|&x| x != 0));
            let want_q: Option<Vec<i32>> = (r[9] != "-")
                .then(|| r[9].split(',').map(|x| x.parse().unwrap()).collect())
                .filter(|q: &Vec<i32>| q.iter().any(|&x| x != 0));
            assert_eq!(q, want_q, "{what}");
            n[0] += 1;
        } else {
            assert_eq!(mins[0], r[7].parse::<i64>().unwrap(), "{what}");
            assert_eq!(mins[count - 1], r[8].parse::<i64>().unwrap(), "{what}");
            n[1] += 1;
        }
    }
    assert_eq!(n, [47, 33]);
}

/// Type 111 (TS- D part): `zsitsi6.f` lines 213-266 store (time, value) pairs and
/// `zritsi6.f` lines 154-184 read them back as pairs. Monolith 3.3.27 and HEC-DSSVue's
/// Monolith read the record through `Public/ztsRetrievePattern.c`, whose lines 280 and
/// 326-336 use the DSS7 layout (all times, then all values) and return 8 values unlike those written;
/// the engine follows the Fortran reader and the values written.
#[test]
fn irregular_pattern_reads_written_pairs() {
    let f = Dss6File::open(fixture(VARIANTS)).unwrap();
    let rec = f
        .read_ts_record(b"/P33/IPAT/FLOW/TS-PATTERN/IR-YEAR/PAT/")
        .unwrap();
    assert_eq!(rec.record_type, 111);
    assert_eq!(rec.layout, TsLayout::IrregularPattern);
    assert_eq!(
        rec.values.to_f64(),
        [0.1f32, 0.4, 0.9, 0.3].map(f64::from).to_vec()
    );
    // Stored minutes 525600 + the written offsets 0 min, 600 min, 40 days and 200 days.
    assert_eq!(
        rec.times,
        vec![525_600 * 60, 526_200 * 60, 583_200 * 60, 813_600 * 60]
    );
}

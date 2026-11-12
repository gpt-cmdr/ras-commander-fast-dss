//! P71c: SNODAS times and data types by default from the checked NOHRSC header.
//!
//! The table is the 16 headers of the public NSIDC tars `SNODAS_20100215.tar` and
//! `SNODAS_20200101.tar`, with their `Start` and `Stop` fields as published. Each header is rebuilt from the
//! committed fixture header with its `Start` and `Stop` fields replaced. With
//! `P71_SNODAS_MEMBERS` set to the folder holding the unpacked members (`SNODAS_<date>_unzip/*.txt`),
//! `table_is_the_real_headers` also reads the real headers and checks the table against them.

use dss_vortex::import::{open_source, OpenOptions};
use dss_vortex::source::snodas::{self, Grid, ProductKind, SnodasFile, SnodasOptions, SnodasTimes};
use dss_vortex::{DataType, FrameTime, VortexError};
use std::io::Read;
use std::path::{Path, PathBuf};

const STEM: &str = "us_ssmv11039lL00T0024TTNATS2020010105DP000";

/// Member stem, header start, header stop (UTC, `YYYY-MM-DDTHH`).
const HEADERS: [(&str, &str, &str); 16] = [
    (
        "us_ssmv01025SlL00T0024TTNATS2010021505DP001",
        "2010-02-14T06",
        "2010-02-15T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2010021505DP001",
        "2010-02-14T06",
        "2010-02-15T06",
    ),
    (
        "us_ssmv11034tS__T0001TTNATS2010021505HP001",
        "2010-02-15T06",
        "2010-02-15T06",
    ),
    (
        "us_ssmv11036tS__T0001TTNATS2010021505HP001",
        "2010-02-15T06",
        "2010-02-15T06",
    ),
    (
        "us_ssmv11038wS__A0024TTNATS2010021505DP001",
        "2010-02-14T07",
        "2010-02-15T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2010021505DP000",
        "2010-02-14T06",
        "2010-02-15T05",
    ),
    (
        "us_ssmv11044bS__T0001TTNATS2010021505HP000",
        "2010-02-15T05",
        "2010-02-15T05",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2010021505DP000",
        "2010-02-14T06",
        "2010-02-15T05",
    ),
    (
        "us_ssmv01025SlL00T0024TTNATS2020010105DP001",
        "2019-12-31T06",
        "2020-01-01T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2020010105DP001",
        "2019-12-31T06",
        "2020-01-01T06",
    ),
    (
        "us_ssmv11034tS__T0001TTNATS2020010105HP001",
        "2020-01-01T06",
        "2020-01-01T06",
    ),
    (
        "us_ssmv11036tS__T0001TTNATS2020010105HP001",
        "2020-01-01T06",
        "2020-01-01T06",
    ),
    (
        "us_ssmv11038wS__A0024TTNATS2020010105DP001",
        "2019-12-31T07",
        "2020-01-01T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2020010105DP000",
        "2019-12-31T06",
        "2020-01-01T06",
    ),
    (
        "us_ssmv11044bS__T0024TTNATS2020010105DP000",
        "2019-12-31T06",
        "2020-01-01T06",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2020010105DP000",
        "2019-12-31T06",
        "2020-01-01T06",
    ),
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/p71")
}

fn fixture_header() -> String {
    let gz = std::fs::read(fixtures().join(format!("{STEM}.txt.gz"))).unwrap();
    let mut text = String::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// `YYYY-MM-DDTHH` to its fields.
fn fields(t: &str) -> [i64; 4] {
    let n = |a: usize, b: usize| t[a..b].parse::<i64>().unwrap();
    [n(0, 4), n(5, 7), n(8, 10), n(11, 13)]
}

/// (day since 1899-12-31, second), as the reader keeps a time.
fn stamp(t: &str) -> (i64, i64) {
    let [y, m, d, h] = fields(t);
    // days_from_civil (H. Hinnant), 1970-01-01 is day 25,568.
    let y2 = y - i64::from(m <= 2);
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468 + 25_568, h * 3600)
}

fn with_times(header: &str, start: &str, stop: &str) -> String {
    let [sy, sm, sd, sh] = fields(start);
    let [ey, em, ed, eh] = fields(stop);
    let value = |key: &str| -> Option<i64> {
        Some(match key {
            "Start year" => sy,
            "Start month" => sm,
            "Start day" => sd,
            "Start hour" => sh,
            "Stop year" => ey,
            "Stop month" => em,
            "Stop day" => ed,
            "Stop hour" => eh,
            _ => return None,
        })
    };
    header
        .lines()
        .map(|l| match l.split_once(':') {
            Some((k, _)) => match value(k.trim()) {
                Some(v) => format!("{}: {v}", k.trim()),
                None => l.to_string(),
            },
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn grid(stem: &str, start: &str, stop: &str) -> Grid {
    let dat_name = format!("{stem}.dat");
    let header = snodas::parse_header(with_times(&fixture_header(), start, stop).as_bytes())
        .expect("header");
    Grid {
        product: snodas::product(&dat_name).unwrap(),
        dat_name,
        header: Some(header),
    }
}

fn options(times: SnodasTimes) -> SnodasOptions {
    SnodasOptions {
        times,
        ..SnodasOptions::default()
    }
}

fn refusal<T: std::fmt::Debug>(r: Result<T, VortexError>) -> String {
    match r {
        Err(e @ VortexError::Refused { .. }) => e.to_string(),
        other => panic!("not a refusal: {other:?}"),
    }
}

#[test]
fn default_is_the_checked_header() {
    assert_eq!(SnodasOptions::default().times, SnodasTimes::Header);
    let (mut totals, mut snapshots, mut ambiguous, mut historical) = (0, 0, 0, 0);
    for (stem, start, stop) in HEADERS {
        let g = grid(stem, start, stop);
        let code = &stem[8..12];
        let date = &stem[stem.len() - 15..stem.len() - 7];
        let six = stamp(&format!(
            "{}-{}-{}T06",
            &date[..4],
            &date[4..6],
            &date[6..8]
        ));
        match g.product.kind {
            // The 2010 sublimation headers stop at 05:00: refused until qualified.
            ProductKind::DailyTotal if stamp(stop) != six => {
                historical += 1;
                assert!(matches!(code, "1039" | "1050"), "{stem}");
                assert_eq!(stamp(stop), (six.0, six.1 - 3600), "{stem}");
                let error = refusal(g.time_and_type(SnodasOptions::default()));
                for words in [
                    "historical",
                    "snodas_times=\"header\"",
                    "snodas_times=\"vortex\"",
                    "\"header_as_stated\"",
                ] {
                    assert!(error.contains(words), "{stem}: {error}");
                }
                assert!(g.time_and_type(options(SnodasTimes::Vortex)).is_ok());
                assert!(g
                    .time_and_type(options(SnodasTimes::HeaderAsStated))
                    .is_ok());
            }
            ProductKind::DailyTotal => {
                totals += 1;
                assert!(matches!(code, "1025" | "1039" | "1044" | "1050"), "{stem}");
                let (time, data_type) = g.time_and_type(SnodasOptions::default()).unwrap();
                // The 24 hours ending 06:00 UTC of the file's date, PER-CUM.
                assert_eq!(
                    time,
                    FrameTime::Period {
                        start: (six.0 - 1, six.1),
                        end: six
                    },
                    "{stem}"
                );
                assert_eq!(data_type, DataType::PerCum, "{stem}");
                // That is the header's own period.
                assert_eq!(
                    (stamp(start), stamp(stop)),
                    ((six.0 - 1, six.1), six),
                    "{stem}"
                );
            }
            ProductKind::Snapshot => {
                snapshots += 1;
                assert!(matches!(code, "1034" | "1036"), "{stem}");
                let (time, data_type) = g.time_and_type(SnodasOptions::default()).unwrap();
                assert_eq!(time, FrameTime::Instant(six), "{stem}");
                assert_eq!(time, FrameTime::Instant(stamp(start)), "{stem}");
                assert_eq!(data_type, DataType::InstVal, "{stem}");
            }
            ProductKind::Ambiguous => {
                ambiguous += 1;
                let error = refusal(g.time_and_type(SnodasOptions::default()));
                for words in [
                    "snodas_times=\"header\"",
                    "snodas_times=\"vortex\"",
                    "\"header_as_stated\"",
                ] {
                    assert!(error.contains(words), "{stem}: {error}");
                }
                // Either named interpretation reads it.
                assert!(g.time_and_type(options(SnodasTimes::Vortex)).is_ok());
                assert!(g
                    .time_and_type(options(SnodasTimes::HeaderAsStated))
                    .is_ok());
            }
        }
    }
    // Seven totals; four snapshots; 1038 twice and the 2010 melt (T0001) ambiguous by code; the
    // two 2010 sublimation totals refused by their 05:00 stop.
    assert_eq!((totals, snapshots, ambiguous, historical), (7, 4, 3, 2));
}

/// The stem with one timing field replaced. From the end: the date (8), `TS` (2), the step (1)
/// and the offset (4); `code` is whichever of `T0024`, `T0001`, `A0024` the stem holds.
fn altered(stem: &str, field: &str, value: &str) -> String {
    let n = stem.len();
    let (from, to) = match field {
        "date" => (n - 15, n - 7),
        "hour" => (n - 7, n - 5),
        "step" => (n - 5, n - 4),
        "offset" => (n - 4, n),
        _ => {
            let at = ["T0024", "T0001", "A0024"]
                .iter()
                .find_map(|c| stem.find(c))
                .unwrap();
            (at, at + 5)
        }
    };
    format!("{}{value}{}", &stem[..from], &stem[to..])
}

/// Each timing field of a qualified stem altered in turn: the date a day later, the hour 06, 17
/// and malformed, the step and the offset swapped, and the time code swapped.
fn alterations(stem: &str) -> Vec<(&'static str, String)> {
    let n = stem.len();
    let date = &stem[n - 15..n - 7];
    let next = format!(
        "{}{:02}",
        &date[..6],
        date[6..8].parse::<u32>().unwrap() + 1
    );
    let step = if &stem[n - 5..n - 4] == "D" { "H" } else { "D" };
    let offset = if &stem[n - 4..] == "P000" {
        "P001"
    } else {
        "P000"
    };
    let code = if stem.contains("T0024") {
        "T0001"
    } else {
        "T0024"
    };
    [
        ("date", next),
        ("hour", "06".to_string()),
        ("hour", "17".to_string()),
        ("hour", "5x".to_string()),
        ("step", step.to_string()),
        ("offset", offset.to_string()),
        ("code", code.to_string()),
    ]
    .into_iter()
    .map(|(f, v)| (f, altered(stem, f, &v)))
    .collect()
}

/// One real header: its own stem is read by the default; each altered stem is refused by the
/// default, naming both options, and still read by `"vortex"`, which uses the date only.
fn check_fields(stem: &str, header: &snodas::Header) {
    let make = |s: &str| Grid {
        product: snodas::product(&format!("{s}.dat")).unwrap(),
        dat_name: format!("{s}.dat"),
        header: Some(header.clone()),
    };
    assert!(
        make(stem).time_and_type(SnodasOptions::default()).is_ok(),
        "{stem}"
    );
    for (field, s) in alterations(stem) {
        let g = make(&s);
        let error = refusal(g.time_and_type(SnodasOptions::default()));
        for words in [
            "snodas_times=\"header\"",
            "snodas_times=\"vortex\"",
            "\"header_as_stated\"",
        ] {
            assert!(error.contains(words), "{field} {s}: {error}");
        }
        assert!(
            g.time_and_type(options(SnodasTimes::Vortex)).is_ok(),
            "{field} {s}"
        );
    }
}

/// The rows of the table the default reads: not ambiguous by code, no historical 05:00 stop.
fn qualified() -> Vec<(&'static str, &'static str, &'static str)> {
    HEADERS
        .into_iter()
        .filter(|(stem, _, stop)| {
            snodas::product(&format!("{stem}.dat")).unwrap().kind != ProductKind::Ambiguous
                && !stop.ends_with("T05")
        })
        .collect()
}

#[test]
fn every_filename_timing_field_is_checked() {
    let rows = qualified();
    assert_eq!(rows.len(), 11);
    for (stem, start, stop) in rows {
        check_fields(stem, grid(stem, start, stop).header.as_ref().unwrap());
    }
    // The 2020 sublimation member renamed to hour 17, its header unchanged.
    let g = grid(
        &altered(STEM, "hour", "17"),
        "2019-12-31T06",
        "2020-01-01T06",
    );
    let error = refusal(g.time_and_type(SnodasOptions::default()));
    assert!(error.contains("hour (TS) is 17"), "{error}");
}

#[test]
fn a_05_stop_is_the_historical_refusal_for_sublimation_and_a_disagreement_otherwise() {
    // The 2020 sublimation header with its stop moved to 05:00.
    let error = refusal(
        grid(STEM, "2019-12-31T06", "2020-01-01T05").time_and_type(SnodasOptions::default()),
    );
    assert!(error.contains("historical"), "{error}");
    // Other daily totals with a 05:00 stop: a plain disagreement, never accepted.
    for stem in [
        "us_ssmv01025SlL00T0024TTNATS2020010105DP001",
        "us_ssmv01025SlL00T0024TTNATS2010021505DP001",
        "us_ssmv11044bS__T0024TTNATS2020010105DP000",
    ] {
        let date = &stem[stem.len() - 15..stem.len() - 7];
        let (start, stop) = if date == "20100215" {
            ("2010-02-14T06", "2010-02-15T05")
        } else {
            ("2019-12-31T06", "2020-01-01T05")
        };
        let error = refusal(grid(stem, start, stop).time_and_type(SnodasOptions::default()));
        assert!(
            !error.contains("historical") && error.contains("header states"),
            "{stem}: {error}"
        );
    }
}

/// `check_fields` on the real header files, when they are available (`P71_SNODAS_MEMBERS`).
#[test]
fn every_filename_timing_field_is_checked_on_the_real_headers() {
    let Ok(root) = std::env::var("P71_SNODAS_MEMBERS") else {
        eprintln!("P71_SNODAS_MEMBERS not set; checked on headers rebuilt from the table only");
        return;
    };
    let rows = qualified();
    assert_eq!(rows.len(), 11);
    for (stem, _, _) in rows {
        let date = &stem[stem.len() - 15..stem.len() - 7];
        let path = Path::new(&root)
            .join(format!("SNODAS_{date}_unzip"))
            .join(format!("{stem}.txt"));
        let header = snodas::parse_header(&std::fs::read(&path).unwrap()).unwrap();
        check_fields(stem, &header);
    }
}

#[test]
fn ambiguous_products_are_1038_and_melt_t0001() {
    for (stem, _, _) in HEADERS {
        let p = snodas::product(&format!("{stem}.dat")).unwrap();
        let expect = &stem[8..12] == "1038" || (&stem[8..12] == "1044" && stem.contains("T0001"));
        assert_eq!(p.kind == ProductKind::Ambiguous, expect, "{stem}");
    }
}

#[test]
fn vortex_option_is_unchanged() {
    for (stem, start, stop) in HEADERS {
        let g = grid(stem, start, stop);
        let (time, data_type) = g.time_and_type(options(SnodasTimes::Vortex)).unwrap();
        assert_eq!(time, g.product.vortex_time, "{stem}");
        // VortexGrid.inferDataType: a period of a variable it calls precipitation accumulates,
        // and none of the SNODAS names is one ("Liquid Precipitation" is not), so every period
        // averages; an instant is instantaneous.
        let want = match time {
            FrameTime::Period { .. } => DataType::PerAver,
            _ => DataType::InstVal,
        };
        assert_eq!(data_type, want, "{stem}");
        // And it is one day after the header for the totals.
        if let FrameTime::Period { start: s, .. } = time {
            assert_eq!(s, g.product.day, "{stem}");
        }
    }
}

#[test]
fn header_as_stated_is_the_header() {
    for (stem, start, stop) in HEADERS {
        let g = grid(stem, start, stop);
        let (time, _) = g
            .time_and_type(options(SnodasTimes::HeaderAsStated))
            .unwrap();
        let want = if start == stop {
            FrameTime::Instant(stamp(start))
        } else {
            FrameTime::Period {
                start: stamp(start),
                end: stamp(stop),
            }
        };
        assert_eq!(time, want, "{stem}");
    }
}

#[test]
fn a_header_that_disagrees_is_refused() {
    for (stem, start, stop, bad_start, bad_stop) in [
        // A total stated a day late, as Vortex stamps it.
        (
            "us_ssmv11039lL00T0024TTNATS2020010105DP000",
            "2019-12-31T06",
            "2020-01-01T06",
            "2020-01-01T06",
            "2020-01-02T06",
        ),
        // A total of 23 hours.
        (
            "us_ssmv01025SlL00T0024TTNATS2020010105DP001",
            "2019-12-31T06",
            "2020-01-01T06",
            "2019-12-31T07",
            "2020-01-01T06",
        ),
        // SWE at 05:00.
        (
            "us_ssmv11034tS__T0001TTNATS2020010105HP001",
            "2020-01-01T06",
            "2020-01-01T06",
            "2020-01-01T05",
            "2020-01-01T05",
        ),
        // SWE over a period.
        (
            "us_ssmv11036tS__T0001TTNATS2020010105HP001",
            "2020-01-01T06",
            "2020-01-01T06",
            "2019-12-31T06",
            "2020-01-01T06",
        ),
    ] {
        assert!(grid(stem, start, stop)
            .time_and_type(SnodasOptions::default())
            .is_ok());
        let error =
            refusal(grid(stem, bad_start, bad_stop).time_and_type(SnodasOptions::default()));
        assert!(
            error.contains("disagree") || error.contains("where its file name"),
            "{error}"
        );
        assert!(error.contains("header_as_stated"), "{error}");
    }
}

/// A tar of the fixture member under two names: the 2020 blowing-snow sublimation, and the same
/// bytes named as the 2020 snowpack temperature with that product's header times.
#[test]
fn a_tar_with_an_ambiguous_product_opens_and_refuses_only_that_product() {
    let dat = std::fs::read(fixtures().join(format!("{STEM}.dat.gz"))).unwrap();
    let header = std::fs::read(fixtures().join(format!("{STEM}.txt.gz"))).unwrap();
    let temp = "us_ssmv11038wS__A0024TTNATS2020010105DP001";
    let temp_header = with_times(&fixture_header(), "2019-12-31T07", "2020-01-01T06");
    let mut tar = Vec::new();
    for (name, bytes) in [
        (format!("{STEM}.txt.gz"), header.clone()),
        (format!("{STEM}.dat.gz"), dat.clone()),
        (format!("{temp}.txt"), temp_header.into_bytes()),
        (format!("{temp}.dat.gz"), dat.clone()),
    ] {
        tar.extend(tar_header(&name, bytes.len()));
        tar.extend(&bytes);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.extend(vec![0u8; 1024]);
    let file = SnodasFile::from_tar(
        "SNODAS_20200101.tar",
        tar.clone(),
        SnodasOptions::default(),
        1 << 32,
    )
    .unwrap();
    let sublimation = "Sublimation of Blowing Snow";
    let temperature = "Snow Pack Average Temperature";
    assert_eq!(file.variables(), vec![sublimation, temperature]);
    let frame = file.frame(sublimation).unwrap();
    assert_eq!(frame.parts().data_type, DataType::PerCum);
    let error = refusal(file.frame(temperature));
    assert!(error.contains("1038"), "{error}");
    // Through the importer: the same.
    let dir = std::env::temp_dir().join(format!("p71c_tar_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SNODAS_20200101.tar");
    std::fs::write(&path, &tar).unwrap();
    let source = open_source(&path, &OpenOptions::default()).unwrap();
    assert!(source.step_meta(sublimation, 0).is_ok());
    assert!(source.step_meta(temperature, 0).is_err());
    let vortex = OpenOptions {
        snodas: options(SnodasTimes::Vortex),
        ..OpenOptions::default()
    };
    let source = open_source(&path, &vortex).unwrap();
    let meta = source.step_meta(temperature, 0).unwrap();
    assert_eq!(meta.source_type, DataType::InstVal);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn table_is_the_real_headers() {
    let Ok(root) = std::env::var("P71_SNODAS_MEMBERS") else {
        eprintln!("P71_SNODAS_MEMBERS not set; the table is checked against the fixture only");
        return;
    };
    for (stem, start, stop) in HEADERS {
        let date = &stem[stem.len() - 15..stem.len() - 7];
        let path = Path::new(&root)
            .join(format!("SNODAS_{date}_unzip"))
            .join(format!("{stem}.txt"));
        let h = snodas::parse_header(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((h.start, h.stop), (stamp(start), stamp(stop)), "{stem}");
    }
}

fn tar_header(name: &str, size: usize) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..107].copy_from_slice(b"0000644");
    h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h
}

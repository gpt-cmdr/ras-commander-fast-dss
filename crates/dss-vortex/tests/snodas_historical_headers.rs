//! Maintainer decision 2026-11-14: the two historical NOHRSC SNODAS header
//! conventions are read, by default, as the header declares them.
//!
//! - Melt (`1044`) and sublimation (`1039`, `1050`) dated before 2019-02-05: 06:00 UTC of the
//!   previous day to a 05:00 UTC stop. From 2019-02-05: 06:00 to 06:00.
//! - Precipitation (`1025`) dated before 2008-10-23: 07:00 UTC of the previous day to 06:00 UTC.
//!   From 2008-10-23: 06:00 to 06:00.
//!
//! The fixtures are the real NOHRSC header members, byte for byte, of the public NSIDC G02158
//! masked daily tars of 2003-10-01 (the archive's first day), 2008-10-22 and 2008-10-23 (the
//! precipitation boundary), and 2019-02-04 and 2019-02-05 (the melt and sublimation boundary);
//! `fixtures/public/README.md` gives each one's tar URL and SHA-256.

use dss_vortex::import::{open_source, OpenOptions};
use dss_vortex::source::snodas::{self, Grid, SnodasFile, SnodasOptions, SnodasTimes};
use dss_vortex::{DataType, FrameTime, VortexError};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Member stem; header start and stop as the default reads them (UTC, `YYYY-MM-DDTHH`). The
/// members' SHA-256 are checked against the fixtures README by `tests/python/test_snodas_historical.py`.
const CASES: [(&str, &str, &str); 25] = [
    // 2003-10-01: both conventions.
    (
        "us_ssmv01025SlL00T0024TTNATS2003100105DP001",
        "2003-09-30T07",
        "2003-10-01T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2003100105DP001",
        "2003-09-30T07",
        "2003-10-01T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2003100105DP000",
        "2003-09-30T06",
        "2003-10-01T05",
    ),
    (
        "us_ssmv11044bS__T0024TTNATS2003100105DP000",
        "2003-09-30T06",
        "2003-10-01T05",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2003100105DP000",
        "2003-09-30T06",
        "2003-10-01T05",
    ),
    // 2008-10-22: the last day of the precipitation convention.
    (
        "us_ssmv01025SlL00T0024TTNATS2008102205DP001",
        "2008-10-21T07",
        "2008-10-22T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2008102205DP001",
        "2008-10-21T07",
        "2008-10-22T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2008102205DP000",
        "2008-10-21T06",
        "2008-10-22T05",
    ),
    (
        "us_ssmv11044bS__T0024TTNATS2008102205DP000",
        "2008-10-21T06",
        "2008-10-22T05",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2008102205DP000",
        "2008-10-21T06",
        "2008-10-22T05",
    ),
    // 2008-10-23: precipitation 06:00 to 06:00; melt and sublimation still 05:00.
    (
        "us_ssmv01025SlL00T0024TTNATS2008102305DP001",
        "2008-10-22T06",
        "2008-10-23T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2008102305DP001",
        "2008-10-22T06",
        "2008-10-23T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2008102305DP000",
        "2008-10-22T06",
        "2008-10-23T05",
    ),
    (
        "us_ssmv11044bS__T0024TTNATS2008102305DP000",
        "2008-10-22T06",
        "2008-10-23T05",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2008102305DP000",
        "2008-10-22T06",
        "2008-10-23T05",
    ),
    // 2019-02-04: the last day of the melt and sublimation convention.
    (
        "us_ssmv01025SlL00T0024TTNATS2019020405DP001",
        "2019-02-03T06",
        "2019-02-04T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2019020405DP001",
        "2019-02-03T06",
        "2019-02-04T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2019020405DP000",
        "2019-02-03T06",
        "2019-02-04T05",
    ),
    (
        "us_ssmv11044bS__T0024TTNATS2019020405DP000",
        "2019-02-03T06",
        "2019-02-04T05",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2019020405DP000",
        "2019-02-03T06",
        "2019-02-04T05",
    ),
    // 2019-02-05: every daily total 06:00 to 06:00.
    (
        "us_ssmv01025SlL00T0024TTNATS2019020505DP001",
        "2019-02-04T06",
        "2019-02-05T06",
    ),
    (
        "us_ssmv01025SlL01T0024TTNATS2019020505DP001",
        "2019-02-04T06",
        "2019-02-05T06",
    ),
    (
        "us_ssmv11039lL00T0024TTNATS2019020505DP000",
        "2019-02-04T06",
        "2019-02-05T06",
    ),
    (
        "us_ssmv11044bS__T0024TTNATS2019020505DP000",
        "2019-02-04T06",
        "2019-02-05T06",
    ),
    (
        "us_ssmv11050lL00T0024TTNATS2019020505DP000",
        "2019-02-04T06",
        "2019-02-05T06",
    ),
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public")
}

fn stored(stem: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(format!("snodas_headers/{stem}.txt.gz"))).unwrap()
}

fn text(stem: &str) -> String {
    let mut out = String::new();
    flate2::read::GzDecoder::new(&stored(stem)[..])
        .read_to_string(&mut out)
        .unwrap();
    out
}

/// (day since 1899-12-31, second) of `YYYY-MM-DDTHH`.
fn stamp(t: &str) -> (i64, i64) {
    let n = |a: usize, b: usize| t[a..b].parse::<i64>().unwrap();
    let (y, m, d, h) = (n(0, 4), n(5, 7), n(8, 10), n(11, 13));
    // days_from_civil (H. Hinnant), 1970-01-01 is day 25,568.
    let y2 = y - i64::from(m <= 2);
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468 + 25_568, h * 3600)
}

fn grid_with(stem: &str, header: &str) -> Grid {
    let dat_name = format!("{stem}.dat");
    Grid {
        product: snodas::product(&dat_name).unwrap(),
        dat_name,
        header: Some(snodas::parse_header(header.as_bytes()).unwrap()),
    }
}

fn grid(stem: &str) -> Grid {
    grid_with(stem, &text(stem))
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

/// The header with one field, such as `Stop hour`, replaced.
fn with_field(header: &str, key: &str, value: i64) -> String {
    header
        .lines()
        .map(|l| match l.split_once(':') {
            Some((k, _)) if k.trim() == key => format!("{key}: {value}"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_historical(stem: &str) -> bool {
    let date = &stem[stem.len() - 15..stem.len() - 7];
    match &stem[8..12] {
        "1025" => date < "20081023",
        _ => date < "20190205",
    }
}

#[test]
fn each_header_states_the_table() {
    for (stem, start, stop) in CASES {
        let g = grid(stem);
        let h = g.header.as_ref().unwrap();
        assert_eq!((h.start, h.stop), (stamp(start), stamp(stop)), "{stem}");
        // The table's eras: the historical rows differ from 06:00 to 06:00 as documented.
        let six = g.product.day;
        let modern = ((six.0 - 1, six.1), six);
        if !is_historical(stem) {
            assert_eq!((h.start, h.stop), modern, "{stem}");
        } else if &stem[8..12] == "1025" {
            assert_eq!(
                (h.start, h.stop),
                ((six.0 - 1, six.1 + 3600), six),
                "{stem}"
            );
        } else {
            assert_eq!(
                (h.start, h.stop),
                ((six.0 - 1, six.1), (six.0, six.1 - 3600)),
                "{stem}"
            );
        }
    }
}

#[test]
fn default_reads_every_era_as_its_header_declares() {
    let mut historical = 0;
    for (stem, start, stop) in CASES {
        let g = grid(stem);
        let (time, data_type) = g
            .time_and_type(SnodasOptions::default())
            .unwrap_or_else(|e| panic!("{stem}: {e}"));
        assert_eq!(
            time,
            FrameTime::Period {
                start: stamp(start),
                end: stamp(stop)
            },
            "{stem}"
        );
        assert_eq!(data_type, DataType::PerCum, "{stem}");
        historical += usize::from(is_historical(stem));
    }
    // 2003: five; 2008-10-22: five; 2008-10-23 and 2019-02-04: three each (melt, sublimation).
    assert_eq!(historical, 16);
}

#[test]
fn vortex_option_is_unchanged() {
    for (stem, _, _) in CASES {
        let g = grid(stem);
        let (time, data_type) = g.time_and_type(options(SnodasTimes::Vortex)).unwrap();
        // getTimeSpan: 06:00 of the file's date to 06:00 the next day, whatever the header says;
        // inferDataType: PER-AVER (none of the SNODAS names is precipitation to Vortex).
        let six = g.product.day;
        assert_eq!(
            time,
            FrameTime::Period {
                start: six,
                end: (six.0 + 1, six.1)
            },
            "{stem}"
        );
        assert_eq!(time, g.product.vortex_time, "{stem}");
        assert_eq!(data_type, DataType::PerAver, "{stem}");
        // No header needed.
        let bare = Grid {
            header: None,
            ..g.clone()
        };
        assert_eq!(
            bare.time_and_type(options(SnodasTimes::Vortex)).unwrap(),
            (time, data_type)
        );
    }
}

#[test]
fn header_as_stated_is_unchanged() {
    for (stem, start, stop) in CASES {
        let (time, data_type) = grid(stem)
            .time_and_type(options(SnodasTimes::HeaderAsStated))
            .unwrap();
        assert_eq!(
            time,
            FrameTime::Period {
                start: stamp(start),
                end: stamp(stop)
            },
            "{stem}"
        );
        assert_eq!(data_type, DataType::PerAver, "{stem}");
    }
}

/// A historical convention stated by a file dated on or after the day it ended is refused, naming
/// the convention, its end and both options. Built from the first modern day's real header with the
/// one field changed.
#[test]
fn a_historical_convention_after_its_end_is_refused() {
    for (stem, key, value, ended) in [
        (
            "us_ssmv11039lL00T0024TTNATS2019020505DP000",
            "Stop hour",
            5,
            "2019-02-05",
        ),
        (
            "us_ssmv11044bS__T0024TTNATS2019020505DP000",
            "Stop hour",
            5,
            "2019-02-05",
        ),
        (
            "us_ssmv11050lL00T0024TTNATS2019020505DP000",
            "Stop hour",
            5,
            "2019-02-05",
        ),
        (
            "us_ssmv01025SlL00T0024TTNATS2008102305DP001",
            "Start hour",
            7,
            "2008-10-23",
        ),
        (
            "us_ssmv01025SlL01T0024TTNATS2008102305DP001",
            "Start hour",
            7,
            "2008-10-23",
        ),
        // And later: 2019-02-05 precipitation with a 07:00 start.
        (
            "us_ssmv01025SlL00T0024TTNATS2019020505DP001",
            "Start hour",
            7,
            "2008-10-23",
        ),
    ] {
        let g = grid_with(stem, &with_field(&text(stem), key, value));
        let error = refusal(g.time_and_type(SnodasOptions::default()));
        for words in [
            "historical",
            ended,
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
}

/// Every other contradiction is refused as before, in the historical eras too: only the two
/// conventions, each for its own products, are accepted.
#[test]
fn other_contradictions_are_still_refused() {
    for (stem, key, value) in [
        // Melt and sublimation with precipitation's convention (07:00 start).
        (
            "us_ssmv11044bS__T0024TTNATS2008102205DP000",
            "Start hour",
            7,
        ),
        (
            "us_ssmv11039lL00T0024TTNATS2003100105DP000",
            "Start hour",
            7,
        ),
        // Precipitation with melt's convention (05:00 stop).
        (
            "us_ssmv01025SlL00T0024TTNATS2003100105DP001",
            "Stop hour",
            5,
        ),
        // Other stops and starts.
        ("us_ssmv11050lL00T0024TTNATS2019020405DP000", "Stop hour", 4),
        (
            "us_ssmv11044bS__T0024TTNATS2019020405DP000",
            "Start hour",
            5,
        ),
        (
            "us_ssmv01025SlL01T0024TTNATS2008102205DP001",
            "Start hour",
            8,
        ),
        (
            "us_ssmv01025SlL01T0024TTNATS2008102205DP001",
            "Stop hour",
            7,
        ),
        // A stop a day late.
        ("us_ssmv11039lL00T0024TTNATS2008102305DP000", "Stop day", 24),
    ] {
        let g = grid_with(stem, &with_field(&text(stem), key, value));
        let error = refusal(g.time_and_type(SnodasOptions::default()));
        assert!(
            error.contains("header states") && !error.contains("historical"),
            "{stem} {key} {value}: {error}"
        );
        assert!(error.contains("\"header_as_stated\""), "{error}");
    }
    // A historical header under a file name whose timing field conflicts: the name check comes
    // first, as before.
    let stem = "us_ssmv11044bS__T0024TTNATS2019020405DP000";
    let renamed = stem.replace("2019020405DP000", "2019020417DP000");
    let error = refusal(grid_with(&renamed, &text(stem)).time_and_type(SnodasOptions::default()));
    assert!(error.contains("hour (TS) is 17"), "{error}");
    // The historical header moved to the next day's name (2019-02-05, a day after its end).
    let next = stem.replace("20190204", "20190205");
    let shifted = with_field(&with_field(&text(stem), "Start day", 4), "Stop day", 5);
    let error = refusal(grid_with(&next, &shifted).time_and_type(SnodasOptions::default()));
    assert!(
        error.contains("historical") && error.contains("2019-02-05"),
        "{error}"
    );
}

/// Through a tar and the importer's source: the historical member opens and gives its header's
/// period. The grid bytes are the committed 2020 sublimation member's, named as the 2019-02-04
/// one; only the times are under test.
#[test]
fn historical_member_through_a_tar_and_the_importer() {
    let fixture = "us_ssmv11039lL00T0024TTNATS2020010105DP000";
    let dat = std::fs::read(fixtures().join(format!("p71/{fixture}.dat.gz"))).unwrap();
    let stem = "us_ssmv11039lL00T0024TTNATS2019020405DP000";
    let mut tar = Vec::new();
    for (name, bytes) in [
        (format!("{stem}.txt.gz"), stored(stem)),
        (format!("{stem}.dat.gz"), dat),
    ] {
        tar.extend(tar_header(&name, bytes.len()));
        tar.extend(&bytes);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.extend(vec![0u8; 1024]);
    let variable = "Sublimation of Blowing Snow";
    let want = FrameTime::Period {
        start: stamp("2019-02-03T06"),
        end: stamp("2019-02-04T05"),
    };
    let file = SnodasFile::from_tar(
        "SNODAS_20190204.tar",
        tar.clone(),
        SnodasOptions::default(),
        1 << 32,
    )
    .unwrap();
    let frame = file.frame(variable).unwrap();
    assert_eq!(frame.parts().time, want);
    assert_eq!(frame.parts().data_type, DataType::PerCum);
    // Missing cells stay -9999, the marker, as before.
    assert_eq!(frame.parts().nodata, Some(snodas::NODATA));
    let dir = std::env::temp_dir().join(format!("snodas_hist_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SNODAS_20190204.tar");
    std::fs::write(&path, &tar).unwrap();
    let source = open_source(&path, &OpenOptions::default()).unwrap();
    let meta = source.step_meta(variable, 0).unwrap();
    assert_eq!((meta.time, meta.source_type), (want, DataType::PerCum));
    let vortex = OpenOptions {
        snodas: options(SnodasTimes::Vortex),
        ..OpenOptions::default()
    };
    let meta = open_source(&path, &vortex)
        .unwrap()
        .step_meta(variable, 0)
        .unwrap();
    assert_eq!(
        (meta.time, meta.source_type),
        (
            FrameTime::Period {
                start: stamp("2019-02-04T06"),
                end: stamp("2019-02-05T06")
            },
            DataType::PerAver
        )
    );
    let _ = std::fs::remove_dir_all(dir);
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

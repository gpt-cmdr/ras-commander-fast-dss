//! NetCDF fixtures against Vortex 1.1.0's own decoded output, recorded in
//! `fixtures/public/netcdf/expected.json`. Bits are compared
//! by hash, so this is also the cross-platform determinism check for this source.

use dss_vortex::source::netcdf::{parse_time_cell_method, NetcdfFile};
use dss_vortex::{Crs, DataType, FrameTime, GeoTransform, GridFrame, VortexError};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/netcdf")
}

fn expected() -> Value {
    serde_json::from_str(&fs::read_to_string(dir().join("expected.json")).unwrap()).unwrap()
}

fn fnv(cells: &[f32]) -> String {
    let mut value: u64 = 0xcbf29ce484222325;
    for cell in cells {
        for byte in cell.to_bits().to_le_bytes() {
            value = (value ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    format!("{value:016x}")
}

fn java_f64(text: &Value) -> f64 {
    text.as_str().unwrap().parse().unwrap()
}

/// `ZonedDateTime.toString` to milliseconds: `2000-01-01T06:00Z`, optional seconds, optional zone.
fn java_time(text: &str) -> Option<i64> {
    if text == "None" {
        return None;
    }
    let body = text.split('Z').next().unwrap();
    let (date, clock) = body.split_once('T').unwrap();
    let mut d = date.split('-').map(|p| p.parse::<i64>().unwrap());
    let (y, m, dd) = (d.next().unwrap(), d.next().unwrap(), d.next().unwrap());
    let mut c = clock.split(':');
    let h: i64 = c.next().unwrap().parse().unwrap();
    let mi: i64 = c.next().unwrap().parse().unwrap();
    let s: f64 = c.next().map_or(0.0, |x| x.parse().unwrap());
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + dd - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400_000 + (h * 3600 + mi * 60) * 1000 + (s * 1000.0) as i64)
}

fn type_name(t: DataType) -> &'static str {
    match t {
        DataType::PerCum => "ACCUMULATION",
        DataType::PerAver => "AVERAGE",
        DataType::InstVal => "INSTANTANEOUS",
        DataType::InstCum => "INSTANT_CUMULATIVE",
    }
}

#[test]
fn every_fixture_matches_vortex_or_is_a_named_refusal() {
    let expected = expected();
    let refusals: Value = serde_json::from_str(
        &fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/nc_expected_refusals.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let files = expected.as_object().unwrap();
    assert!(files.len() >= 50, "{} fixtures", files.len());
    let (mut decoded, mut refused, mut both, mut zero) = (0, 0, 0, 0);
    for (name, row) in files {
        let file = NetcdfFile::open(&dir().join(name)).unwrap();
        let variables = row["variables"].as_object().unwrap();
        let mut listed: Vec<String> = variables.keys().cloned().collect();
        listed.sort();
        assert_eq!(file.variable_names(), listed, "{name}: listing");
        for (variable, want) in variables {
            let got = file.variable(variable);
            if want.get("error").is_some() {
                assert!(
                    got.is_err(),
                    "{name}:{variable}: Vortex fails and this reads"
                );
                both += 1;
                continue;
            }
            let v = match got {
                Ok(v) => v,
                Err(VortexError::Refused { what, reason }) => {
                    let text = format!("{what}: {reason}");
                    let allowed = refusals[name.as_str()][variable.as_str()]
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}:{variable} refused: {text}"));
                    assert!(text.contains(allowed), "{name}:{variable}: {text}");
                    assert!(text.contains(variable.as_str()), "{name}: names {text}");
                    refused += 1;
                    continue;
                }
                Err(other) => panic!("{name}:{variable}: {other}"),
            };
            let dtos = want["dtos"].as_array().unwrap();
            assert_eq!(v.step_count() as u64, want["dto_count"].as_u64().unwrap());
            // Zero steps on both sides is its own count, never a decoded match.
            if want["dto_count"].as_u64() == Some(0) {
                assert!(dtos.is_empty(), "{name}:{variable}");
                zero += 1;
                continue;
            }
            assert!(!dtos.is_empty(), "{name}:{variable}");
            for d in dtos {
                let where_ = format!("{name}:{variable}[{}]", d["step"]);
                let s = v.step(d["step"].as_u64().unwrap() as usize).unwrap();
                assert_eq!(s.nx as u64, d["nx"].as_u64().unwrap(), "{where_}");
                assert_eq!(s.ny as u64, d["ny"].as_u64().unwrap(), "{where_}");
                for (ours, key) in [
                    (s.dx, "dx"),
                    (s.dy, "dy"),
                    (s.origin_x, "originX"),
                    (s.origin_y, "originY"),
                    (s.nodata, "noDataValue"),
                ] {
                    assert_eq!(
                        ours.to_bits(),
                        java_f64(&d[key]).to_bits(),
                        "{where_} {key}"
                    );
                }
                assert_eq!(s.wkt, d["wkt"].as_str().unwrap(), "{where_}");
                assert_eq!(s.units.as_deref(), d["units"].as_str(), "{where_}");
                assert_eq!(s.short_name, d["shortName"].as_str().unwrap(), "{where_}");
                assert_eq!(s.full_name, d["fullName"].as_str().unwrap(), "{where_}");
                assert_eq!(
                    s.description.as_deref(),
                    d["description"].as_str(),
                    "{where_}"
                );
                assert_eq!(
                    s.start,
                    java_time(d["startTime"].as_str().unwrap()),
                    "{where_}"
                );
                assert_eq!(s.end, java_time(d["endTime"].as_str().unwrap()), "{where_}");
                assert_eq!(
                    type_name(s.data_type),
                    d["dataType"].as_str().unwrap(),
                    "{where_}"
                );
                assert_eq!(
                    s.cells.len() as u64,
                    d["cells"].as_u64().unwrap(),
                    "{where_}"
                );
                assert_eq!(
                    fnv(&s.cells),
                    d["cells_fnv1a"].as_str().unwrap(),
                    "{where_}"
                );
            }
            decoded += 1;
        }
    }
    println!("decoded {decoded}, refused {refused}, both fail {both}, zero steps {zero}");
    assert!(decoded >= 70, "{decoded}");
    assert!(refused >= 7, "{refused}");
    assert!(both >= 1, "{both}");
    assert_eq!(zero, 1, "{zero}");
}

#[test]
fn fixture_bytes_are_the_recorded_ones() {
    // A changed fixture must not silently pass against an old record.
    let expected = expected();
    for (name, row) in expected.as_object().unwrap() {
        let mut value: u64 = 0xcbf29ce484222325;
        for byte in fs::read(dir().join(name)).unwrap() {
            value = (value ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        assert_eq!(
            format!("{value:016x}"),
            row["bytes_fnv1a"].as_str().unwrap(),
            "{name}"
        );
    }
    let on_disk = fs::read_dir(dir()).unwrap().count();
    assert_eq!(
        on_disk,
        expected.as_object().unwrap().len() + 1,
        "fixture count"
    );
}

#[test]
fn notebook_731_shape_becomes_per_cum_inches_with_its_window() {
    // examples/731: float32, time: sum, units in, EPSG:4269, time bounds on the model clock.
    let file = NetcdfFile::open(&dir().join("nb731_float32.nc")).unwrap();
    let frame = file.frame("precip_incremental", 1).unwrap();
    let parts = frame.parts();
    assert_eq!(parts.data_type, DataType::PerCum);
    assert_eq!(parts.units, "in");
    assert_eq!((parts.nx, parts.ny), (7, 5));
    let dss_time = match parts.time {
        dss_vortex::FrameTime::Period { start, end } => (start, end),
        ref other => panic!("{other:?}"),
    };
    // 2020-06-01 06:30 to 07:00.
    assert_eq!(dss_time.0 .1, 6 * 3600 + 1800);
    assert_eq!(dss_time.1 .1, 7 * 3600);
    assert!(frame.cell(0, 0).unwrap().is_nan());
    assert!(!frame.cell(1, 0).unwrap().is_nan());
}

#[test]
fn a_period_type_on_an_instant_reads_but_is_no_frame() {
    let file = NetcdfFile::open(&dir().join("instants.nc")).unwrap();
    let v = file.variable("rain").unwrap();
    let s = v.step(0).unwrap();
    assert_eq!(s.data_type, DataType::PerCum);
    assert_eq!(s.start, s.end);
    assert!(matches!(v.frame(0), Err(VortexError::Frame(_))));
}

#[test]
fn group_paths_and_x_first_order_read() {
    let file = NetcdfFile::open(&dir().join("group_x_first.nc4")).unwrap();
    assert_eq!(
        file.variable_names(),
        vec!["Grid/Intermediate/source", "Grid/precipitation"]
    );
    let s = file
        .variable("Grid/precipitation")
        .unwrap()
        .step(0)
        .unwrap();
    assert_eq!(s.short_name, "precipitation");
    assert_eq!((s.nx, s.ny), (6, 4));
    // No bare short name: a variable is reached by its full path alone.
    assert!(file.variable("precipitation").is_err());
}

#[test]
fn a_step_out_of_range_is_refused() {
    let file = NetcdfFile::open(&dir().join("instants.nc")).unwrap();
    assert!(file.variable("rain").unwrap().step(3).is_err());
    assert!(file.frame("no_such_variable", 0).is_err());
}

#[test]
fn cell_methods_rule_is_public_and_matches_vortex_tests() {
    // Every case in Vortex 1.1.0's NetcdfDataReaderTest.parseTimeCellMethodReadsCfSyntax.
    for (cell_methods, axis, want) in [
        ("time: mean", None, "mean"),
        ("time: sum", None, "sum"),
        ("time: point", None, "point"),
        ("area: mean time: maximum", None, "maximum"),
        ("time: mean (interval: 1 day)", None, "mean"),
        ("time: mean within days time: mean over days", None, "mean"),
        ("area: mean", None, ""),
        ("mean", None, "mean"),
        ("sum", None, "sum"),
        ("point", None, "point"),
        ("", None, ""),
        ("   ", None, ""),
        ("valid_time: mean", Some("valid_time"), "mean"),
        ("valid_time: mean", None, ""),
        ("time: mean", Some("valid_time"), "mean"),
    ] {
        assert_eq!(
            parse_time_cell_method(cell_methods, axis),
            want,
            "{cell_methods}"
        );
    }
}

/// What is wrong with a frame as the record of its step, if anything.
fn frame_disagrees(s: &dss_vortex::source::netcdf::Step, frame: &GridFrame) -> Option<String> {
    let p = frame.parts();
    let stamp = |t: i64| {
        (
            // A frame's day is HEC's julian day, on which 1970-01-01 is 25568.
            t.div_euclid(86_400_000) + 25_568,
            t.div_euclid(1000).rem_euclid(86_400),
        )
    };
    let time = match (s.start, s.end) {
        (Some(a), Some(b)) if a == b => FrameTime::Instant(stamp(b)),
        (Some(a), Some(b)) => FrameTime::Period {
            start: stamp(a),
            end: stamp(b),
        },
        _ => FrameTime::Invariant,
    };
    let bits = |c: &[f32]| c.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
    if p.time != time {
        return Some(format!("time {:?} against {time:?}", p.time));
    }
    if bits(&p.cells) != bits(&s.cells) {
        return Some("cells".to_string());
    }
    if p.data_type != s.data_type {
        return Some(format!("type {:?} against {:?}", p.data_type, s.data_type));
    }
    if p.variable != s.full_name {
        return Some(format!("variable {} against {}", p.variable, s.full_name));
    }
    if p.crs != Crs::Wkt(s.wkt.clone()) {
        return Some("crs".to_string());
    }
    if p.transform != GeoTransform::axis_aligned(s.origin_x, s.dx, s.origin_y, s.dy) {
        return Some("transform".to_string());
    }
    if p.units != s.units.clone().unwrap_or_default() || (p.nx, p.ny) != (s.nx, s.ny) {
        return Some("units or shape".to_string());
    }
    None
}

/// The gates compare steps. This ties each frame to its step, so a stamp lost between the two shows.
#[test]
fn every_frame_is_its_step_or_a_type_its_window_cannot_hold() {
    let (mut frames, mut period_on_instant, mut instant_on_period, mut fractions) = (0, 0, 0, 0);
    for name in expected().as_object().unwrap().keys() {
        let file = NetcdfFile::open(&dir().join(name)).unwrap();
        for variable in file.variable_names() {
            let Ok(v) = file.variable(&variable) else {
                continue;
            };
            for t in 0..v.step_count() {
                let Ok(s) = v.step(t) else {
                    continue;
                };
                let error = match v.frame(t) {
                    Ok(frame) => {
                        assert_eq!(frame_disagrees(&s, &frame), None, "{name}:{variable}[{t}]");
                        frames += 1;
                        continue;
                    }
                    Err(error) => error,
                };
                let whole = |m: Option<i64>| m.is_none_or(|m| m.rem_euclid(1000) == 0);
                let period = matches!(s.data_type, DataType::PerCum | DataType::PerAver);
                match error {
                    VortexError::Frame(_) if period && s.start == s.end => period_on_instant += 1,
                    VortexError::Frame(_) if !period && s.start != s.end => instant_on_period += 1,
                    VortexError::Refused { .. } if !whole(s.start) || !whole(s.end) => {
                        fractions += 1
                    }
                    other => panic!("{name}:{variable}[{t}]: {other}"),
                }
            }
        }
    }
    assert_eq!(
        (frames, period_on_instant, instant_on_period, fractions),
        (139, 3, 4, 2)
    );
}

/// Negative control for the check above: a stamp, a cell or a type moved in the step shows.
#[test]
fn a_frame_differing_from_its_step_is_noticed() {
    let file = NetcdfFile::open(&dir().join("nb731_float32.nc")).unwrap();
    let name = file.variable_names().remove(0);
    let v = file.variable(&name).unwrap();
    let s = v.step(1).unwrap();
    let frame = v.frame(1).unwrap();
    assert_eq!(frame_disagrees(&s, &frame), None);
    let mut moved = s.clone();
    moved.start = moved.start.map(|t| t + 3_600_000);
    assert!(frame_disagrees(&moved, &frame).unwrap().starts_with("time"));
    let mut moved = s.clone();
    moved.end = moved.end.map(|t| t - 1000);
    assert!(frame_disagrees(&moved, &frame).unwrap().starts_with("time"));
    let mut moved = s.clone();
    moved.cells[0] = f32::from_bits(moved.cells[0].to_bits() ^ 1);
    assert_eq!(frame_disagrees(&moved, &frame).unwrap(), "cells");
    let mut moved = s.clone();
    moved.data_type = DataType::InstVal;
    assert!(frame_disagrees(&moved, &frame).unwrap().starts_with("type"));
    let mut moved = s.clone();
    moved.full_name = "other".to_string();
    assert!(frame_disagrees(&moved, &frame)
        .unwrap()
        .starts_with("variable"));
}

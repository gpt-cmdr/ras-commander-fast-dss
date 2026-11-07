//! The time tools from file to file: failure partway, crash recovery, an existing output, stamps per
//! data type. Oracle comparison lives in `oracle/vortex/run_temporal.py`; these run on any platform.

#![allow(clippy::result_large_err)]

use dss_core::dss7::write::{FaultPoint, WriteStep};
use dss_core::dss7::Dss7File;
use dss_core::grid_write::GRID_NODATA;
use dss_vortex::frame::GeoTransform;
use dss_vortex::import::{self, ArrayInput, Failure, ImportRequest, Selection};
use dss_vortex::sink_dss::{self, Parts, SinkCrs, SinkOptions};
use dss_vortex::temporal::{
    self, parse_stamp, read_window, resample_times, shift_times, Rule, TemporalError,
    TemporalRequest,
};
use dss_vortex::units::{self, Overrides};
use dss_vortex::warp::{Align, Method};
use dss_vortex::{Crs, DataType, FrameTime, NamedCrs, VortexError};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("dss_vortex_temporal")
        .join(format!("{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const NX: usize = 3;
const NY: usize = 2;

fn pair(text: &str) -> (i64, i64) {
    let s = parse_stamp(text).unwrap();
    (s.div_euclid(86_400), s.rem_euclid(86_400))
}

fn period(d: &str, e: &str) -> FrameTime {
    FrameTime::Period {
        start: pair(d),
        end: pair(e),
    }
}

/// Writes frames through the importer, whose own tests cover it.
fn write_series(
    path: &Path,
    variable: &str,
    units: &str,
    frames: &[(FrameTime, Option<DataType>, Vec<f32>)],
) {
    for (time, data_type, cells) in frames {
        let input = ArrayInput {
            cells: cells.clone(),
            nx: NX,
            ny: NY,
            transform: GeoTransform::axis_aligned(2000.0, 2000.0, 8000.0, -2000.0),
            crs: Crs::Named(NamedCrs::Shg),
            nodata: GRID_NODATA,
            times: vec![*time],
            variable: variable.to_string(),
            description: None,
            units: units.to_string(),
            file_name: String::new(),
        };
        let mut request = ImportRequest::new(
            vec![],
            path.to_path_buf(),
            Selection::Variable(variable.into()),
            Method::Near,
        );
        request.data_type = *data_type;
        import::import_arrays(&input, &request, &mut |_| ControlFlow::Continue(())).unwrap();
    }
}

fn request(input: &Path, output: &Path) -> TemporalRequest {
    TemporalRequest {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        selection: None,
        parts: Parts::default(),
        c_part: None,
        units: None,
        data_type: None,
        version: None,
        sync: false,
        allow_undetectable_readers: true,
        fault: None,
        limits: Default::default(),
    }
}

fn hours(n: usize) -> Vec<(FrameTime, Option<DataType>, Vec<f32>)> {
    (0..n)
        .map(|i| {
            let a = parse_stamp("01JAN2024:0000").unwrap() + 3600 * i as i64;
            let time = FrameTime::Period {
                start: (a.div_euclid(86_400), a.rem_euclid(86_400)),
                end: ((a + 3600).div_euclid(86_400), (a + 3600).rem_euclid(86_400)),
            };
            let cells = (0..NX * NY).map(|c| (i * 10 + c) as f32 * 0.25).collect();
            (time, None, cells)
        })
        .collect()
}

fn go<'a>() -> impl FnMut(usize, usize) -> ControlFlow<()> + 'a {
    |_, _| ControlFlow::Continue(())
}

fn names(path: &Path) -> Vec<String> {
    let mut f = Dss7File::open(path).unwrap();
    let c = f.catalog().unwrap();
    let mut out: Vec<String> = c
        .resolved_live()
        .into_iter()
        .map(|i| c.catalog.entries[i].pathname_lossy())
        .collect();
    out.sort();
    out
}

fn message(e: &TemporalError) -> String {
    e.to_string()
}

#[test]
fn stamps_for_each_data_type() {
    let dir = scratch("stamps");
    // The record that once gave HMS no rain: a period's D and E must state start and end, per type.
    let input = dir.join("in.dss");
    write_series(
        &input,
        "PRECIPITATION",
        "MM",
        &[(
            period("01JAN2024:2300", "01JAN2024:2400"),
            None,
            vec![1.0; 6],
        )],
    );
    let out = dir.join("cum.dss");
    shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
    assert_eq!(
        names(&out),
        ["///PRECIPITATION/02JAN2024:0000/02JAN2024:0100//"]
    );
    let g = Dss7File::open(&out)
        .unwrap()
        .read_grid(b"///PRECIPITATION/02JAN2024:0000/02JAN2024:0100//")
        .unwrap();
    assert_eq!(g.data_type, 1, "PER-CUM");
    assert_eq!(g.record_type, 420, "a time-stamped Albers grid");
    assert!(g.end_time > g.start_time);

    let out = dir.join("cum2.dss");
    shift_times(&request(&input, &out), 0, 0, &mut go()).unwrap();
    assert_eq!(
        names(&out),
        ["///PRECIPITATION/01JAN2024:2300/01JAN2024:2400//"]
    );

    // PER-AVER, under another C part.
    let input = dir.join("ave.dss");
    write_series(
        &input,
        "TEMPERATURE",
        "DEG C",
        &[(
            period("01JAN2024:2300", "01JAN2024:2400"),
            None,
            vec![1.0; 6],
        )],
    );
    let out = dir.join("ave_out.dss");
    shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
    assert_eq!(
        names(&out),
        ["///TEMPERATURE/02JAN2024:0000/02JAN2024:0100//"]
    );
    let g = Dss7File::open(&out)
        .unwrap()
        .read_grid(b"///TEMPERATURE/02JAN2024:0000/02JAN2024:0100//")
        .unwrap();
    assert_eq!(g.data_type, 0, "PER-AVER");

    // INST-VAL: one part, 2400 at midnight.
    let input = dir.join("inst.dss");
    write_series(
        &input,
        "ALBEDO",
        "UNDEF",
        &[(
            FrameTime::Instant(pair("01JAN2024:2300")),
            None,
            vec![1.0; 6],
        )],
    );
    let out = dir.join("inst_out.dss");
    shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
    assert_eq!(names(&out), ["///ALBEDO/01JAN2024:2400///"]);
    let g = Dss7File::open(&out)
        .unwrap()
        .read_grid(b"///ALBEDO/01JAN2024:2400///")
        .unwrap();
    assert_eq!(g.data_type, 2, "INST-VAL");

    // INST-CUM stated: D and E, both of the one instant.
    let out = dir.join("inst_cum.dss");
    let mut r = request(&input, &out);
    r.data_type = Some(DataType::InstCum);
    shift_times(&r, 3600, 3600, &mut go()).unwrap();
    assert_eq!(names(&out), ["///ALBEDO/02JAN2024:0000/01JAN2024:2400//"]);
}

#[test]
fn volume_is_kept_over_complete_windows_on_a_file() {
    let dir = scratch("volume");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(24));
    let total: f64 = hours(24)
        .iter()
        .flat_map(|(_, _, c)| c.iter().map(|&v| f64::from(v)))
        .sum();
    for minutes in [15, 30, 60, 120, 180, 240, 360, 480, 720, 1440] {
        let out = dir.join(format!("{minutes}.dss"));
        let report =
            resample_times(&request(&input, &out), minutes, Rule::Strict, &mut go()).unwrap();
        assert_eq!(report.records.len() as i64, 1440 / minutes, "{minutes}");
        assert_eq!(report.dropped_tail_seconds, 0);
        assert_eq!(report.windows_every_cell_missing, 0);
        let mut f = Dss7File::open(&out).unwrap();
        let mut sum = 0.0f64;
        for name in names(&out) {
            let g = f.read_grid(name.as_bytes()).unwrap();
            sum += g.data.iter().map(|&v| f64::from(v)).sum::<f64>();
        }
        // Quarters of quarters: every value and every sum is exact in `f32`, so this is equality.
        assert_eq!(sum, total, "{minutes} min");
    }
}

#[test]
fn a_gap_is_refused_before_the_output_exists_and_vortex_rule_writes_it() {
    let dir = scratch("gap");
    let input = dir.join("in.dss");
    let mut frames = hours(6);
    frames.remove(3);
    write_series(&input, "PRECIPITATION", "MM", &frames);
    let out = dir.join("out.dss");
    let e = resample_times(&request(&input, &out), 360, Rule::Strict, &mut go()).unwrap_err();
    assert!(message(&e).contains("has no source"), "{e}");
    assert_eq!(e.phase, "planning");
    assert!(!out.exists(), "a refusal leaves no file");
    let report = resample_times(&request(&input, &out), 360, Rule::Vortex, &mut go()).unwrap();
    assert_eq!(report.records.len(), 1);
    // The partial volume is what Vortex holds: five hours', and no marker for the sixth.
    let g = Dss7File::open(&out)
        .unwrap()
        .read_grid(names(&out)[0].as_bytes())
        .unwrap();
    let want: f32 = frames.iter().map(|f| f.2[0]).sum();
    assert_eq!(
        g.data[g.nx], want,
        "row order aside, the first cell of the north row"
    );
}

#[test]
fn leading_windows_are_all_missing_and_the_tail_is_reported() {
    let dir = scratch("align");
    let input = dir.join("in.dss");
    // 07:00 to 13:30 in half-hours, in steps of 3 hours from midnight.
    let frames: Vec<_> = (0..13)
        .map(|i| {
            let a = parse_stamp("01JAN2024:0700").unwrap() + 1800 * i as i64;
            let p = |t: i64| (t.div_euclid(86_400), t.rem_euclid(86_400));
            (
                FrameTime::Period {
                    start: p(a),
                    end: p(a + 1800),
                },
                None,
                vec![1.0; 6],
            )
        })
        .collect();
    write_series(&input, "PRECIPITATION", "MM", &frames);
    let out = dir.join("out.dss");
    let report = resample_times(&request(&input, &out), 180, Rule::Strict, &mut go()).unwrap();
    // 00-03, 03-06, 06-09 begin before the series; 09-12 holds 6 half-hours; 12-15 overruns.
    assert_eq!(report.records.len(), 4);
    assert_eq!(report.windows_every_cell_missing, 3);
    assert_eq!(report.dropped_tail_seconds, 5400);
    let mut f = Dss7File::open(&out).unwrap();
    let held = f
        .read_grid(b"///PRECIPITATION/01JAN2024:0900/01JAN2024:1200//")
        .unwrap();
    assert!(held.data.iter().all(|&v| v == 6.0));
    let empty = f
        .read_grid(b"///PRECIPITATION/01JAN2024:0600/01JAN2024:0900//")
        .unwrap();
    assert!(empty.data.iter().all(|&v| v == GRID_NODATA));
}

#[test]
fn a_missing_cell_stays_missing_in_every_type() {
    let dir = scratch("missing");
    for (variable, units) in [("PRECIPITATION", "MM"), ("TEMPERATURE", "DEG C")] {
        let input = dir.join(format!("{variable}.dss"));
        let mut frames = hours(4);
        frames[1].2[0] = GRID_NODATA;
        write_series(&input, variable, units, &frames);
        let out = dir.join(format!("{variable}_out.dss"));
        resample_times(&request(&input, &out), 240, Rule::Strict, &mut go()).unwrap();
        let g = Dss7File::open(&out)
            .unwrap()
            .read_grid(names(&out)[0].as_bytes())
            .unwrap();
        // Buffer cell 0 of a north-up frame is row ny - 1 in the record.
        assert_eq!(g.data[NX * (NY - 1)], GRID_NODATA, "{variable}");
        assert_eq!(g.data.iter().filter(|&&v| v == GRID_NODATA).count(), 1);
    }
}

#[test]
fn refusals_leave_no_file() {
    let dir = scratch("refusals");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(3));
    let out = dir.join("out.dss");
    let holding = |e: TemporalError, text: &str| {
        assert!(message(&e).contains(text), "{e}");
        assert_eq!(e.phase, "planning");
        assert!(e.completed.is_empty());
    };
    holding(
        shift_times(&request(&input, &out), 30, 30, &mut go()).unwrap_err(),
        "whole minute",
    );
    holding(
        shift_times(&request(&input, &out), 3600, 0, &mut go()).unwrap_err(),
        "precipitation at an instant",
    );
    holding(
        shift_times(&request(&input, &out), 0, -7200, &mut go()).unwrap_err(),
        "before it starts",
    );
    holding(
        shift_times(&request(&input, &input), 3600, 3600, &mut go()).unwrap_err(),
        "it is also the input",
    );
    let mut r = request(&input, &out);
    r.selection = Some(vec!["///NOT/THERE/////".into()]);
    holding(
        shift_times(&r, 3600, 3600, &mut go()).unwrap_err(),
        "not a grid record",
    );
    let mut r = request(&input, &out);
    r.parts.a = "*".into();
    holding(
        shift_times(&r, 3600, 3600, &mut go()).unwrap_err(),
        "cannot be '*'",
    );
    let mut r = request(&input, &out);
    r.selection = Some(vec![]);
    holding(
        shift_times(&r, 3600, 3600, &mut go()).unwrap_err(),
        "holds no grid record",
    );
    holding(
        resample_times(&request(&input, &out), 1440, Rule::Vortex, &mut go()).unwrap_err(),
        "no complete window",
    );
    holding(
        resample_times(&request(&input, &out), 7, Rule::Vortex, &mut go()).unwrap_err(),
        "not one of Vortex's",
    );
    assert!(!out.exists());
    // An empty file: nothing to select.
    let empty = dir.join("empty.dss");
    dss_core::dss7::write::create_dss7(&empty).unwrap();
    holding(
        shift_times(&request(&empty, &out), 3600, 3600, &mut go()).unwrap_err(),
        "holds no grid record",
    );
    assert!(!out.exists());
    // Not DSS at all.
    let text = dir.join("text.dss");
    std::fs::write(
        &text,
        b"not a dss file, and long enough to read a header from it at all",
    )
    .unwrap();
    assert!(shift_times(&request(&text, &out), 3600, 3600, &mut go()).is_err());
    assert!(!out.exists());
    // Output that is not DSS is left alone.
    let bad = dir.join("bad.dss");
    std::fs::write(&bad, b"keep me").unwrap();
    assert!(shift_times(&request(&input, &bad), 3600, 3600, &mut go()).is_err());
    assert_eq!(std::fs::read(&bad).unwrap(), b"keep me");
}

// Records a reader can find. After a simulated crash, a catalog entry can lead to nothing yet.
fn snapshot(path: &Path) -> Vec<(String, Vec<u32>)> {
    let mut f = Dss7File::open(path).unwrap();
    names(path)
        .into_iter()
        .filter_map(|n| match f.read_grid(n.as_bytes()) {
            Ok(g) => Some((n, g.data.iter().map(|v| v.to_bits()).collect())),
            Err(dss_core::GridError::NotFound) => None,
            Err(e) => panic!("{n}: {e}"),
        })
        .collect()
}

#[test]
fn a_crash_at_every_write_step_leaves_other_records_and_a_rerun_heals() {
    let dir = scratch("crash");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(3));
    let clean = dir.join("clean.dss");
    shift_times(&request(&input, &clean), 3600, 3600, &mut go()).unwrap();
    let want = snapshot(&clean);
    assert_eq!(want.len(), 3);
    let other = (
        period("01JAN2024:0000", "01JAN2024:0100"),
        None,
        vec![5.0; 6],
    );
    let mut stopped = 0;
    for (replacing, label) in [(false, "new"), (true, "replace")] {
        for (i, step) in WriteStep::ALL.into_iter().enumerate() {
            let out = dir.join(format!("{label}{i}.dss"));
            write_series(&out, "TEMPERATURE", "DEG C", std::slice::from_ref(&other));
            if replacing {
                shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
            }
            let before = snapshot(&out);
            let mut r = request(&input, &out);
            r.fault = Some((1, FaultPoint { block: 0, step }));
            let Err(e) = shift_times(&r, 3600, 3600, &mut go()) else {
                continue; // This step is not on this write path.
            };
            stopped += 1;
            assert_eq!(e.phase, "writing", "{step:?}");
            assert!(matches!(e.failure, Failure::Write(_)), "{step:?}");
            assert_eq!(e.completed.len(), 1, "{step:?}");
            // `completed` holds acknowledged writes only, and the one being written is named.
            assert_eq!(e.in_doubt.as_deref(), Some(want[1].0.as_str()), "{step:?}");
            assert!(message(&e).contains("may or may not be in the file"), "{e}");
            // The untouched record and the finished one are whole. The stopped record is absent
            // or whole, in old cells or new, which are equal here.
            let after = snapshot(&out);
            let kept = before.iter().find(|b| b.0.contains("TEMPERATURE")).unwrap();
            assert!(after.contains(kept), "{step:?}");
            assert!(after.contains(&want[0]), "{step:?}");
            for row in after.iter().filter(|r| r.0.contains("PRECIPITATION")) {
                assert!(want.contains(row), "{step:?} {}", row.0);
            }
            // A rerun finishes the file.
            shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
            let healed = snapshot(&out);
            assert_eq!(healed.len(), 4, "{step:?}");
            for row in &want {
                assert!(healed.contains(row), "{step:?} {}", row.0);
            }
        }
    }
    assert!(stopped >= 10, "only {stopped} steps stopped the writer");
}

#[test]
fn a_cancel_keeps_what_finished() {
    let dir = scratch("cancel");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(4));
    let out = dir.join("out.dss");
    let mut seen = 0;
    let mut stop_after_two = |_: usize, _: usize| {
        seen += 1;
        if seen == 2 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let e = shift_times(&request(&input, &out), 3600, 3600, &mut stop_after_two).unwrap_err();
    assert!(matches!(e.failure, Failure::Cancelled));
    assert_eq!(e.completed.len(), 2);
    assert_eq!(names(&out).len(), 2);
}

#[test]
fn an_existing_output_keeps_its_other_records() {
    let dir = scratch("existing");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(2));
    let out = dir.join("out.dss");
    write_series(
        &out,
        "TEMPERATURE",
        "DEG C",
        &[(
            period("01JAN2024:0000", "01JAN2024:0100"),
            None,
            vec![5.0; 6],
        )],
    );
    let before = snapshot(&out);
    let report = shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
    assert!(report.records.iter().all(|r| r.record.outcome == "added"));
    let after = snapshot(&out);
    assert!(after.contains(&before[0]));
    assert_eq!(after.len(), 3);
    // Asking for another version than the file's is refused before writing.
    let mut r = request(&input, &out);
    r.version = Some(6);
    let e = shift_times(&r, 3600, 3600, &mut go()).unwrap_err();
    assert!(message(&e).contains("Writers keep"), "{e}");
    assert_eq!(snapshot(&out), after);
}

#[test]
fn dss6_output_writes_and_reads_back() {
    let dir = scratch("v6");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(2));
    let out = dir.join("out6.dss");
    let mut r = request(&input, &out);
    r.version = Some(6);
    shift_times(&r, 3600, 3600, &mut go()).unwrap();
    // And a DSS6 input.
    let back = dir.join("back.dss");
    let report = shift_times(&request(&out, &back), -3600, -3600, &mut go()).unwrap();
    assert_eq!(report.records.len(), 2);
    assert_eq!(
        snapshot(&back),
        snapshot(&{
            let same = dir.join("same.dss");
            shift_times(&request(&input, &same), 0, 0, &mut go()).unwrap();
            same
        })
    );
}

#[test]
fn reads_refuse_what_they_cannot_state() {
    let dir = scratch("reads");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(3));
    let t = |s: &str| parse_stamp(s).unwrap();
    let series = "///PRECIPITATION/*/*//";
    let e = read_window(
        &input,
        series,
        t("01JAN2024:0100"),
        t("01JAN2024:0100"),
        Rule::Strict,
    )
    .unwrap_err();
    assert!(matches!(e, VortexError::Refused { .. }), "{e}");
    let w = read_window(
        &input,
        series,
        t("01JAN2024:0000"),
        t("01JAN2024:0300"),
        Rule::Strict,
    )
    .unwrap();
    assert_eq!(w.cells.len(), NX * NY);
    let before = read_window(
        &input,
        series,
        t("31DEC2023:2300"),
        t("01JAN2024:0100"),
        Rule::Strict,
    )
    .unwrap();
    assert!(before.cells.iter().all(|&v| v == GRID_NODATA));
    assert!(temporal::series_info(&input, "///NOT/*/*//").is_err());
}

// ---- values, files and partial writes -------------------------------------------------------------

/// Cells stored as given, past the importer's refusal: how a file from elsewhere can hold -901.
fn raw_series(path: &Path, c: &str, units_text: &str, frames: &[(FrameTime, Vec<f32>)]) {
    if !path.exists() {
        dss_core::dss7::write::create_dss7(path).unwrap();
    }
    let t = GeoTransform::axis_aligned(2000.0, 2000.0, 8000.0, -2000.0);
    let place = sink_dss::placement(&t, NY, Align::Vortex).unwrap();
    let crs = SinkCrs::of_frame(&Crs::Named(NamedCrs::Shg)).unwrap();
    for (time, cells) in frames {
        let meaning =
            units::record_meaning(c, Some(c), "", units_text, time, &Overrides::default()).unwrap();
        // As stored: no conversion, since the file is what is being described.
        let mut meaning = meaning;
        meaning.units = units_text.to_string();
        meaning.conversion = units::Conversion::None;
        let derived = meaning.c_part.clone();
        let planned =
            sink_dss::plan_record(&Parts::default(), meaning, &derived, Some(c), time).unwrap();
        let options = SinkOptions {
            version: 7,
            sync: false,
            allow_undetectable_readers: true,
            fault: None,
        };
        sink_dss::write_record(path, &planned, &crs, place, cells, NX, NY, &options).unwrap();
    }
}

fn hourly(values: &[f32]) -> Vec<(FrameTime, Vec<f32>)> {
    values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let a = parse_stamp("01JAN2024:0000").unwrap() + 3600 * i as i64;
            let at = |t: i64| (t.div_euclid(86_400), t.rem_euclid(86_400));
            let time = FrameTime::Period {
                start: at(a),
                end: at(a + 3600),
            };
            (time, vec![1.0, 2.0, 3.0, 4.0, 5.0, v])
        })
        .collect()
}

/// The refusal is planning's, names the record and time, and neither makes a file nor changes one.
fn assert_refused_before_writing(
    run: &dyn Fn(&TemporalRequest) -> Result<temporal::TemporalReport, TemporalError>,
    input: &Path,
    dir: &Path,
    text: &[&str],
) {
    let out = dir.join("fresh_output.dss");
    let e = run(&request(input, &out)).unwrap_err();
    assert_eq!(e.phase, "planning", "{e}");
    assert!(e.completed.is_empty(), "{e}");
    assert_eq!(e.in_doubt, None, "{e}");
    for t in text {
        assert!(message(&e).contains(t), "wanted {t:?}: {e}");
    }
    assert!(
        matches!(e.failure, Failure::Vortex(VortexError::Refused { .. })),
        "{e}"
    );
    assert!(!out.exists(), "a refusal made a file");
    let kept = dir.join("kept_output.dss");
    let _ = std::fs::remove_file(&kept);
    write_series(
        &kept,
        "TEMPERATURE",
        "DEG C",
        &[(
            period("01JAN2024:0000", "01JAN2024:0100"),
            None,
            vec![5.0; 6],
        )],
    );
    let before = std::fs::read(&kept).unwrap();
    run(&request(input, &kept)).unwrap_err();
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        before,
        "a refusal changed a file"
    );
}

#[test]
fn a_shifted_cell_the_reader_would_lose_is_refused_before_anything_is_written() {
    for value in [-901.0f32, -902.0] {
        let dir = scratch(&format!("shift_lost_{}", -value as i32));
        let input = dir.join("in.dss");
        // The last record is the one: earlier records would otherwise be written first.
        raw_series(&input, "PRECIPITATION", "MM", &hourly(&[0.5, 0.25, value]));
        assert_refused_before_writing(
            &|r| shift_times(r, 3600, 3600, &mut go()),
            &input,
            &dir,
            &[
                "take as missing",
                "03:00",
                "2024-01-01T03:00 to 2024-01-01T04:00",
            ],
        );
    }
}

#[test]
fn a_mean_or_an_accumulation_landing_there_is_refused_under_either_rule() {
    for rule in [Rule::Strict, Rule::Vortex] {
        // Windows 00-02 and 02-04. The first is fine, so a run that wrote as it went would leave it.
        let dir = scratch(&format!("mean_lost_{rule:?}"));
        let input = dir.join("mean.dss");
        raw_series(
            &input,
            "TEMPERATURE",
            "DEG C",
            &hourly(&[1.0, 2.0, -800.0, -1002.0]),
        );
        assert_refused_before_writing(
            &|r| resample_times(r, 120, rule, &mut go()),
            &input,
            &dir,
            &[
                "take as missing",
                "2024-01-01T02:00 to 2024-01-01T04:00",
                "TEMPERATURE",
            ],
        );

        let dir = scratch(&format!("sum_lost_{rule:?}"));
        let input = dir.join("sum.dss");
        raw_series(
            &input,
            "PRECIPITATION",
            "MM",
            &hourly(&[1.0, 2.0, -450.5, -451.5]),
        );
        assert_refused_before_writing(
            &|r| resample_times(r, 120, rule, &mut go()),
            &input,
            &dir,
            &[
                "take as missing",
                "2024-01-01T02:00 to 2024-01-01T04:00",
                "-902",
            ],
        );
    }
}

#[test]
fn only_the_f32_cast_lands_there() {
    // In f64 this sum is not -901: (r + cell) as f32 rounds to it on the third step.
    let x = -901.0f32 / 3.0;
    assert_ne!(3.0 * f64::from(x), -901.0, "the case must need the cast");
    let mut r = [0.0f32];
    for _ in 0..3 {
        temporal::accumulate(&mut r, &[x], 1.0);
    }
    assert_eq!(r[0], -901.0, "the case must land");
    let dir = scratch("cast_lost");
    let input = dir.join("cast.dss");
    raw_series(&input, "PRECIPITATION", "MM", &hourly(&[x, x, x]));
    assert_refused_before_writing(
        &|r| resample_times(r, 180, Rule::Strict, &mut go()),
        &input,
        &dir,
        &["take as missing", "2024-01-01T00:00 to 2024-01-01T03:00"],
    );
}

#[test]
fn a_unit_conversion_landing_there_is_refused() {
    // Kelvin: `value + -273.15f32`, as `Conversion::convert` adds it.
    let kelvin = -901.0f32 - -273.15f32;
    assert_eq!(kelvin + -273.15f32, -901.0, "the case must land exactly");
    let dir = scratch("kelvin_lost");
    let input = dir.join("k.dss");
    raw_series(&input, "TEMPERATURE", "K", &hourly(&[1.0, kelvin]));
    assert_refused_before_writing(
        &|r| shift_times(r, 3600, 3600, &mut go()),
        &input,
        &dir,
        &["take as missing", "TEMPERATURE"],
    );
}

#[test]
fn a_source_cell_the_reader_calls_missing_differs_by_rule_as_stated() {
    let dir = scratch("source_lost");
    let input = dir.join("in.dss");
    raw_series(
        &input,
        "PRECIPITATION",
        "MM",
        &hourly(&[1.0, -901.0, 3.0, 4.0]),
    );
    // Strict refuses, and names the source record.
    assert_refused_before_writing(
        &|r| resample_times(r, 120, Rule::Strict, &mut go()),
        &input,
        &dir,
        &["01JAN2024:0100", "takes as missing", "rule=\"vortex\""],
    );
    let t = parse_stamp("01JAN2024:0000").unwrap();
    let e = read_window(&input, "///PRECIPITATION/*/*//", t, t + 7200, Rule::Strict).unwrap_err();
    assert!(e.to_string().contains("takes as missing"), "{e}");
    // Vortex's rule: a number, as there: 1 + -901 in the last cell, and 3 + 4.
    let out = dir.join("vortex.dss");
    resample_times(&request(&input, &out), 120, Rule::Vortex, &mut go()).unwrap();
    let g = Dss7File::open(&out)
        .unwrap()
        .read_grid(b"///PRECIPITATION/01JAN2024:0000/01JAN2024:0200//")
        .unwrap();
    assert_eq!(g.data[5], 1.0 - 901.0);
    // At the point it would be stored as itself, Vortex's rule refuses too.
    let out = dir.join("vortex_same.dss");
    let e = resample_times(&request(&input, &out), 60, Rule::Vortex, &mut go()).unwrap_err();
    assert!(message(&e).contains("take as missing"), "{e}");
    assert!(!out.exists());
}

#[test]
fn a_cast_past_f32_is_refused_under_the_strict_rule() {
    let dir = scratch("overflow");
    let input = dir.join("in.dss");
    let big = 3.0e38f32;
    raw_series(&input, "PRECIPITATION", "MM", &hourly(&[big, big]));
    let out = dir.join("out.dss");
    let e = resample_times(&request(&input, &out), 120, Rule::Strict, &mut go()).unwrap_err();
    assert!(message(&e).contains("infinity"), "{e}");
    assert!(!out.exists());
}

fn assert_same_file_is_refused(input: &Path, other_name: &Path, before: &[u8]) {
    for run in [
        shift_times(&request(input, other_name), 3600, 3600, &mut go()),
        resample_times(&request(input, other_name), 60, Rule::Strict, &mut go()),
    ] {
        let e = run.unwrap_err();
        assert!(message(&e).contains("it is also the input"), "{e}");
        assert_eq!(e.phase, "planning");
        assert!(e.completed.is_empty());
    }
    assert_eq!(std::fs::read(input).unwrap(), before);
    assert_eq!(std::fs::read(other_name).unwrap(), before);
}

#[test]
fn a_hard_link_to_the_input_is_refused_and_both_names_keep_their_bytes() {
    let dir = scratch("hard_link");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(2));
    let link = dir.join("alias.dss");
    std::fs::hard_link(&input, &link).unwrap();
    let before = std::fs::read(&input).unwrap();
    assert_same_file_is_refused(&input, &link, &before);
    // The same path, and a path spelled another way.
    assert_same_file_is_refused(&input, &input, &before);
    assert_same_file_is_refused(&input, &dir.join(".").join("in.dss"), &before);
    // A copy with equal bytes is another file, and may be written.
    let copy = dir.join("copy.dss");
    std::fs::copy(&input, &copy).unwrap();
    shift_times(&request(&input, &copy), 3600, 3600, &mut go()).unwrap();
    assert_eq!(std::fs::read(&input).unwrap(), before);
}

#[test]
fn a_symlink_to_the_input_is_refused() {
    let dir = scratch("symlink");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(2));
    let link = dir.join("alias.dss");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&input, &link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&input, &link);
    match made {
        Ok(()) => {
            let before = std::fs::read(&input).unwrap();
            assert_same_file_is_refused(&input, &link, &before);
        }
        // ERROR_PRIVILEGE_NOT_HELD. Named, so that a skipped check is not read as a passed one. Any other
        // failure, and any failure on Unix, is a failure.
        Err(e) if cfg!(windows) && e.raw_os_error() == Some(1314) => {
            println!("SKIPPED: this account may not make a symlink; no symlink was tested");
        }
        Err(e) => panic!("could not make a symlink: {e}"),
    }
}

// Needs no privilege, so it runs where a symlink cannot be made: one file through a directory junction.
#[cfg(windows)]
#[test]
fn a_path_through_a_junction_to_the_input_is_refused() {
    let dir = scratch("junction");
    let real = dir.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let input = real.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(2));
    let junction = dir.join("through");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&real)
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    let before = std::fs::read(&input).unwrap();
    assert_same_file_is_refused(&input, &junction.join("in.dss"), &before);
}

#[test]
fn an_input_changed_during_a_run_stops_it_and_says_what_was_written() {
    for tool in ["shift", "resample"] {
        let dir = scratch(&format!("changed_{tool}"));
        let input = dir.join("in.dss");
        write_series(&input, "PRECIPITATION", "MM", &hours(4));
        let out = dir.join("out.dss");
        let extra = (
            period("02JAN2024:0000", "02JAN2024:0100"),
            None,
            vec![7.0; NX * NY],
        );
        let mut change = |done: usize, _: usize| {
            if done == 1 {
                write_series(&input, "PRECIPITATION", "MM", std::slice::from_ref(&extra));
            }
            ControlFlow::Continue(())
        };
        let run = if tool == "shift" {
            shift_times(&request(&input, &out), 3600, 3600, &mut change)
        } else {
            resample_times(&request(&input, &out), 60, Rule::Strict, &mut change)
        };
        let e = run.unwrap_err();
        assert!(message(&e).contains("changed since"), "{e}");
        assert_eq!(e.phase, "writing");
        assert_eq!(e.completed.len(), 1, "{tool}");
        assert_eq!(e.in_doubt, None, "{tool}");
        assert_eq!(names(&out).len(), 1, "{tool}");
    }
}

#[test]
fn neither_a_refusal_nor_a_cancel_has_a_record_in_doubt() {
    let dir = scratch("not_in_doubt");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(3));
    let out = dir.join("out.dss");
    let e = shift_times(&request(&input, &out), 30, 30, &mut go()).unwrap_err();
    assert_eq!(e.in_doubt, None);
    let mut stop_after_one = |done: usize, _: usize| {
        if done == 1 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let e = shift_times(&request(&input, &out), 3600, 3600, &mut stop_after_one).unwrap_err();
    assert!(matches!(e.failure, Failure::Cancelled));
    assert_eq!(e.completed.len(), 1);
    assert_eq!(e.in_doubt, None);
    assert!(!message(&e).contains("may or may not"));
}

#[test]
fn a_resampler_write_failure_names_the_record_in_doubt() {
    let dir = scratch("resample_doubt");
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &hours(4));
    let out = dir.join("out.dss");
    let mut r = request(&input, &out);
    r.fault = Some((
        1,
        FaultPoint {
            block: 0,
            step: WriteStep::ALL[0],
        },
    ));
    let e = resample_times(&r, 120, Rule::Strict, &mut go()).unwrap_err();
    assert_eq!(e.phase, "writing");
    assert_eq!(e.completed.len(), 1);
    assert_eq!(
        e.in_doubt.as_deref(),
        Some("///PRECIPITATION/01JAN2024:0200/01JAN2024:0400//")
    );
}

#[test]
fn a_rerun_replaces_its_windows_and_leaves_every_other_record() {
    let dir = scratch("rerun");
    let long = dir.join("long.dss");
    write_series(&long, "PRECIPITATION", "MM", &hours(4));
    let short = dir.join("short.dss");
    let other: Vec<_> = hours(2)
        .into_iter()
        .map(|(t, d, _)| (t, d, vec![9.5; NX * NY]))
        .collect();
    write_series(&short, "PRECIPITATION", "MM", &other);

    // The shifter.
    let out = dir.join("shift.dss");
    shift_times(&request(&long, &out), 3600, 3600, &mut go()).unwrap();
    let first = snapshot(&out);
    assert_eq!(first.len(), 4);
    let report = shift_times(&request(&short, &out), 3600, 3600, &mut go()).unwrap();
    assert!(report
        .records
        .iter()
        .all(|r| r.record.outcome == "replaced"));
    let second = snapshot(&out);
    assert_eq!(second.len(), 4, "no record is removed");
    // Sorted by name, and the two later windows are the earlier run's, bit for bit.
    let by_name = |rows: &[(String, Vec<u32>)], part: &str| {
        rows.iter().find(|r| r.0.contains(part)).unwrap().1.clone()
    };
    for part in ["N/01JAN2024:0300/", "N/01JAN2024:0400/"] {
        assert_eq!(by_name(&second, part), by_name(&first, part), "{part}");
    }
    for part in ["N/01JAN2024:0100/", "N/01JAN2024:0200/"] {
        assert_ne!(by_name(&second, part), by_name(&first, part), "{part}");
        assert!(by_name(&second, part)
            .iter()
            .all(|&b| b == 9.5f32.to_bits()));
    }

    // The resampler.
    let out = dir.join("resample.dss");
    resample_times(&request(&long, &out), 60, Rule::Strict, &mut go()).unwrap();
    let first = snapshot(&out);
    resample_times(&request(&short, &out), 60, Rule::Strict, &mut go()).unwrap();
    let second = snapshot(&out);
    assert_eq!(second.len(), 4);
    assert_eq!(
        by_name(&second, "01JAN2024:0200/01JAN2024:0300"),
        by_name(&first, "01JAN2024:0200/01JAN2024:0300")
    );
    assert_eq!(
        by_name(&second, "01JAN2024:0300/01JAN2024:0400"),
        by_name(&first, "01JAN2024:0300/01JAN2024:0400")
    );
    assert!(by_name(&second, "01JAN2024:0000/01JAN2024:0100")
        .iter()
        .all(|&b| b == 9.5f32.to_bits()));
}

// A period and an instant, moved across each boundary, in each type.
#[test]
fn stamps_at_month_and_year_ends_for_each_type() {
    // (last hour before the boundary, hour after it, first after, midnight as 2400, midnight as 0000)
    let cases = [
        ("31JAN2024", "01FEB2024"),
        ("29FEB2024", "01MAR2024"),
        ("28FEB2023", "01MAR2023"),
        ("30APR2024", "01MAY2024"),
        ("31DEC2023", "01JAN2024"),
    ];
    let dir = scratch("month_ends");
    for (i, (last, next)) in cases.iter().enumerate() {
        let window = period(&format!("{last}:2300"), &format!("{last}:2400"));
        for (label, c, u, want_type) in [
            ("cum", "PRECIPITATION", "MM", 1),
            ("ave", "TEMPERATURE", "DEG C", 0),
        ] {
            let input = dir.join(format!("{label}{i}.dss"));
            write_series(&input, c, u, &[(window, None, vec![1.5; 6])]);
            // Stays: a period's end at midnight says 2400.
            let out = dir.join(format!("{label}{i}_same.dss"));
            shift_times(&request(&input, &out), 0, 0, &mut go()).unwrap();
            let name = format!("///{c}/{last}:2300/{last}:2400//");
            assert_eq!(names(&out), std::slice::from_ref(&name));
            let g = Dss7File::open(&out)
                .unwrap()
                .read_grid(name.as_bytes())
                .unwrap();
            assert_eq!(g.data_type, want_type, "{name}");
            assert_eq!(
                g.end_time.unwrap() - g.start_time.unwrap(),
                3600,
                "{name}: seconds"
            );
            // Forward: a period's start at midnight says 0000, of the next day, in the next month.
            let out = dir.join(format!("{label}{i}_next.dss"));
            shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
            let name = format!("///{c}/{next}:0000/{next}:0100//");
            assert_eq!(names(&out), std::slice::from_ref(&name));
            let g = Dss7File::open(&out)
                .unwrap()
                .read_grid(name.as_bytes())
                .unwrap();
            assert_eq!(g.data_type, want_type, "{name}");
            assert_eq!(
                g.end_time.unwrap() - g.start_time.unwrap(),
                3600,
                "{name}: seconds"
            );
        }
        let input = dir.join(format!("inst{i}.dss"));
        write_series(
            &input,
            "ALBEDO",
            "UNDEF",
            &[(
                FrameTime::Instant(pair(&format!("{last}:2300"))),
                None,
                vec![1.5; 6],
            )],
        );
        // INST-VAL: one part, 2400 at midnight.
        let out = dir.join(format!("inst{i}_out.dss"));
        shift_times(&request(&input, &out), 3600, 3600, &mut go()).unwrap();
        let name = format!("///ALBEDO/{last}:2400///");
        assert_eq!(names(&out), std::slice::from_ref(&name));
        let g = Dss7File::open(&out)
            .unwrap()
            .read_grid(name.as_bytes())
            .unwrap();
        assert_eq!(g.data_type, 2, "{name}");
        // INST-CUM stated: D says 0000 of the next day and E 2400 of the last, one instant.
        let out = dir.join(format!("instcum{i}_out.dss"));
        let mut r = request(&input, &out);
        r.data_type = Some(DataType::InstCum);
        shift_times(&r, 3600, 3600, &mut go()).unwrap();
        let name = format!("///ALBEDO/{next}:0000/{last}:2400//");
        assert_eq!(names(&out), std::slice::from_ref(&name));
        let g = Dss7File::open(&out)
            .unwrap()
            .read_grid(name.as_bytes())
            .unwrap();
        assert_eq!(g.data_type, 3, "{name}");
        assert_eq!(g.end_time, g.start_time, "{name}");
    }
}

#[test]
fn resampled_windows_cross_month_and_year_ends_on_their_stamps() {
    let dir = scratch("window_ends");
    // Six hours either side of the year's end.
    let at = |h: i64| {
        let t = parse_stamp("31DEC2023:1800").unwrap() + h * 3600;
        (t.div_euclid(86_400), t.rem_euclid(86_400))
    };
    let frames: Vec<_> = (0..12)
        .map(|h| {
            (
                FrameTime::Period {
                    start: at(h),
                    end: at(h + 1),
                },
                None,
                vec![0.5f32; NX * NY],
            )
        })
        .collect();
    let input = dir.join("in.dss");
    write_series(&input, "PRECIPITATION", "MM", &frames);
    let out = dir.join("out.dss");
    resample_times(&request(&input, &out), 360, Rule::Strict, &mut go()).unwrap();
    let want: Vec<String> = [
        "///PRECIPITATION/01JAN2024:0000/01JAN2024:0600//",
        "///PRECIPITATION/31DEC2023:0000/31DEC2023:0600//",
        "///PRECIPITATION/31DEC2023:0600/31DEC2023:1200//",
        "///PRECIPITATION/31DEC2023:1200/31DEC2023:1800//",
        "///PRECIPITATION/31DEC2023:1800/31DEC2023:2400//",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(names(&out), want);
    let mut f = Dss7File::open(&out).unwrap();
    // Before the series: every cell missing. Six hours of 0.5 each after it.
    let g = f.read_grid(want[1].as_bytes()).unwrap();
    assert!(g.data.iter().all(|&c| c == GRID_NODATA));
    for name in [&want[0], &want[4]] {
        let g = f.read_grid(name.as_bytes()).unwrap();
        assert!(g.data.iter().all(|&c| c == 3.0), "{name}");
    }
}

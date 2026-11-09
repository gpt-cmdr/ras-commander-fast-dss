//! A stated accumulation on GRIB2 input, with no oracle. Inputs are the formula-made
//! MRMS message in `fixtures/public/vortex_p46`, with its parameter and time patched, so every duration
//! and calendar edge is reached. Expected stamps are literals, never computed by the code under test.

#![allow(clippy::result_large_err)] // `ImportError`: see `import.rs`.
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{FaultPoint, WriteStep};
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::frame::{Crs, DataType, NamedCrs};
use dss_vortex::import::{
    self, Bounds, Duplicates, Failure, ImportError, ImportParts, ImportRequest, ImportedRecord,
    Selection,
};
use dss_vortex::products::window::Accumulation as A;
use dss_vortex::products::WindowRule;
use dss_vortex::warp::Method;
use dss_vortex::VortexError;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

const FIXTURE: &str = "MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240615-000000.grib2";
const V24: &str = "MultiSensor_QPE_24H_Pass2_altitude_above_msl";
const V01: &str = "MultiSensor_QPE_01H_Pass2_altitude_above_msl";
const V15: &str = "RadarOnly_QPE_15M_altitude_above_msl";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/vortex_p46")
}

fn fresh(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("import_window")
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Sections 0 and 1 are fixed in place. Section 4 is found by walking lengths.
fn section4(bytes: &[u8]) -> usize {
    let mut at = 16;
    loop {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if bytes[at + 4] == 4 {
            return at;
        }
        at += length;
    }
}

#[derive(Clone, Copy)]
struct Time(u16, u8, u8, u8, u8);

/// One message: discipline 209 and category 6 stay unless `parameter` says otherwise.
fn message(parameter: (u8, u8, u8), time: Time, seconds: u8) -> Vec<u8> {
    let mut b = fs::read(fixtures().join(FIXTURE)).unwrap();
    b[6] = parameter.0;
    if parameter.0 != 209 {
        // NCEP: its tables hold the WMO parameters (GRIB2 Code Table 4.2).
        b[16 + 5..16 + 7].copy_from_slice(&7u16.to_be_bytes());
    }
    let Time(year, month, day, hour, minute) = time;
    let s1 = 16;
    b[s1 + 12..s1 + 14].copy_from_slice(&year.to_be_bytes());
    b[s1 + 14] = month;
    b[s1 + 15] = day;
    b[s1 + 16] = hour;
    b[s1 + 17] = minute;
    b[s1 + 18] = seconds;
    let s4 = section4(&b);
    b[s4 + 9] = parameter.1;
    b[s4 + 10] = parameter.2;
    b
}

const JUNE15_06: Time = Time(2024, 6, 15, 6, 0);

fn qpe(number: u8, time: Time) -> Vec<u8> {
    message((209, 6, number), time, 0)
}

fn request(
    inputs: Vec<PathBuf>,
    output: PathBuf,
    variable: &str,
    rule: WindowRule,
) -> ImportRequest {
    let mut r = ImportRequest::new(
        inputs,
        output,
        Selection::Variable(variable.to_string()),
        Method::Near,
    );
    r.target = Some(Crs::Named(NamedCrs::Shg));
    r.cell_size = Some(2000.0);
    r.bounds = Bounds::Envelope {
        envelope: Envelope {
            min_x: 700_000.0,
            max_x: 720_000.0,
            min_y: 1_300_000.0,
            max_y: 1_320_000.0,
        },
        crs: None,
    };
    r.parts = ImportParts {
        a: "A".into(),
        b: "B".into(),
        c: None,
        f: "F".into(),
    };
    r.window_rule = rule;
    r
}

fn run(r: &ImportRequest) -> Result<Vec<ImportedRecord>, ImportError> {
    import::import_gridded(r, &mut |_| ControlFlow::Continue(()))
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn pathnames(records: &[ImportedRecord]) -> Vec<String> {
    records.iter().map(|r| r.record.pathname.clone()).collect()
}

fn read(path: &Path, pathname: &str, version: u8) -> Grid {
    if version == 6 {
        Dss6File::open(path)
            .unwrap()
            .read_grid(pathname.as_bytes())
            .unwrap()
    } else {
        Dss7File::open(path)
            .unwrap()
            .read_grid(pathname.as_bytes())
            .unwrap()
    }
}

fn refused(e: &ImportError, text: &str) {
    assert_eq!(e.phase, "planning", "{e}");
    assert!(e.completed.is_empty());
    assert!(e.in_doubt.is_none());
    match &e.failure {
        Failure::Vortex(VortexError::Refused { .. }) => {}
        other => panic!("not a refusal: {other}"),
    }
    assert!(e.to_string().contains(text), "{e}");
}

const DURATIONS: [(u8, A, &str, &str, &str); 8] = [
    (
        45,
        A::M15,
        "RadarOnly_QPE_15M_altitude_above_msl",
        "15JUN2024:0545",
        "15JUN2024:0600",
    ),
    (37, A::H01, V01, "15JUN2024:0500", "15JUN2024:0600"),
    (
        38,
        A::H03,
        "MultiSensor_QPE_03H_Pass2_altitude_above_msl",
        "15JUN2024:0300",
        "15JUN2024:0600",
    ),
    (
        39,
        A::H06,
        "MultiSensor_QPE_06H_Pass2_altitude_above_msl",
        "15JUN2024:0000",
        "15JUN2024:0600",
    ),
    (
        40,
        A::H12,
        "MultiSensor_QPE_12H_Pass2_altitude_above_msl",
        "14JUN2024:1800",
        "15JUN2024:0600",
    ),
    (41, A::H24, V24, "14JUN2024:0600", "15JUN2024:0600"),
    (
        42,
        A::H48,
        "MultiSensor_QPE_48H_Pass2_altitude_above_msl",
        "13JUN2024:0600",
        "15JUN2024:0600",
    ),
    (
        43,
        A::H72,
        "MultiSensor_QPE_72H_Pass2_altitude_above_msl",
        "12JUN2024:0600",
        "15JUN2024:0600",
    ),
];

#[test]
fn every_duration_gets_its_period_in_both_versions() {
    for version in [7u8, 6] {
        for (number, wanted, variable, d, e) in DURATIONS {
            let dir = fresh(&format!("every_{version}_{}", wanted.name()));
            let input = write(&dir, "x.grib2", &qpe(number, JUNE15_06));
            let mut r = request(
                vec![input],
                dir.join("o.dss"),
                variable,
                WindowRule::Accumulation(wanted),
            );
            r.version = Some(version);
            let records = run(&r).unwrap();
            let want = format!("/A/B/PRECIPITATION/{d}/{e}/F/");
            assert_eq!(pathnames(&records), vec![want.clone()]);
            let note = records[0].window.as_ref().unwrap();
            assert_eq!(note.seconds, wanted.seconds());
            assert_eq!(note.stated_by, "variable");
            assert!(!note.overlaps_earlier && !note.gap_before);
            assert_eq!(records[0].record.data_type, DataType::PerCum);
            let g = read(&r.output, &want, version);
            assert_eq!(g.data_units.trim(), "MM");
        }
    }
}

#[test]
fn periods_cross_midnight_months_years_and_leap_days() {
    let cases = [
        (
            45,
            Time(2024, 1, 1, 0, 0),
            "31DEC2023:2345",
            "31DEC2023:2400",
        ),
        (
            45,
            Time(2025, 1, 1, 0, 15),
            "01JAN2025:0000",
            "01JAN2025:0015",
        ),
        (
            37,
            Time(2024, 7, 1, 0, 0),
            "30JUN2024:2300",
            "30JUN2024:2400",
        ),
        (
            41,
            Time(2024, 3, 1, 0, 0),
            "29FEB2024:0000",
            "29FEB2024:2400",
        ),
        (
            41,
            Time(2023, 3, 1, 0, 0),
            "28FEB2023:0000",
            "28FEB2023:2400",
        ),
        (
            41,
            Time(2000, 3, 1, 0, 0),
            "29FEB2000:0000",
            "29FEB2000:2400",
        ),
        (
            41,
            Time(2100, 3, 1, 0, 0),
            "28FEB2100:0000",
            "28FEB2100:2400",
        ),
        (
            42,
            Time(2024, 3, 1, 0, 0),
            "28FEB2024:0000",
            "29FEB2024:2400",
        ),
        (
            43,
            Time(2024, 3, 2, 0, 0),
            "28FEB2024:0000",
            "01MAR2024:2400",
        ),
        (
            43,
            Time(2024, 12, 31, 12, 0),
            "28DEC2024:1200",
            "31DEC2024:1200",
        ),
        (
            43,
            Time(2025, 1, 1, 0, 0),
            "29DEC2024:0000",
            "31DEC2024:2400",
        ),
    ];
    for (i, (number, time, d, e)) in cases.into_iter().enumerate() {
        let (_, wanted, variable, _, _) = *DURATIONS.iter().find(|x| x.0 == number).unwrap();
        for version in [7u8, 6] {
            let dir = fresh(&format!("calendar_{i}_{version}"));
            let input = write(&dir, "x.grib2", &qpe(number, time));
            let mut r = request(
                vec![input],
                dir.join("o.dss"),
                variable,
                WindowRule::Accumulation(wanted),
            );
            r.version = Some(version);
            let records = run(&r).unwrap();
            assert_eq!(
                pathnames(&records),
                vec![format!("/A/B/PRECIPITATION/{d}/{e}/F/")],
                "case {i}"
            );
        }
    }
}

#[test]
fn a_renamed_file_is_refused_whichever_name_is_asked_for() {
    let dir = fresh("renamed");
    let bytes = qpe(41, JUNE15_06);
    let renamed = write(
        &dir,
        "MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240615-060000.grib2",
        &bytes,
    );
    let output = dir.join("o.dss");
    // The file name says one hour: it must not get one-hour stamps by asking for what it says.
    let e = run(&request(
        vec![renamed.clone()],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H01),
    ))
    .unwrap_err();
    refused(&e, "variable");
    assert!(e.to_string().contains("24H"), "{e}");
    let e = run(&request(
        vec![renamed],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    ))
    .unwrap_err();
    refused(&e, "file name");
    assert!(!output.exists());
}

#[test]
fn a_variable_or_file_that_disagrees_is_refused_for_every_other_duration() {
    let dir = fresh("disagree");
    let output = dir.join("o.dss");
    for (number, stated, variable, _, _) in DURATIONS {
        let plain = write(&dir, "plain.grib2", &qpe(number, JUNE15_06));
        for (_, asked, ..) in DURATIONS {
            let r = request(
                vec![plain.clone()],
                output.clone(),
                variable,
                WindowRule::Accumulation(asked),
            );
            let got = run(&r);
            if asked == stated {
                got.unwrap();
                fs::remove_file(&output).unwrap();
            } else {
                refused(&got.unwrap_err(), "states");
                assert!(!output.exists());
            }
        }
    }
    // The file name alone, on a variable that agrees with what is asked.
    let bad = write(&dir, "rain_QPE_06H.grib2", &qpe(41, JUNE15_06));
    let e = run(&request(
        vec![bad],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    ))
    .unwrap_err();
    refused(&e, "file name");
}

#[test]
fn one_bad_input_refuses_the_whole_run_and_leaves_an_existing_file_as_it_was() {
    let dir = fresh("one_bad");
    let good = write(&dir, "a.grib2", &qpe(41, JUNE15_06));
    let bad = write(&dir, "b_QPE_01H.grib2", &qpe(41, Time(2024, 6, 16, 6, 0)));
    let output = dir.join("o.dss");
    let first = request(
        vec![good.clone()],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    );
    run(&first).unwrap();
    let before = fs::read(&output).unwrap();
    let mut both = first;
    both.inputs = vec![good, bad];
    let e = run(&both).unwrap_err();
    refused(&e, "file name");
    assert!(e.input.unwrap().ends_with("b_QPE_01H.grib2"));
    assert_eq!(fs::read(&output).unwrap(), before);
}

#[test]
fn who_stated_it_is_recorded() {
    let dir = fresh("stated_by");
    let output = dir.join("o.dss");
    let qpe24 = qpe(41, JUNE15_06);
    let by = |name: &str, bytes: &[u8], variable: &str, wanted: A| {
        let input = write(&dir, name, bytes);
        let _ = fs::remove_file(&output);
        let records = run(&request(
            vec![input],
            output.clone(),
            variable,
            WindowRule::Accumulation(wanted),
        ))
        .unwrap();
        records[0].window.as_ref().unwrap().stated_by
    };
    assert_eq!(
        by("x_QPE_24H.grib2", &qpe24, V24, A::H24),
        "variable+file_name"
    );
    assert_eq!(by("x.grib2", &qpe24, V24, A::H24), "variable");
    // WMO Total precipitation at an instant: the only names here that state no duration.
    let total = message((0, 1, 8), JUNE15_06, 0);
    let variable = "Total_precipitation_altitude_above_msl";
    assert_eq!(by("y.grib2", &total, variable, A::H06), "caller");
    assert_eq!(by("y_QPE_06H.grib2", &total, variable, A::H06), "file_name");
}

#[test]
fn several_instants_in_one_file_get_the_stated_period_and_not_midpoints() {
    let dir = fresh("several");
    let mut joined = qpe(37, JUNE15_06);
    joined.extend(qpe(37, Time(2024, 6, 15, 7, 0)));
    joined.extend(qpe(37, Time(2024, 6, 15, 8, 0)));
    let input = write(&dir, "MRMS_MultiSensor_QPE_01H_Pass2_joined.grib2", &joined);
    let records = run(&request(
        vec![input],
        dir.join("o.dss"),
        V01,
        WindowRule::Accumulation(A::H01),
    ))
    .unwrap();
    assert_eq!(
        pathnames(&records),
        [
            "/A/B/PRECIPITATION/15JUN2024:0500/15JUN2024:0600/F/",
            "/A/B/PRECIPITATION/15JUN2024:0600/15JUN2024:0700/F/",
            "/A/B/PRECIPITATION/15JUN2024:0700/15JUN2024:0800/F/",
        ]
    );
    assert!(records
        .iter()
        .all(|r| { !r.window.as_ref().unwrap().overlaps_earlier }));
    assert!(records
        .iter()
        .all(|r| { !r.window.as_ref().unwrap().gap_before }));
}

fn marks(records: &[ImportedRecord]) -> Vec<(bool, bool)> {
    records
        .iter()
        .map(|r| {
            let n = r.window.as_ref().unwrap();
            (n.overlaps_earlier, n.gap_before)
        })
        .collect()
}

#[test]
fn rolling_periods_are_all_stored_and_marked_whatever_order_they_come_in() {
    let dir = fresh("rolling");
    let at = |hour: u8| qpe(41, Time(2024, 6, 15, hour, 0));
    // Out of time order on purpose: the marks follow the periods, not the inputs.
    let inputs = vec![
        write(&dir, "b.grib2", &at(7)),
        write(&dir, "a.grib2", &at(6)),
        write(&dir, "c.grib2", &at(8)),
    ];
    let output = dir.join("o.dss");
    let records = run(&request(
        inputs,
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    ))
    .unwrap();
    assert_eq!(
        pathnames(&records),
        [
            "/A/B/PRECIPITATION/14JUN2024:0700/15JUN2024:0700/F/",
            "/A/B/PRECIPITATION/14JUN2024:0600/15JUN2024:0600/F/",
            "/A/B/PRECIPITATION/14JUN2024:0800/15JUN2024:0800/F/",
        ]
    );
    assert_eq!(
        marks(&records),
        [(true, false), (false, false), (true, false)]
    );
    for name in pathnames(&records) {
        read(&output, &name, 7);
    }
}

#[test]
fn abutting_periods_do_not_overlap_and_a_gap_is_marked() {
    let dir = fresh("gap");
    let at = |hour: u8| qpe(37, Time(2024, 6, 15, hour, 0));
    let inputs = vec![
        write(&dir, "a.grib2", &at(6)),
        write(&dir, "b.grib2", &at(7)),
        write(&dir, "c.grib2", &at(9)),
    ];
    let records = run(&request(
        inputs,
        dir.join("o.dss"),
        V01,
        WindowRule::Accumulation(A::H01),
    ))
    .unwrap();
    assert_eq!(
        marks(&records),
        [(false, false), (false, false), (false, true)]
    );
}

#[test]
fn a_record_replaced_in_the_run_does_not_count_as_an_overlap() {
    let dir = fresh("replaced");
    let inputs = vec![
        write(&dir, "a.grib2", &qpe(41, JUNE15_06)),
        write(&dir, "b.grib2", &qpe(41, JUNE15_06)),
    ];
    let output = dir.join("o.dss");
    let mut r = request(
        inputs,
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    );
    assert!(run(&r).unwrap_err().to_string().contains("twice"));
    assert!(!output.exists());
    r.duplicates = Duplicates::Last;
    let records = run(&r).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(marks(&records), [(false, false)]);
}

#[test]
fn what_is_not_a_depth_is_refused() {
    let dir = fresh("not_depth");
    let output = dir.join("o.dss");
    // A rate would be multiplied by the window, on the caller's word alone.
    for (parameter, variable, text) in [
        ((209, 6, 1), "PrecipRate_altitude_above_msl", "converted"),
        (
            (209, 6, 0),
            "PrecipFlag_altitude_above_msl",
            "not precipitation",
        ),
    ] {
        let input = write(&dir, "x.grib2", &message(parameter, JUNE15_06, 0));
        let mut r = request(
            vec![input],
            output.clone(),
            variable,
            WindowRule::Accumulation(A::H01),
        );
        refused(&run(&r).unwrap_err(), text);
        // Stating the C part does not make it a depth.
        r.parts.c = Some("PRECIPITATION".into());
        refused(&run(&r).unwrap_err(), text);
        assert!(!output.exists());
    }
    let temperature = fixtures().join("tmp_instant.grib2");
    let e = run(&request(
        vec![temperature],
        output.clone(),
        "Temperature_height_above_ground",
        WindowRule::Accumulation(A::H01),
    ))
    .unwrap_err();
    refused(&e, "not precipitation");
    assert!(!output.exists());
}

#[test]
fn only_per_cum_can_be_stated_with_it() {
    let dir = fresh("types");
    let input = write(&dir, "x.grib2", &qpe(41, JUNE15_06));
    let output = dir.join("o.dss");
    let mut r = request(
        vec![input],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    );
    for stated in [DataType::InstVal, DataType::InstCum, DataType::PerAver] {
        r.data_type = Some(stated);
        refused(&run(&r).unwrap_err(), "PER-CUM");
        assert!(!output.exists());
    }
    r.data_type = Some(DataType::PerCum);
    let records = run(&r).unwrap();
    assert_eq!(records[0].record.data_type, DataType::PerCum);
}

#[test]
fn a_record_stating_its_own_interval_must_agree_and_then_keeps_it() {
    let dir = fresh("own");
    let apcp = fixtures().join("apcp_a.grib2");
    let variable = "Total_precipitation_surface_1_Hour_Accumulation";
    let output = dir.join("o.dss");
    let plain = dir.join("plain.grib2");
    fs::copy(&apcp, &plain).unwrap();
    let default = run(&request(
        vec![plain.clone()],
        dir.join("default.dss"),
        variable,
        WindowRule::Refuse,
    ))
    .unwrap();
    let stated = run(&request(
        vec![plain.clone()],
        output.clone(),
        variable,
        WindowRule::Accumulation(A::H01),
    ))
    .unwrap();
    assert_eq!(pathnames(&stated), pathnames(&default));
    let name = "/A/B/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/F/";
    assert_eq!(pathnames(&stated), [name]);
    let _ = fs::remove_file(&output);
    let e = run(&request(
        vec![plain],
        output.clone(),
        "Total_precipitation_surface_1_Hour_Accumulation",
        WindowRule::Accumulation(A::H06),
    ))
    .unwrap_err();
    refused(&e, "states");
    assert!(!output.exists());
}

#[test]
fn a_time_with_seconds_is_refused_and_not_rounded() {
    let dir = fresh("seconds");
    let input = write(&dir, "x.grib2", &message((209, 6, 41), JUNE15_06, 30));
    let output = dir.join("o.dss");
    let e = run(&request(
        vec![input],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    ))
    .unwrap_err();
    assert_eq!(e.phase, "planning");
    assert!(!output.exists());
}

#[test]
fn a_raster_input_is_refused_rather_than_silently_keeping_its_own_time() {
    let dir = fresh("raster");
    // Named, as a listing's order differs by platform and some fixtures here are refused on purpose.
    let raster = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/raster/2000-01-01T0000.tif");
    let list = import::open_source(&raster, &import::OpenOptions::default())
        .unwrap()
        .variables();
    let output = dir.join("o.dss");
    let mut r = request(
        vec![raster],
        output.clone(),
        &list[0],
        WindowRule::Accumulation(A::H01),
    );
    r.align = dss_vortex::warp::Align::Vortex;
    refused(&run(&r).unwrap_err(), "GRIB2");
    assert!(!output.exists());
}

#[test]
fn only_the_stamps_and_type_differ_from_the_instant_route() {
    for version in [7u8, 6] {
        for (number, wanted, variable, d, e) in DURATIONS {
            if wanted == A::H01 {
                continue;
            }
            let dir = fresh(&format!("instant_{version}_{}", wanted.name()));
            let input = write(&dir, "x.grib2", &qpe(number, JUNE15_06));
            let mut stated = request(
                vec![input.clone()],
                dir.join("stated.dss"),
                variable,
                WindowRule::Accumulation(wanted),
            );
            stated.version = Some(version);
            let mut instant = request(
                vec![input],
                dir.join("instant.dss"),
                variable,
                WindowRule::Refuse,
            );
            instant.version = Some(version);
            instant.data_type = Some(DataType::InstVal);
            run(&stated).unwrap();
            let names = run(&instant).unwrap();
            assert_eq!(
                pathnames(&names),
                ["/A/B/PRECIPITATION/15JUN2024:0600//F/"],
                "the reference route must be the instant"
            );
            let a = read(
                &stated.output,
                &format!("/A/B/PRECIPITATION/{d}/{e}/F/"),
                version,
            );
            let b = read(&instant.output, &names[0].record.pathname, version);
            let bits = |g: &Grid| g.data.iter().map(|c| c.to_bits()).collect::<Vec<u32>>();
            assert!(!a.data.is_empty());
            assert_eq!(bits(&a), bits(&b));
            assert_eq!((a.nx, a.ny), (b.nx, b.ny));
            assert_eq!(
                (
                    a.max_data_value.map(f32::to_bits),
                    a.min_data_value.map(f32::to_bits),
                    a.mean_data_value.map(f32::to_bits)
                ),
                (
                    b.max_data_value.map(f32::to_bits),
                    b.min_data_value.map(f32::to_bits),
                    b.mean_data_value.map(f32::to_bits)
                )
            );
            assert_eq!(a.data_units, b.data_units);
            assert_ne!(a.data_type, b.data_type);
        }
    }
}

#[test]
fn without_the_option_the_refusal_stands() {
    let dir = fresh("default");
    let output = dir.join("o.dss");
    for (number, wanted, variable, ..) in DURATIONS {
        if wanted == A::H01 {
            continue;
        }
        let input = write(&dir, "x.grib2", &qpe(number, JUNE15_06));
        for rule in [WindowRule::Refuse, WindowRule::Vortex] {
            let e = run(&request(
                vec![input.clone()],
                output.clone(),
                variable,
                rule,
            ))
            .unwrap_err();
            refused(&e, "precipitation at an instant");
        }
        assert!(!output.exists());
    }
    // Nor does a variable naming 15M need a special path to reach that.
    assert!(DURATIONS.iter().any(|d| d.2 == V15));
}

#[test]
fn records_written_before_a_crash_are_reported_and_a_rerun_heals() {
    let dir = fresh("crash");
    let inputs = vec![
        write(&dir, "a.grib2", &qpe(41, JUNE15_06)),
        write(&dir, "b.grib2", &qpe(41, Time(2024, 6, 16, 6, 0))),
    ];
    let output = dir.join("o.dss");
    let mut r = request(
        inputs,
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    );
    let first = "/A/B/PRECIPITATION/14JUN2024:0600/15JUN2024:0600/F/";
    let second = "/A/B/PRECIPITATION/15JUN2024:0600/16JUN2024:0600/F/";
    r.fault = Some((
        1,
        FaultPoint {
            block: 0,
            step: WriteStep::ALL[0],
        },
    ));
    let e = run(&r).unwrap_err();
    assert_eq!(e.phase, "writing");
    assert_eq!(pathnames(&e.completed), [first]);
    assert_eq!(
        e.completed[0].window.as_ref().unwrap().stated_by,
        "variable"
    );
    assert_eq!(e.in_doubt.as_deref(), Some(second));
    assert!(!read(&output, first, 7).data.is_empty());
    r.fault = None;
    let records = run(&r).unwrap();
    assert_eq!(pathnames(&records), [first, second]);
    assert!(!read(&output, second, 7).data.is_empty());
}

const ONE_HOUR_NAMES: [&str; 3] = [
    "MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240615-060000.grib2",
    "RadarOnly_QPE_01H_00.00_20240615-060000.grib2",
    "MRMS_GaugeCorr_QPE_01H_00.00_20240615-060000.grib2",
];

/// Design rule: a renamed file is refused and both durations are named.
#[test]
fn the_default_route_refuses_a_renamed_file_and_names_both_durations() {
    let dir = fresh("default_renamed");
    let output = dir.join("o.dss");
    for name in ONE_HOUR_NAMES {
        for (number, wanted, variable, ..) in DURATIONS {
            if wanted == A::H01 {
                continue;
            }
            let input = write(&dir, name, &qpe(number, JUNE15_06));
            let e = run(&request(
                vec![input.clone()],
                output.clone(),
                variable,
                WindowRule::Refuse,
            ))
            .unwrap_err();
            refused(&e, "one-hour window (01H)");
            assert!(e.to_string().contains(wanted.name()), "{e}");
            // Stating the type does not get past it: the stamps would still be one hour.
            let mut stated = request(vec![input], output.clone(), variable, WindowRule::Refuse);
            stated.data_type = Some(DataType::PerCum);
            refused(&run(&stated).unwrap_err(), "one-hour window (01H)");
            assert!(!output.exists());
        }
    }
}

#[test]
fn the_refusal_leaves_an_existing_file_as_it_was() {
    let dir = fresh("default_renamed_existing");
    let output = dir.join("o.dss");
    let good = write(&dir, ONE_HOUR_NAMES[0], &qpe(37, JUNE15_06));
    run(&request(
        vec![good],
        output.clone(),
        V01,
        WindowRule::Refuse,
    ))
    .unwrap();
    let before = fs::read(&output).unwrap();
    let bad = write(&dir, ONE_HOUR_NAMES[1], &qpe(41, JUNE15_06));
    refused(
        &run(&request(vec![bad], output.clone(), V24, WindowRule::Refuse)).unwrap_err(),
        "one-hour window (01H)",
    );
    assert_eq!(fs::read(&output).unwrap(), before);
}

#[test]
fn a_real_one_hour_file_keeps_its_window_on_the_default_route() {
    for name in ONE_HOUR_NAMES {
        let dir = fresh(&format!("default_real_{}", &name[..8]));
        let input = write(&dir, name, &qpe(37, JUNE15_06));
        let records = run(&request(
            vec![input],
            dir.join("o.dss"),
            V01,
            WindowRule::Refuse,
        ))
        .unwrap();
        assert_eq!(
            pathnames(&records),
            ["/A/B/PRECIPITATION/15JUN2024:0500/15JUN2024:0600/F/"]
        );
        assert!(records[0].window.is_none());
    }
}

/// A variable stating no duration keeps the file-name rule: nothing disagrees with it.
#[test]
fn a_variable_stating_no_duration_keeps_the_file_name_rule() {
    let dir = fresh("default_silent");
    let input = write(&dir, ONE_HOUR_NAMES[0], &message((0, 1, 8), JUNE15_06, 0));
    let records = run(&request(
        vec![input],
        dir.join("o.dss"),
        "Total_precipitation_altitude_above_msl",
        WindowRule::Refuse,
    ))
    .unwrap();
    assert_eq!(
        pathnames(&records),
        ["/A/B/PRECIPITATION/15JUN2024:0500/15JUN2024:0600/F/"]
    );
}

/// `WindowRule::Vortex` is the opt-in for Vortex's stamps, and the negative control for the refusal.
#[test]
fn window_rule_vortex_still_gives_vortexs_one_hour_stamps() {
    let dir = fresh("vortex_renamed");
    let input = write(&dir, ONE_HOUR_NAMES[0], &qpe(41, JUNE15_06));
    let records = run(&request(
        vec![input],
        dir.join("o.dss"),
        V24,
        WindowRule::Vortex,
    ))
    .unwrap();
    assert_eq!(
        pathnames(&records),
        ["/A/B/PRECIPITATION/15JUN2024:0500/15JUN2024:0600/F/"]
    );
}

// MRMS's table gives these years and a ratio; real MRMS files were checked against it.
const ARI24: (u8, u8, u8) = (209, 12, 19);
const ARI01: (u8, u8, u8) = (209, 12, 15);
const FFG01: (u8, u8, u8) = (209, 12, 26);
const ARI24_V: &str = "FLASH_QPE_ARI24H_altitude_above_msl";
const ARI01_V: &str = "FLASH_QPE_ARI01H_altitude_above_msl";
const FFG01_V: &str = "FLASH_QPE_FFG01H_altitude_above_msl";

#[test]
fn a_recurrence_interval_or_ratio_is_not_a_depth_under_a_stated_accumulation() {
    let dir = fresh("not_a_depth");
    let output = dir.join("o.dss");
    for (parameter, file, variable, wanted) in [
        (
            ARI24,
            "MRMS_FLASH_QPE_ARI24H_00.00_20240615-060000.grib2",
            ARI24_V,
            A::H24,
        ),
        (
            ARI01,
            "MRMS_FLASH_QPE_ARI01H_00.00_20240615-060000.grib2",
            ARI01_V,
            A::H01,
        ),
        (
            FFG01,
            "MRMS_FLASH_QPE_FFG01H_00.00_20240615-060000.grib2",
            FFG01_V,
            A::H01,
        ),
    ] {
        let input = write(&dir, file, &message(parameter, JUNE15_06, 0));
        let mut r = request(
            vec![input],
            output.clone(),
            variable,
            WindowRule::Accumulation(wanted),
        );
        refused(&run(&r).unwrap_err(), "not a rain depth");
        // A depth label over years changes nothing.
        r.units = Some("MM".into());
        refused(&run(&r).unwrap_err(), "not a rain depth");
        r.data_type = Some(DataType::PerCum);
        refused(&run(&r).unwrap_err(), "not a rain depth");
        assert!(!output.exists());
    }
}

#[test]
fn a_units_label_must_be_a_depth_under_a_stated_accumulation() {
    let dir = fresh("labels");
    let input = write(&dir, "x.grib2", &qpe(41, JUNE15_06));
    let output = dir.join("o.dss");
    let mut r = request(
        vec![input],
        output.clone(),
        V24,
        WindowRule::Accumulation(A::H24),
    );
    for label in [
        "CFS",
        "MM/HR",
        "mm/hr",
        "year",
        "UNDEF",
        "",
        "DEG C",
        "not a unit",
    ] {
        r.units = Some(label.into());
        refused(&run(&r).unwrap_err(), "not a depth");
        assert!(!output.exists(), "{label}");
    }
    // A depth label is a relabel, as documented: the cells stay as they are.
    for label in ["MM", "IN"] {
        let _ = fs::remove_file(&output);
        r.units = Some(label.into());
        let records = run(&r).unwrap();
        assert_eq!(records[0].record.units, label);
    }
}

// Regression: the ordinary route reached the same products.
#[test]
fn the_default_route_refuses_a_product_that_is_not_a_depth_and_vortex_keeps_it() {
    let dir = fresh("default_not_depth");
    let output = dir.join("o.dss");
    let named = "MRMS_MultiSensor_QPE_ARI01H_00.00_20240615-060000.grib2";
    let input = write(&dir, named, &message(ARI01, JUNE15_06, 0));
    let mut r = request(vec![input], output.clone(), ARI01_V, WindowRule::Refuse);
    refused(&run(&r).unwrap_err(), "not a rain depth");
    r.data_type = Some(DataType::InstVal);
    refused(&run(&r).unwrap_err(), "not a rain depth");
    assert!(!output.exists());
    // The negative control: Vortex's own record, by name, when asked for.
    r.window_rule = WindowRule::Vortex;
    r.data_type = None;
    let records = run(&r).unwrap();
    assert_eq!(
        pathnames(&records),
        ["/A/B/PRECIPITATION/15JUN2024:0500/15JUN2024:0600/F/"]
    );
    assert_eq!(records[0].record.units, "year");
}

// Regression: a label in a file name.
#[test]
fn a_label_in_a_file_name_is_not_a_statement_and_a_stem_is() {
    let dir = fresh("labels_in_names");
    let output = dir.join("o.dss");
    let run_named = |name: &str, number: u8, variable: &str, wanted: A| {
        let input = write(&dir, name, &qpe(number, JUNE15_06));
        let _ = fs::remove_file(&output);
        run(&request(
            vec![input],
            output.clone(),
            variable,
            WindowRule::Accumulation(wanted),
        ))
    };
    for (name, stated_by) in [
        ("24H_RadarOnly_QPE_01H_x.grib2", "variable+file_name"),
        ("24h_radaronly_qpe_01h_x.grib2", "variable+file_name"),
        ("QPE_24H_RadarOnly_QPE_01H.grib2", "variable+file_name"),
        ("storm_24H.grib2", "variable"),
        ("hrrr_24H.t12z.wrfsfcf01.grib2", "variable"),
        ("run_500m_06h.grib2", "variable"),
    ] {
        let records = run_named(name, 37, V01, A::H01).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            records[0].window.as_ref().unwrap().stated_by,
            stated_by,
            "{name}"
        );
    }
    // The stem still decides: a real disagreement between product name and variable.
    for (name, number, variable, wanted) in [
        ("24H_RadarOnly_QPE_01H_x.grib2", 41, V24, A::H24),
        ("01H_MultiSensor_QPE_24H_x.grib2", 37, V01, A::H01),
        ("MRMS_QPE_72H_label.grib2", 37, V01, A::H01),
        (
            "RadarOnly_QPE_01H_MultiSensor_QPE_24H.grib2",
            37,
            V01,
            A::H01,
        ),
    ] {
        let e = run_named(name, number, variable, wanted).unwrap_err();
        refused(&e, "file name");
        assert!(!output.exists(), "{name}");
    }
}

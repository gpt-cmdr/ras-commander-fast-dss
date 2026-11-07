//! NetCDF through the importer (packet P46b), with no oracle: stamps against the file's own times,
//! refusals that leave an output as it was, the stop, the limits, and an output that is an input.
//! `oracle/vortex/run_import.py --netcdf` compares the records with Vortex.
//!
//! The AORC hours hold real rain: NOAA's values and times for 19 and 20 September 2019 over the Texas
//! Panhandle, in the layout of the real `AORC_APCP_MARFC_1984010100.nc4`, which is a dry hour
//! (`fixtures/public/README.md`).

#![allow(clippy::result_large_err)] // `ImportError`: see `import.rs`.
use dss_core::dss6::Dss6File;
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_vortex::crs::Envelope;
use dss_vortex::frame::{Crs, DataType, NamedCrs};
use dss_vortex::import::{
    self, Bounds, Failure, ImportError, ImportParts, ImportRequest, ImportedRecord, Selection,
};
use dss_vortex::import_netcdf::{NetcdfOptions, StopHandle};
use dss_vortex::products::WindowRule;
use dss_vortex::source::netcdf::{FrameHeader, NetcdfFile};
use dss_vortex::warp::Method;
use dss_vortex::{GridFrame, VortexError};
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

const APCP: &str = "APCP_surface";

fn fixtures(folder: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(folder)
}

fn aorc(name: &str) -> PathBuf {
    fixtures("netcdf_p46b").join(name)
}

// Scratch under the build's own directory, never beside the fixtures.
fn fresh(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("import_netcdf")
        .join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

// A box in SHG inside the AORC hours' grid, on whole 2000 m cells.
fn request(inputs: &[PathBuf], output: &Path, variable: &str) -> ImportRequest {
    let mut r = ImportRequest::new(
        inputs.to_vec(),
        output.to_path_buf(),
        Selection::Variable(variable.into()),
        Method::Bilinear,
    );
    r.target = Some(Crs::Named(NamedCrs::Shg));
    r.cell_size = Some(2000.0);
    r.bounds = Bounds::Envelope {
        envelope: Envelope {
            min_x: -500_000.0,
            max_x: -400_000.0,
            min_y: 1_300_000.0,
            max_y: 1_380_000.0,
        },
        crs: None,
    };
    r.parts = ImportParts {
        a: "SHG".into(),
        b: "P46B".into(),
        c: None,
        f: "RUN".into(),
    };
    r
}

fn run(r: &ImportRequest) -> Result<Vec<ImportedRecord>, ImportError> {
    import::import_gridded(r, &mut |_| ControlFlow::Continue(()))
}

fn read_back(path: &Path, version: u8) -> BTreeMap<String, Grid> {
    let mut out = BTreeMap::new();
    if version == 6 {
        let file = Dss6File::open(path).unwrap();
        for e in &file.catalog().unwrap().entries {
            let g = file.read_grid(&e.pathname).unwrap();
            out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
        }
    } else {
        let mut file = Dss7File::open(path).unwrap();
        let catalog = file.catalog().unwrap();
        for e in &catalog.catalog.entries {
            let g = file.read_grid(&e.pathname).unwrap();
            out.insert(String::from_utf8_lossy(&e.pathname).into_owned(), g);
        }
    }
    out
}

// The D and E parts of the hour ending at a time, in seconds since 1970, by this test's own
// arithmetic: nothing here calls the engine's time code.
fn hour_ending(seconds: i64) -> (String, String) {
    fn civil(days: i64) -> (i64, i64, i64) {
        // Days since 1970-01-01 to a date (Howard Hinnant's algorithm, public domain).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + i64::from(m <= 2), m, d)
    }
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let part = |t: i64, end: bool| {
        let (mut days, mut of_day) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        if end && of_day == 0 {
            days -= 1;
            of_day = 86_400;
        }
        let (y, m, d) = civil(days);
        format!(
            "{d:02}{}{y:04}:{:02}{:02}",
            MONTHS[(m - 1) as usize],
            of_day / 3600,
            of_day % 3600 / 60
        )
    };
    (part(seconds - 3600, false), part(seconds, true))
}

// A file's own times, through the reader's step and not its frame: milliseconds since 1970.
fn file_times(path: &Path) -> Vec<i64> {
    let file = NetcdfFile::open(path).unwrap();
    let v = file.variable(APCP).unwrap();
    (0..v.step_count())
        .map(|i| v.frame_header(i).unwrap().file_window.unwrap().1 / 1000)
        .collect()
}

// Why a run's pathnames are not the hours ending at these times, each once.
fn stamps_disagree(times: &[i64], pathnames: &[String]) -> Option<String> {
    let mut want: Vec<(String, String)> = times.iter().map(|t| hour_ending(*t)).collect();
    let mut have: Vec<(String, String)> = pathnames
        .iter()
        .map(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            (
                parts.get(4).copied().unwrap_or("").to_string(),
                parts.get(5).copied().unwrap_or("").to_string(),
            )
        })
        .collect();
    want.sort();
    have.sort();
    if times.is_empty() {
        return Some("no times to compare".into());
    }
    (want != have).then(|| format!("file {want:?}, records {have:?}"))
}

#[test]
fn the_stamp_check_fails_on_each_way_a_stamp_can_be_wrong() {
    let midnight = 1_568_937_600; // 2019-09-20 00:00
    assert_eq!(
        hour_ending(midnight),
        ("19SEP2019:2300".to_string(), "19SEP2019:2400".to_string())
    );
    assert_eq!(
        hour_ending(midnight + 3600),
        ("20SEP2019:0000".to_string(), "20SEP2019:0100".to_string())
    );
    let good = "/SHG/P46B/PRECIPITATION/19SEP2019:2300/19SEP2019:2400/RUN/".to_string();
    assert_eq!(
        stamps_disagree(&[midnight], std::slice::from_ref(&good)),
        None
    );
    for wrong in [
        // The hour starting at the file's time: off by one step.
        "/SHG/P46B/PRECIPITATION/20SEP2019:0000/20SEP2019:0100/RUN/",
        // Midnight written as 0000 of the next day.
        "/SHG/P46B/PRECIPITATION/19SEP2019:2300/20SEP2019:0000/RUN/",
        // No period at all: the stamp that gave HEC-HMS no rain.
        "/SHG/P46B/PRECIPITATION/19SEP2019:2400//RUN/",
        "/SHG/P46B/PRECIPITATION/19SEP2019:2400/19SEP2019:2400/RUN/",
    ] {
        assert!(
            stamps_disagree(&[midnight], &[wrong.to_string()]).is_some(),
            "{wrong}"
        );
    }
    // Missing, extra, duplicate and empty.
    assert!(stamps_disagree(&[midnight, midnight + 3600], std::slice::from_ref(&good)).is_some());
    assert!(stamps_disagree(&[midnight], &[good.clone(), good.clone()]).is_some());
    assert!(stamps_disagree(&[midnight], &[]).is_some());
    assert!(stamps_disagree(&[], &[]).is_some());
}

#[test]
fn aorc_hours_are_stamped_by_their_end_across_midnight_in_dss7_and_dss6() {
    let inputs: Vec<PathBuf> = ["2019091923", "2019092000", "2019092001"]
        .iter()
        .map(|t| aorc(&format!("AORC_APCP_TXPAN_{t}.nc4")))
        .collect();
    let times: Vec<i64> = inputs.iter().flat_map(|p| file_times(p)).collect();
    assert_eq!(times, vec![1_568_934_000, 1_568_937_600, 1_568_941_200]);
    let mut cells_by_version = Vec::new();
    for version in [7u8, 6] {
        let out = fresh(&format!("hours{version}")).join("out.dss");
        let mut r = request(&inputs, &out, APCP);
        r.version = Some(version);
        let written = run(&r).unwrap();
        let names: Vec<String> = written.iter().map(|w| w.record.pathname.clone()).collect();
        assert_eq!(stamps_disagree(&times, &names), None);
        assert_eq!(
            names,
            vec![
                "/SHG/P46B/PRECIPITATION/19SEP2019:2200/19SEP2019:2300/RUN/",
                "/SHG/P46B/PRECIPITATION/19SEP2019:2300/19SEP2019:2400/RUN/",
                "/SHG/P46B/PRECIPITATION/20SEP2019:0000/20SEP2019:0100/RUN/",
            ]
        );
        let grids = read_back(&out, version);
        assert_eq!(grids.len(), 3, "the file holds what the run reports");
        let mut all = Vec::new();
        for (w, time) in written.iter().zip(&times) {
            assert_eq!(w.record.data_type, DataType::PerCum);
            assert_eq!(w.record.units, "MM");
            let g = &grids[&w.record.pathname];
            assert_eq!(g.data_units, "MM");
            assert_eq!(g.data_type, 1, "PER-CUM");
            assert_eq!((g.nx, g.ny), (50, 40));
            // The header's own times, in HEC seconds: an hour, ending at the file's time.
            let end = time + 25_568 * 86_400;
            assert_eq!((g.start_time, g.end_time), (Some(end - 3600), Some(end)));
            // Rain, so a comparison of these cells can fail on a value.
            let wet = g.data.iter().filter(|c| **c > 0.0 && **c < 1e30).count();
            assert!(wet > 300, "{wet} wet cells in {}", w.record.pathname);
            all.push(g.data.iter().map(|c| c.to_bits()).collect::<Vec<u32>>());
        }
        assert_ne!(all[0], all[1], "each hour holds its own cells");
        assert_ne!(all[1], all[2]);
        cells_by_version.push(all);
    }
    assert_eq!(
        cells_by_version[0], cells_by_version[1],
        "DSS6 keeps the bits DSS7 does"
    );
}

#[test]
fn hours_in_one_file_get_one_record_each_and_other_spacing_is_refused() {
    let six = aorc("AORC_APCP_TXPAN_6H_2019091921.nc4");
    let out = fresh("six").join("out.dss");
    let written = run(&request(std::slice::from_ref(&six), &out, APCP)).unwrap();
    let names: Vec<String> = written.iter().map(|w| w.record.pathname.clone()).collect();
    assert_eq!(names.len(), 6);
    assert_eq!(stamps_disagree(&file_times(&six), &names), None);
    assert!(names.contains(&"/SHG/P46B/PRECIPITATION/19SEP2019:2300/19SEP2019:2400/RUN/".into()));

    // Steps three hours apart under a name that says AORC: the name's hour is not the file's.
    let three = aorc("AORC_APCP_TXPAN_3HOURLY.nc4");
    let out = fresh("three").join("out.dss");
    let e = run(&request(std::slice::from_ref(&three), &out, APCP)).unwrap_err();
    assert!(matches!(
        &e.failure,
        Failure::Vortex(VortexError::Refused { .. })
    ));
    assert!(e.to_string().contains("not one hour apart"), "{e}");
    assert_eq!(
        (e.phase, e.completed.len(), &e.in_doubt),
        ("planning", 0, &None)
    );
    assert!(!out.exists());
    // Vortex's stamps, when asked for: an hour ending at each time.
    let mut r = request(std::slice::from_ref(&three), &out, APCP);
    r.window_rule = WindowRule::Vortex;
    let names: Vec<String> = run(&r)
        .unwrap()
        .into_iter()
        .map(|w| w.record.pathname)
        .collect();
    assert_eq!(stamps_disagree(&file_times(&three), &names), None);
    assert_eq!(names.len(), 3);
}

#[test]
fn without_its_name_an_aorc_hour_is_an_instant_and_is_refused_as_precipitation() {
    let d = fresh("renamed");
    let renamed = d.join("rain.nc4");
    fs::copy(aorc("AORC_APCP_TXPAN_2019092000.nc4"), &renamed).unwrap();
    let out = d.join("out.dss");
    let e = run(&request(std::slice::from_ref(&renamed), &out, APCP)).unwrap_err();
    assert!(e.to_string().contains("precipitation at an instant"), "{e}");
    assert!(!out.exists());
    // Stated, it is Vortex's record: the end only.
    let mut r = request(&[renamed], &out, APCP);
    r.data_type = Some(DataType::InstVal);
    let written = run(&r).unwrap();
    assert_eq!(
        written[0].record.pathname,
        "/SHG/P46B/PRECIPITATION/19SEP2019:2400//RUN/"
    );
}

#[test]
fn precipitation_that_is_not_a_depth_is_refused_and_vortex_s_record_is_an_opt_in() {
    let whole = |path: PathBuf, out: &Path| {
        let mut r = request(&[path], out, "precipitation");
        r.target = None;
        r.cell_size = None;
        r.bounds = Bounds::None;
        r.align = dss_vortex::warp::Align::Vortex;
        r
    };
    let out = fresh("percent").join("out.dss");
    let percent = aorc("precip_percent.nc4");
    let e = run(&whole(percent.clone(), &out)).unwrap_err();
    assert!(e.to_string().contains("not a rain depth"), "{e}");
    assert!(!out.exists());
    // A label does not make a percentage a depth.
    let mut r = whole(percent.clone(), &out);
    r.units = Some("MM".into());
    assert!(run(&r)
        .unwrap_err()
        .to_string()
        .contains("not a rain depth"));
    assert!(!out.exists());
    let mut r = whole(percent, &out);
    r.window_rule = WindowRule::Vortex;
    let written = run(&r).unwrap();
    assert_eq!(
        written[0].record.pathname,
        "/SHG/P46B/PRECIPITATION/01JAN2000:0500/01JAN2000:0600/RUN/"
    );
    // Percent has no DSS string in Vortex's table, so its record says UNDEF.
    assert_eq!(written[0].record.units, "UNDEF");

    // No units at all: only the caller can say it is a depth.
    let out = fresh("nounits").join("out.dss");
    let bare = aorc("precip_nounits.nc4");
    assert!(run(&whole(bare.clone(), &out)).is_err());
    assert!(!out.exists());
    let mut r = whole(bare, &out);
    r.units = Some("MM".into());
    let written = run(&r).unwrap();
    assert_eq!(written[0].record.units, "MM");
    assert_eq!(written[0].record.data_type, DataType::PerCum);
}

// Every refusal the source or the importer makes leaves an existing output byte for byte.
#[test]
fn a_refusal_leaves_an_existing_output_untouched() {
    let d = fresh("untouched");
    let out = d.join("out.dss");
    run(&request(
        &[aorc("AORC_APCP_TXPAN_2019092000.nc4")],
        &out,
        APCP,
    ))
    .unwrap();
    let before = fs::read(&out).unwrap();
    let nc = fixtures("netcdf");
    let cases: [(PathBuf, &str, &str); 9] = [
        (
            aorc("AORC_APCP_TXPAN_3HOURLY.nc4"),
            APCP,
            "not one hour apart",
        ),
        (
            aorc("precip_percent.nc4"),
            "precipitation",
            "not a rain depth",
        ),
        (nc.join("two_d_axes.nc"), "data", "two-dimensional"),
        (nc.join("uneven_axis.nc"), "data", "not evenly spaced"),
        (nc.join("levels.nc"), "data", "several vertical levels"),
        (nc.join("lambert_false_origin.nc"), "data", "false_easting"),
        (nc.join("seconds_float.nc"), "data", "fraction of a second"),
        (
            nc.join("instants.nc"),
            "rain",
            "does not fit the frame's time",
        ),
        (nc.join("07.nc"), "SWE", "file-name rule UA_SWE"),
    ];
    let mut reasons = Vec::new();
    for (path, variable, text) in &cases {
        let mut r = request(std::slice::from_ref(path), &out, variable);
        r.bounds = Bounds::None;
        r.align = dss_vortex::warp::Align::Vortex;
        let e = run(&r).unwrap_err();
        let typed = matches!(
            &e.failure,
            Failure::Vortex(VortexError::Refused { .. } | VortexError::Frame(_))
        );
        assert!(typed, "{}: {e}", path.display());
        assert!(e.to_string().contains(text), "{}: {e}", path.display());
        assert_eq!(
            (e.phase, e.completed.len(), &e.in_doubt),
            ("planning", 0, &None)
        );
        assert_eq!(fs::read(&out).unwrap(), before, "{}", path.display());
        reasons.push(e.to_string());
    }
    // Each refusal is its own: one text for all would pass the checks above on any of them.
    reasons.sort();
    reasons.dedup();
    assert_eq!(reasons.len(), cases.len());
    // And a variable the file does not hold.
    let e = run(&request(
        &[aorc("AORC_APCP_TXPAN_2019092000.nc4")],
        &out,
        "TMP",
    ))
    .unwrap_err();
    assert!(e.to_string().contains("not in this input"), "{e}");
    assert_eq!(fs::read(&out).unwrap(), before);
}

#[test]
fn an_output_that_is_an_input_is_refused_by_any_name() {
    let d = fresh("alias");
    let input = d.join("AORC_APCP_TXPAN_2019092000.nc4");
    fs::copy(aorc("AORC_APCP_TXPAN_2019092000.nc4"), &input).unwrap();
    let before = fs::read(&input).unwrap();
    let link = d.join("out.dss");
    fs::hard_link(&input, &link).unwrap();
    for output in [&link, &input] {
        let e = run(&request(std::slice::from_ref(&input), output, APCP)).unwrap_err();
        assert!(e.to_string().contains("it is the input"), "{e}");
        assert_eq!(
            (e.phase, e.completed.len(), &e.in_doubt),
            ("planning", 0, &None)
        );
        assert_eq!(fs::read(&input).unwrap(), before);
    }
    // Another file with the same bytes is not the input, and is refused for what it is.
    let copy = d.join("copy.dss");
    fs::copy(&input, &copy).unwrap();
    let e = run(&request(std::slice::from_ref(&input), &copy, APCP)).unwrap_err();
    assert!(e.to_string().contains("not a DSS file"), "{e}");
    assert_eq!(fs::read(&copy).unwrap(), before);
    // With the link gone, the same request writes.
    fs::remove_file(&link).unwrap();
    assert_eq!(run(&request(&[input], &link, APCP)).unwrap().len(), 1);
}

#[test]
fn a_stop_ends_the_run_and_says_what_was_written() {
    let six = aorc("AORC_APCP_TXPAN_6H_2019091921.nc4");
    // Asked before the run: nothing is read or created.
    let out = fresh("stop_before").join("out.dss");
    let stop = StopHandle::new();
    stop.request();
    let mut r = request(std::slice::from_ref(&six), &out, APCP);
    r.stop = Some(stop.flag());
    let e = run(&r).unwrap_err();
    assert!(matches!(e.failure, Failure::Stopped), "{e}");
    assert_eq!(
        (e.phase, e.completed.len(), &e.in_doubt),
        ("planning", 0, &None)
    );
    assert!(!out.exists());
    // Withdrawn, the same handle lets a run through.
    stop.clear();
    assert_eq!(run(&r).unwrap().len(), 6);

    // Asked after the second record: two remain, and none is in doubt.
    let out = fresh("stop_during").join("out.dss");
    let stop = StopHandle::new();
    let mut r = request(&[six], &out, APCP);
    r.stop = Some(stop.flag());
    let e = import::import_gridded(&r, &mut |p| {
        if p.done == 2 {
            stop.request();
        }
        ControlFlow::Continue(())
    })
    .unwrap_err();
    assert!(matches!(e.failure, Failure::Stopped), "{e}");
    assert_eq!(
        (e.phase, e.completed.len(), &e.in_doubt),
        ("writing", 2, &None)
    );
    let grids = read_back(&out, 7);
    let completed: Vec<&String> = e.completed.iter().map(|c| &c.record.pathname).collect();
    assert_eq!(grids.keys().collect::<Vec<_>>(), {
        let mut c = completed.clone();
        c.sort();
        c
    });
}

#[test]
fn the_reader_s_limits_reach_it_through_the_request() {
    let hour = aorc("AORC_APCP_TXPAN_2019092000.nc4");
    let out = fresh("limits").join("out.dss");
    // One read may decode one byte: the frame is refused, naming the setting, and nothing is created.
    let mut r = request(std::slice::from_ref(&hour), &out, APCP);
    r.netcdf = NetcdfOptions {
        max_decoded_bytes_per_read: Some(1),
        ..NetcdfOptions::default()
    };
    let e = run(&r).unwrap_err();
    assert!(e.to_string().contains("max_decoded_bytes_per_read"), "{e}");
    assert!(
        matches!(&e.failure, Failure::Vortex(VortexError::Refused { .. })),
        "{e}"
    );
    assert_eq!((e.completed.len(), &e.in_doubt), (0, &None));
    assert!(
        !out.exists(),
        "a first frame that cannot be read creates no file"
    );
    // The importer's cell limit is the reader's.
    let mut r = request(std::slice::from_ref(&hour), &out, APCP);
    r.limits.max_source_cells = 100;
    let e = run(&r).unwrap_err();
    assert!(e.to_string().contains("over the limit of 100 cells"), "{e}");
    assert!(!out.exists());
    // The settings map one to one, and unset ones keep the reader's defaults.
    let source = import::SourceLimits::default();
    let d = dss_vortex::source::netcdf::Limits::default();
    let set = NetcdfOptions {
        max_seconds_per_read: Some(7),
        max_decoded_bytes_per_read: Some(11),
        chunk_cache_bytes: Some(13),
        max_reader_bytes: Some(17),
    }
    .limits(&source);
    assert_eq!(
        (
            set.max_seconds_per_read,
            set.max_decoded_bytes_per_read,
            set.chunk_cache_bytes,
            set.max_reader_bytes
        ),
        (Some(7), 11, 13, 17)
    );
    let unset = NetcdfOptions::default().limits(&source);
    assert_eq!(unset.max_decoded_bytes_per_read, 32 << 30);
    assert_eq!(
        unset.max_decoded_bytes_per_read,
        d.max_decoded_bytes_per_read
    );
    assert_eq!(
        (unset.max_seconds_per_read, unset.chunk_cache_bytes),
        (None, d.chunk_cache_bytes)
    );
    assert_eq!(unset.max_cells, source.max_cells);
    // A generous time limit lets the read through.
    let mut r = request(&[hour], &out, APCP);
    r.netcdf.max_seconds_per_read = Some(600);
    assert_eq!(run(&r).unwrap().len(), 1);
}

#[test]
fn netcdf_is_known_by_its_first_bytes_before_any_text_inside_it() {
    // The real AORC file names a global attribute `GRIB2_grid_template`, inside the bytes the GRIB2
    // check scans, so it reads as NetCDF only because its own magic is looked for first.
    let real = fixtures("netcdf").join("AORC_APCP_MARFC_1984010100.nc4");
    let head = &fs::read(&real).unwrap()[..4096];
    assert!(head.windows(4).any(|w| w == b"GRIB"));
    assert_eq!(
        import::list_variables(&real).unwrap(),
        vec![APCP.to_string()]
    );
    // Classic, and renamed: the name does not choose the reader.
    let d = fresh("magic");
    let renamed = d.join("classic.grib2");
    fs::copy(fixtures("netcdf").join("nb731_float32.nc"), &renamed).unwrap();
    assert_eq!(
        import::list_variables(&renamed).unwrap(),
        vec!["precip_incremental".to_string()]
    );
    let junk = d.join("junk.nc");
    fs::write(&junk, b"CDF\x03 not a version this reader knows").unwrap();
    assert!(matches!(
        import::list_variables(&junk),
        Err(VortexError::Unsupported { .. })
    ));
}

// What a header states against its frame, if they differ.
fn header_disagrees(h: &FrameHeader, frame: &GridFrame) -> Option<&'static str> {
    let p = frame.parts();
    if (h.nx, h.ny) != (p.nx, p.ny) {
        return Some("shape");
    }
    if h.row_order != p.row_order || h.transform != p.transform {
        return Some("transform");
    }
    if h.crs != p.crs {
        return Some("crs");
    }
    let same_marker = match (h.nodata, p.nodata) {
        (Some(a), Some(b)) => a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()),
        (None, None) => true,
        _ => false,
    };
    if !same_marker {
        return Some("nodata");
    }
    if h.time != p.time {
        return Some("time");
    }
    if h.data_type != p.data_type {
        return Some("data type");
    }
    if h.units != p.units {
        return Some("units");
    }
    if h.variable != p.variable || h.description != p.description {
        return Some("names");
    }
    None
}

#[test]
fn a_header_states_what_its_frame_does_on_every_step_of_every_fixture() {
    let (mut frames, mut refused, mut variables, mut files) = (0, 0, 0, 0);
    for folder in ["netcdf", "netcdf_p46b"] {
        let mut paths: Vec<PathBuf> = fs::read_dir(fixtures(folder))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e != "json" && e != "x"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(file) = NetcdfFile::open(&path) else {
                continue;
            };
            files += 1;
            for name in file.variable_names() {
                let Ok(variable) = file.variable(&name) else {
                    continue;
                };
                variables += 1;
                for step in 0..variable.step_count() {
                    let at = format!("{} {name} step {step}", path.display());
                    match (variable.frame_header(step), variable.frame(step)) {
                        (Ok(h), Ok(f)) => {
                            assert_eq!(header_disagrees(&h, &f), None, "{at}");
                            frames += 1;
                        }
                        (Err(h), Err(f)) => {
                            assert_eq!(h.to_string(), f.to_string(), "{at}");
                            refused += 1;
                        }
                        (h, f) => panic!("{at}: header {:?}, frame {:?}", h.err(), f.err()),
                    }
                }
                assert!(variable.frame_header(variable.step_count()).is_err());
            }
        }
    }
    // Counted on this run and pinned, so a file or step dropped from the loop fails. The 9 refused
    // steps are the ones P48's report lists for its fixtures.
    assert_eq!((files, variables, frames, refused), (64, 89, 150, 9));

    // The comparison can fail on each field.
    let file = NetcdfFile::open(&aorc("AORC_APCP_TXPAN_2019092000.nc4")).unwrap();
    let v = file.variable(APCP).unwrap();
    let (h, f) = (v.frame_header(0).unwrap(), v.frame(0).unwrap());
    let changed: [(&str, FrameHeader); 6] = [
        (
            "shape",
            FrameHeader {
                nx: h.nx + 1,
                ..h.clone()
            },
        ),
        (
            "crs",
            FrameHeader {
                crs: Crs::Named(NamedCrs::Shg),
                ..h.clone()
            },
        ),
        (
            "nodata",
            FrameHeader {
                nodata: None,
                ..h.clone()
            },
        ),
        (
            "time",
            FrameHeader {
                time: dss_vortex::FrameTime::Invariant,
                ..h.clone()
            },
        ),
        (
            "units",
            FrameHeader {
                units: "in".into(),
                ..h.clone()
            },
        ),
        (
            "names",
            FrameHeader {
                variable: "other".into(),
                ..h.clone()
            },
        ),
    ];
    for (field, header) in &changed {
        assert_eq!(header_disagrees(header, &f), Some(*field));
    }
    // A frame's day is HEC's: 20 September 2019 is julian day 43727, not a count from 1970.
    assert_eq!(
        h.time,
        dss_vortex::FrameTime::Period {
            start: (43_726, 82_800),
            end: (43_727, 0)
        }
    );
    assert_eq!(dss_core::time::julian_to_ymd(43_727), (2019, 9, 20));
}

// The name selects AORC's convention and nothing more: its digits are not read, and the file's own
// times give the stamps.
#[test]
fn digits_in_an_aorc_name_are_not_read_and_one_step_has_no_spacing_to_check() {
    let d = fresh("digits");
    let source = aorc("AORC_APCP_TXPAN_2019092000.nc4");
    let own = file_times(&source);
    assert_eq!(own, vec![1_568_937_600]);
    for name in [
        // Another year and hour, and no date at all.
        "AORC_APCP_TXPAN_2025010107.nc4",
        "AORC_APCP_NOHOUR.nc4",
    ] {
        let renamed = d.join(name);
        fs::copy(&source, &renamed).unwrap();
        let out = d.join(format!("{name}.dss"));
        let names: Vec<String> = run(&request(std::slice::from_ref(&renamed), &out, APCP))
            .unwrap()
            .into_iter()
            .map(|w| w.record.pathname)
            .collect();
        assert_eq!(stamps_disagree(&own, &names), None, "{name}");
        assert_eq!(
            names,
            vec!["/SHG/P46B/PRECIPITATION/19SEP2019:2300/19SEP2019:2400/RUN/"]
        );
    }
    // One step: nothing in the file states the hour, so the spacing condition tests nothing, and the
    // rule is taken on the name and the variable's name. The same step among others three hours apart
    // is refused, which is all the spacing condition can do.
    let e = run(&request(
        &[aorc("AORC_APCP_TXPAN_3HOURLY.nc4")],
        &d.join("three.dss"),
        APCP,
    ))
    .unwrap_err();
    assert!(e.to_string().contains("not one hour apart"), "{e}");
    assert!(
        e.to_string().contains("the file's name selects"),
        "the refusal says what the name does: {e}"
    );
}

// Unit strings of real NetCDF products that Vortex 1.1.0 stores as UNDEF, with no conversion: read
// from its records by `oracle/vortex/nc_refused_on_vortex.py`, where this importer had refused them.
#[test]
fn unit_strings_vortex_stores_as_undef_are_read_as_it_reads_them() {
    use dss_vortex::units::{parse_unit, Unit};
    for text in [
        "count",           // IMERG daily counts
        "minutes",         // IMERG observation time
        "(0 - 1)",         // ERA-40 cloud cover
        "m s**-1",         // ERA-40 wind
        "kg m**-2",        // ERA-40 column water and ozone
        "m of water",      // ERA-40 evaporation
        "millimeters/day", // GPCP
    ] {
        assert_eq!(parse_unit(Some(text)), Some(Unit::One), "{text}");
        assert_eq!(Unit::One.dss_string(), "UNDEF");
    }
    // The parser is case-sensitive and exact, as Vortex's is, and a string it was not asked about
    // stays unread.
    assert_eq!(parse_unit(Some("Count")), None);
    assert_eq!(parse_unit(Some("m s**-2")), None);
    // None of these is a depth, so a variable named precipitation in one is still refused as one.
    let out = fresh("undef_units").join("out.dss");
    let mut r = request(&[aorc("precip_percent.nc4")], &out, "precipitation");
    r.bounds = Bounds::None;
    r.align = dss_vortex::warp::Align::Vortex;
    assert!(run(&r)
        .unwrap_err()
        .to_string()
        .contains("not a rain depth"));
}

// P49's `bbox` and `times` select from a Zarr store. A NetCDF file does not take them, and says so:
// it never reads the whole file as though they had not been stated.
#[test]
fn a_box_or_a_time_range_on_a_netcdf_file_is_refused_and_never_ignored() {
    let d = fresh("selection");
    let out = d.join("out.dss");
    let six = aorc("AORC_APCP_TXPAN_6H_2019091921.nc4");
    let stated = [
        (
            "box",
            Some([-102.0, 35.0, -101.0, 35.5]),
            None::<(i64, i64)>,
        ),
        (
            "times",
            None::<[f64; 4]>,
            Some((1_568_934_000, 1_568_937_600)),
        ),
        (
            "both",
            Some([-102.0, 35.0, -101.0, 35.5]),
            Some((1_568_934_000, 1_568_937_600)),
        ),
    ];
    for (what, bbox, times) in stated {
        let mut r = request(std::slice::from_ref(&six), &out, APCP);
        r.source_bbox = bbox;
        r.source_times = times;
        let e = run(&r).unwrap_err();
        assert!(
            matches!(&e.failure, Failure::Vortex(VortexError::Refused { .. })),
            "{what}: {e}"
        );
        assert!(
            e.to_string()
                .contains("not a source that takes bbox or times"),
            "{what}: {e}"
        );
        assert_eq!(
            (e.phase, e.completed.len(), &e.in_doubt),
            ("planning", 0, &None)
        );
        assert!(!out.exists(), "{what}");
    }
    // Unstated, the same request writes all six hours: the refusal is of the arguments alone.
    assert_eq!(
        run(&request(std::slice::from_ref(&six), &out, APCP))
            .unwrap()
            .len(),
        6
    );
    let before = fs::read(&out).unwrap();
    let mut r = request(std::slice::from_ref(&six), &out, APCP);
    r.source_times = Some((1_568_934_000, 1_568_937_600));
    assert!(run(&r).is_err());
    assert_eq!(fs::read(&out).unwrap(), before);
}

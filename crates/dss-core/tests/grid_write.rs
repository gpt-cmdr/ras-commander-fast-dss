//! Packet P24: grid writer (DSS7 `write_grid`, DSS6 `write_grid6`).
//!
//! - Round trips for HRAP, Albers (SHG and other parameters) and specified grids,
//!   time-stamped and time-invariant, ZLIB / uncompressed / PRECIP_2_BYTE, with the
//!   reader (cells, geometry, CRS, statistics), equal rewrites (`unchanged`) and changed
//!   rewrites (`replaced`) leaving every other record word-identical.
//! - HEC-written fixture (`fixtures/public/p24_hec_grid{6,7}.dss`, HEC Monolith 3.3.27):
//!   the engine reads it as Monolith does, and a native replay of its operations gives
//!   the same records (deflate streams compared inflated; see the P24 report).
//! - Refusals leave the file byte-identical; a crash after every step of the
//!   transaction leaves the other records intact and the next write repairs the file;
//!   the HEC lock is honoured; a fuzz pass gives only typed errors.

mod dss7_common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss7_common::Rng;
use dss_core::dss6::write::{check_file6, create_dss6, write_grid6, DSS6_STEP_ORDER};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, validate_grid_input, write_grid, write_rts, AlbersParameters, BlockOutcome,
    FaultPoint, GridCompression, GridInput, GridProjection, RtsInput, RtsValues,
    SpecifiedReference, WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::Dss7File;
use dss_core::grid_write::{grid_statistics, GRID_NODATA, UNDEFINED_STORAGE};
use dss_core::{Grid, GridType, GRID_UNDEFINED};

const UTM17N: &str = "PROJCS[\"NAD_1983_UTM_Zone_17N\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"False_Easting\",500000.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-81.0],PARAMETER[\"Scale_Factor\",0.9996],PARAMETER[\"Latitude_Of_Origin\",0.0],UNIT[\"Meter\",1.0]]";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("grid_write_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: false,
        ..WriteOptions::default()
    }
}

fn new_file(dir: &Path, v: u8) -> PathBuf {
    let p = dir.join(format!("g{v}.dss"));
    let _ = std::fs::remove_file(&p);
    if v == 6 {
        create_dss6(&p).unwrap();
    } else {
        create_dss7(&p).unwrap();
    }
    p
}

fn write(
    path: &Path,
    v: u8,
    input: &GridInput,
    o: &WriteOptions,
) -> Result<BlockOutcome, WriteError> {
    let r = if v == 6 {
        write_grid6(path, input, o)?
    } else {
        write_grid(path, input, o)?
    };
    Ok(r.blocks[0].outcome)
}

fn read(path: &Path, v: u8, p: &[u8]) -> Grid {
    if v == 6 {
        Dss6File::open(path).unwrap().read_grid(p).unwrap()
    } else {
        Dss7File::open(path).unwrap().read_grid(p).unwrap()
    }
}

/// Deterministic precipitation-like cells: zeros, depths, a few -FLT_MAX.
fn precip(n: usize, seed: u64) -> Vec<f32> {
    let mut r = Rng(seed);
    (0..n)
        .map(|_| match r.below(10) {
            0 => GRID_NODATA,
            1..=4 => 0.0,
            _ => r.below(800) as f32 / 100.0,
        })
        .collect()
}

fn shg<'a>(path: &'a [u8], data: &'a [f32], nx: usize, ny: usize) -> GridInput<'a> {
    GridInput {
        pathname: path,
        projection: GridProjection::Albers(AlbersParameters::shg()),
        time_stamped: true,
        data,
        nx,
        ny,
        lower_left_cell_x: -1000,
        lower_left_cell_y: 700,
        cell_size: 2000.0,
        x_coord_of_grid_cell_zero: 0.0,
        y_coord_of_grid_cell_zero: 0.0,
        units: b"MM",
        data_type: 1,
        compression: GridCompression::Zlib,
    }
}

fn specified<'a>(path: &'a [u8], data: &'a [f32], nx: usize, ny: usize) -> GridInput<'a> {
    GridInput {
        projection: GridProjection::Specified(SpecifiedReference {
            srs_name: b"UTM17N".to_vec(),
            srs_definition: UTM17N.as_bytes().to_vec(),
            srs_definition_type: 0,
            null_value: -9999.0,
            time_zone_id: b"UTC".to_vec(),
            time_zone_raw_offset: 0,
            is_interval: false,
        }),
        lower_left_cell_x: 250,
        lower_left_cell_y: 4400,
        cell_size: 1000.0,
        x_coord_of_grid_cell_zero: 500.0,
        y_coord_of_grid_cell_zero: -250.0,
        units: b"DEG C",
        data_type: 2,
        ..shg(path, data, nx, ny)
    }
}

fn hrap<'a>(path: &'a [u8], data: &'a [f32], nx: usize, ny: usize) -> GridInput<'a> {
    GridInput {
        projection: GridProjection::Hrap {
            data_source: b"STAGE3".to_vec(),
        },
        lower_left_cell_x: 350,
        lower_left_cell_y: 120,
        cell_size: 4762.5,
        ..shg(path, data, nx, ny)
    }
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// Cells as a reader returns them for what was written: DSS7 keeps every bit; DSS6
/// ZLIB HRAP/Albers grids come back with GRID_UNDEFINED for -FLT_MAX (Java heclib's
/// legacy normalization, P11).
fn expected_cells(v: u8, input: &GridInput) -> Vec<f32> {
    if v == 7 {
        return input.data.to_vec();
    }
    // DSS6 specified grids: the null value marks missing cells too (and -FLT_MAX cells
    // are stored as the null value), so both read back as GRID_UNDEFINED.
    let null = match &input.projection {
        GridProjection::Specified(s) => Some(s.null_value),
        _ => None,
    };
    input
        .data
        .iter()
        .map(|&x| {
            if x == GRID_NODATA || Some(x) == null {
                GRID_UNDEFINED
            } else {
                x
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------

#[test]
fn round_trips_both_versions() {
    let d = tmp("rt");
    for v in [7u8, 6] {
        let f = new_file(&d, v);
        let a = precip(12 * 9, 1);
        let b = precip(7 * 3, 2); // odd cell count
        let mut c: Vec<f32> = (0..40).map(|i| -4.25 + 0.5 * i as f32).collect();
        c[3] = -9999.0;
        c[7] = GRID_NODATA;
        let h = precip(9 * 7, 3);
        let one = [3.25f32];
        let p1 = b"/SHG/P24 T/PRECIP/01SEP2018:0100/01SEP2018:0200/RT/";
        let p2 = b"/SHG/P24 T/PRECIP/01SEP2018:2300/01SEP2018:2400/RT/";
        let p3 = b"/UTM17N/P24 T/TEMPERATURE/01JAN2020:0600//RT/";
        let p4 = b"/HRAP/P24 T/PRECIP/18JAN2019:0600/18JAN2019:0700/RT/";
        let p5 = b"/SHG/P24 T/ELEVATION///RT/";
        let mut inputs = vec![
            shg(p1, &a, 12, 9),
            shg(p2, &b, 7, 3),
            specified(p3, &c, 8, 5),
            hrap(p4, &h, 9, 7),
        ];
        let mut inv = shg(p5, &one, 1, 1);
        inv.time_stamped = false;
        inv.data_type = 2;
        inputs.push(inv);
        if v == 6 {
            inputs[3].compression = GridCompression::Precip2Byte {
                scale_factor: None,
                base: None,
            };
        } else {
            inputs[1].compression = GridCompression::Uncompressed;
        }
        for input in &inputs {
            assert_eq!(write(&f, v, input, &opts()).unwrap(), BlockOutcome::Added);
        }
        for input in &inputs {
            let path = if v == 6 {
                input.pathname.to_ascii_uppercase()
            } else {
                input.pathname.to_vec()
            };
            let g = read(&f, v, &path);
            // P60: time-invariant grids are 4x1 only for DSS7 specified grids.
            let want_type = input.projection.base_record_type()
                + i32::from(
                    !input.time_stamped
                        && v == 7
                        && matches!(input.projection, GridProjection::Specified(_)),
                );
            assert_eq!(g.record_type, want_type);
            assert_eq!((g.nx, g.ny), (input.nx, input.ny));
            assert_eq!(
                (g.lower_left_cell_x, g.lower_left_cell_y),
                (input.lower_left_cell_x, input.lower_left_cell_y)
            );
            assert_eq!(g.cell_size, input.cell_size);
            assert_eq!(g.data_units.as_bytes(), input.units);
            assert_eq!(g.data_type, input.data_type);
            let plan = dss_core::grid_write::plan_grid(input, v).unwrap();
            let st = &plan.stats;
            assert_eq!(
                (g.min_data_value, g.max_data_value, g.mean_data_value),
                (Some(st.min), Some(st.max), Some(st.mean))
            );
            assert_eq!(g.range_limit_table, st.range_limits);
            assert_eq!(g.number_equal_or_exceeding_range_limit, st.range_counts);
            if matches!(input.compression, GridCompression::Precip2Byte { .. }) {
                // Quantized to 1/scale; missing cells come back as GRID_UNDEFINED.
                for (x, y) in input.data.iter().zip(&g.data) {
                    if *x == GRID_NODATA {
                        assert_eq!(*y, GRID_UNDEFINED);
                    } else {
                        assert!((x - y).abs() <= 0.5 / g.compression_scale_factor + 1e-6);
                    }
                }
            } else {
                assert_eq!(bits(&g.data), bits(&expected_cells(v, input)));
            }
            match &input.projection {
                GridProjection::Albers(_) => {
                    assert_eq!(g.grid_type, GridType::Albers);
                    assert!(g
                        .srs_definition
                        .starts_with("PROJCS[\"USA_Contiguous_Albers"));
                    let al = g.albers.as_ref().unwrap();
                    assert_eq!(
                        (al.datum, al.first_standard_parallel, al.central_meridian),
                        (2, 29.5, -96.0)
                    );
                }
                GridProjection::Specified(_) => {
                    assert_eq!(g.srs_definition, UTM17N);
                    assert_eq!(g.null_value, -9999.0);
                    assert!(g.is_missing(-9999.0));
                    assert_eq!(
                        (g.x_coord_of_grid_cell_zero, g.y_coord_of_grid_cell_zero),
                        (500.0, -250.0)
                    );
                }
                GridProjection::Hrap { .. } => assert_eq!(g.grid_type, GridType::Hrap),
            }
            if input.time_stamped {
                assert!(g.start_time.is_some());
            }
        }
        // Equal rewrite writes nothing; a changed one replaces only its record.
        let before = std::fs::read(&f).unwrap();
        assert_eq!(
            write(&f, v, &inputs[0], &opts()).unwrap(),
            BlockOutcome::Unchanged
        );
        assert_eq!(
            std::fs::read(&f).unwrap(),
            before,
            "v{v}: equal rewrite wrote"
        );
        let changed: Vec<f32> = a
            .iter()
            .map(|x| if *x >= 0.0 { x + 1.0 } else { *x })
            .collect();
        let mut ci = shg(p1, &changed, 12, 9);
        ci.compression = inputs[0].compression;
        assert_eq!(write(&f, v, &ci, &opts()).unwrap(), BlockOutcome::Replaced);
        let after = std::fs::read(&f).unwrap();
        let target = if v == 6 {
            p1.to_ascii_uppercase()
        } else {
            p1.to_vec()
        };
        assert_others_identical(v, &before, &after, &target);
        assert_eq!(
            bits(&read(&f, v, &target).data),
            bits(&expected_cells(v, &ci))
        );
        assert_clean(&f, v, &format!("round trip v{v}"));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn albers_parameters_and_wkt() {
    // A non-SHG Albers grid: DSS7 stores the WKT Monolith builds (Java Float.toString),
    // and the reader recovers the parameters from it as Monolith does (P13/P34).
    let d = tmp("albers");
    let cells: Vec<f32> = (0..20).map(|i| i as f32 * 0.25).collect();
    let a = AlbersParameters {
        datum: 1,
        units: b"FEET".to_vec(),
        first_standard_parallel: 33.0,
        second_standard_parallel: 45.0,
        central_meridian: -89.5,
        latitude_of_projection_origin: 23.0,
        false_easting: 1000.0,
        false_northing: -250.0,
    };
    for v in [7u8, 6] {
        let f = new_file(&d, v);
        let mut g = shg(b"/ALB/P24/SWE/03SEP2018:1200//X/", &cells, 5, 4);
        g.projection = GridProjection::Albers(a.clone());
        g.x_coord_of_grid_cell_zero = 15.0;
        g.y_coord_of_grid_cell_zero = -30.0;
        g.data_type = 2;
        write(&f, v, &g, &opts()).unwrap();
        let r = read(&f, v, b"/ALB/P24/SWE/03SEP2018:1200//X/");
        let al = r.albers.unwrap();
        assert_eq!(al.datum, 1);
        assert_eq!(
            (
                al.first_standard_parallel,
                al.second_standard_parallel,
                al.central_meridian,
                al.latitude_of_projection_origin,
                al.false_easting,
                al.false_northing
            ),
            (33.0, 45.0, -89.5, 23.0, 1000.0, -250.0)
        );
        // Monolith reports "" for any non-SHG Albers grid.
        assert_eq!(r.srs_definition, "");
        if v == 7 {
            let stored = r.stored_srs_definition.unwrap();
            assert!(stored.contains("GCS_North_American_1927"));
            assert!(stored.contains("PARAMETER[\"Central_Meridian\",-89.5]"));
            assert!(stored.ends_with("UNIT[\"FEET\",0.0]]"));
        }
        assert_eq!(
            (r.x_coord_of_grid_cell_zero, r.y_coord_of_grid_cell_zero),
            (15.0, -30.0)
        );
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn precip_2_byte_runs_and_hec_quirks() {
    // Runs of exactly 16000·k zeros end with a zero-length run (as HEC writes them); the
    // reader accepts it. Missing runs over 16000 are split the same way.
    let d = tmp("p2b");
    let f = new_file(&d, 6);
    let mut z = vec![0.0f32; 16_001];
    z[0] = 2.0;
    let mut m = vec![GRID_NODATA; 32_001];
    m[32_000] = 4.0;
    let p2b = GridCompression::Precip2Byte {
        scale_factor: None,
        base: None,
    };
    let mut a = shg(
        b"/SHG/RUN/PRECIP/07SEP2018:0100/07SEP2018:0200/X/",
        &z,
        16_001,
        1,
    );
    a.compression = p2b;
    let mut b = shg(
        b"/SHG/RUN/PRECIP/07SEP2018:0200/07SEP2018:0300/X/",
        &m,
        32_001,
        1,
    );
    b.compression = p2b;
    write(&f, 6, &a, &opts()).unwrap();
    write(&f, 6, &b, &opts()).unwrap();
    let ga = read(&f, 6, a.pathname);
    assert_eq!(ga.sizeof_compressed_elements, 6);
    assert_eq!(bits(&ga.data), bits(&z));
    let gb = read(&f, 6, b.pathname);
    assert_eq!(
        gb.data[..32_000]
            .iter()
            .filter(|x| **x == GRID_UNDEFINED)
            .count(),
        32_000
    );
    assert_eq!(gb.data[32_000], 4.0);
    // Monolith's automatic scale leaves this maximum above 32767 steps (scale 38), and
    // Monolith stores it as missing; the writer refuses.
    let lossy = [0.0f32, 870.71];
    let mut c = shg(
        b"/SHG/AUTO/PRECIP/07SEP2018:0100/07SEP2018:0200/X/",
        &lossy,
        2,
        1,
    );
    c.compression = p2b;
    let before = std::fs::read(&f).unwrap();
    assert!(matches!(
        write(&f, 6, &c, &opts()),
        Err(WriteError::BadInput(_))
    ));
    assert_eq!(std::fs::read(&f).unwrap(), before);
    // An explicit scale that fits is accepted and quantizes as heclib's reader decodes.
    c.compression = GridCompression::Precip2Byte {
        scale_factor: Some(10.0),
        base: Some(0.0),
    };
    write(&f, 6, &c, &opts()).unwrap();
    let g = read(&f, 6, c.pathname);
    assert_eq!(g.compression_scale_factor, 10.0);
    assert!((g.data[1] - 870.7).abs() < 1e-3);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn statistics_match_monolith_rules() {
    // GridData.updateStatistics: -FLT_MAX and the null value excluded; HRAP/Albers null
    // is -1.0E30; every cell counts toward the range table.
    let v = [1.0f32, GRID_NODATA, UNDEFINED_STORAGE, 4.0, 0.0, 6.0e-8];
    let s = grid_statistics(&v, 3, 2, UNDEFINED_STORAGE);
    assert_eq!((s.min, s.max), (0.0, 4.0));
    assert_eq!(s.range_counts[..4], [6, 4, 2, 0]);
    let p = b"/SHG/S/PRECIP/01SEP2018:0100/01SEP2018:0200/X/";
    let plan = validate_grid_input(&shg(p, &v, 3, 2)).unwrap();
    assert_eq!(plan.stats, s);
    assert_eq!(plan.record_type, 420);
    assert_eq!(plan.start, Some((43_343, 3_600)));
    assert_eq!(plan.end, Some((43_343, 7_200)));
}

// ---------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------

#[test]
fn refusals_leave_the_file_unchanged() {
    let d = tmp("refuse");
    for v in [7u8, 6] {
        let f = new_file(&d, v);
        let cells = precip(12, 4);
        let ok_path = b"/SHG/R/PRECIP/01SEP2018:0100/01SEP2018:0200/X/";
        write(&f, v, &shg(ok_path, &cells, 4, 3), &opts()).unwrap();
        if v == 7 {
            // A time-series record in the file, untouched by every refusal below.
            let p = b"/A/B/FLOW/01JAN2020/1Hour/TS/";
            let input = RtsInput::new(
                p,
                43_831 * 86_400 + 3_600,
                RtsValues::F32(&[1.0]),
                b"cfs",
                b"INST-VAL",
            );
            write_rts(&f, &input, &opts()).unwrap();
        }
        let before = std::fs::read(&f).unwrap();
        let mut cases: Vec<Case> = Vec::new();
        let bad_path = |e: &WriteError| matches!(e, WriteError::BadPathname(_));
        let bad_input = |e: &WriteError| matches!(e, WriteError::BadInput(_));
        let unsupported = |e: &WriteError| matches!(e, WriteError::Unsupported(_));
        cases.push((
            "8 slashes",
            shg(
                b"/SHG/R/PRECIP/01SEP2018:0100/01SEP2018:0200/X/Y/",
                &cells,
                4,
                3,
            ),
            bad_path,
        ));
        cases.push((
            "6 slashes",
            shg(b"/SHG/R/PRECIP/01SEP2018:0100/X/", &cells, 4, 3),
            bad_path,
        ));
        cases.push((
            "byte 0xe9",
            shg(
                "/SHG/R/PR\u{e9}CIP/01SEP2018:0100/01SEP2018:0200/X/".as_bytes(),
                &cells,
                4,
                3,
            ),
            bad_path,
        ));
        cases.push((
            "no end",
            shg(b"/SHG/R/PRECIP/01SEP2018:0100//X/", &cells, 4, 3),
            bad_path,
        ));
        cases.push((
            "end = start",
            shg(
                b"/SHG/R/PRECIP/01SEP2018:0100/01SEP2018:0100/X/",
                &cells,
                4,
                3,
            ),
            bad_path,
        ));
        cases.push((
            "end < start",
            shg(
                b"/SHG/R/PRECIP/01SEP2018:0200/01SEP2018:0100/X/",
                &cells,
                4,
                3,
            ),
            bad_path,
        ));
        cases.push((
            "bad D",
            shg(b"/SHG/R/PRECIP/01SEP2018/01SEP2018:0100/X/", &cells, 4, 3),
            bad_path,
        ));
        cases.push((
            "bad hour",
            shg(
                b"/SHG/R/PRECIP/01SEP2018:2401/02SEP2018:0100/X/",
                &cells,
                4,
                3,
            ),
            bad_path,
        ));
        cases.push((
            "collection",
            shg(
                b"/SHG/R/PRECIP/01SEP2018:0100/01SEP2018:0200/C:0001|X/",
                &cells,
                4,
                3,
            ),
            bad_path,
        ));
        cases.push(("cells != nx*ny", shg(ok_path, &cells, 5, 3), bad_input));
        cases.push(("nx 0", shg(ok_path, &[], 0, 3), bad_input));
        let mut g = shg(ok_path, &cells, 4, 3);
        g.cell_size = 0.0;
        cases.push(("cell size 0", g, bad_input));
        let mut g = shg(ok_path, &cells, 4, 3);
        g.cell_size = f32::NAN;
        cases.push(("cell size NaN", g, bad_input));
        let mut g = hrap(ok_path, &cells, 4, 3);
        g.x_coord_of_grid_cell_zero = 1.0;
        cases.push(("HRAP origin", g, bad_input));
        let mut g = shg(ok_path, &cells, 4, 3);
        g.data_type = 7;
        cases.push(("data type 7", g, bad_input));
        let mut g = shg(ok_path, &cells, 4, 3);
        g.units = b"MILLIMETRES PER HOUR";
        if v == 6 {
            cases.push(("units > 12 on DSS6", g, bad_input));
        }
        let mut g = shg(ok_path, &cells, 4, 3);
        g.units = b"M\0M";
        cases.push(("NUL in units", g, bad_input));
        let mut g = shg(b"/SHG/R/X///X/", &cells, 4, 3);
        g.time_stamped = true;
        cases.push(("time-stamped without D", g, bad_path));
        let mut g = shg(ok_path, &cells, 4, 3);
        g.time_stamped = false;
        cases.push(("time-invariant with D", g, bad_path));
        let mut g = shg(ok_path, &cells, 4, 3);
        g.lower_left_cell_x = i32::MAX - 1;
        cases.push(("index overflow", g, bad_input));
        let mut g = specified(ok_path, &cells, 4, 3);
        if let GridProjection::Specified(s) = &mut g.projection {
            s.null_value = f32::NAN;
        }
        cases.push(("NaN null", g, bad_input));
        let mut g = shg(ok_path, &cells, 4, 3);
        if v == 7 {
            g.compression = GridCompression::Precip2Byte {
                scale_factor: None,
                base: None,
            };
            cases.push(("PRECIP_2_BYTE on DSS7", g, unsupported));
        } else {
            g.compression = GridCompression::Uncompressed;
            cases.push(("uncompressed on DSS6", g, unsupported));
            let nan = [f32::NAN, 1.0, 2.0, 0.0];
            let mut g = shg(ok_path, &nan, 2, 2);
            g.compression = GridCompression::Precip2Byte {
                scale_factor: None,
                base: None,
            };
            // The cells must outlive the case list: leak this small vector (test only).
            let g: GridInput<'static> = unsafe_free_static(g);
            cases.push(("NaN with PRECIP_2_BYTE", g, bad_input));
            let mut g = shg(ok_path, &cells, 4, 3);
            g.compression = GridCompression::Precip2Byte {
                scale_factor: Some(100.0),
                base: None,
            };
            cases.push(("scale without base", g, bad_input));
            let mut g = shg(ok_path, &cells, 4, 3);
            g.compression = GridCompression::Precip2Byte {
                scale_factor: Some(100.0),
                base: Some(5.0),
            };
            cases.push(("values below the base", g, bad_input));
        }
        // Another grid type at an existing grid's pathname.
        cases.push(("420 -> 410", hrap(ok_path, &cells, 4, 3), unsupported));
        for (what, input, ok) in &cases {
            let e = write(&f, v, input, &opts()).expect_err(what);
            assert!(ok(&e), "v{v} {what}: {e:?}");
            assert_eq!(
                std::fs::read(&f).unwrap(),
                before,
                "v{v} {what}: file changed"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A refusal case: what, the input, and the error variant expected.
type Case<'a> = (&'a str, GridInput<'a>, fn(&WriteError) -> bool);

/// Leak a small input so a test can keep it in a `'static` list.
fn unsafe_free_static(g: GridInput<'_>) -> GridInput<'static> {
    let data: &'static [f32] = Box::leak(g.data.to_vec().into_boxed_slice());
    let path: &'static [u8] = Box::leak(g.pathname.to_vec().into_boxed_slice());
    let units: &'static [u8] = Box::leak(g.units.to_vec().into_boxed_slice());
    GridInput {
        pathname: path,
        data,
        units,
        projection: g.projection,
        ..g
    }
}

/// A time-invariant grid at the pathname of a paired-data record (D and E empty in both)
/// is refused: the writer does not change a record's type (`zwriteExisting()`).
#[test]
fn type_change_of_another_record_is_refused() {
    let d = tmp("typechange");
    for (v, name) in [(7u8, "sample7.dss"), (6, "sample6.dss")] {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public")
            .join(name);
        let f = d.join(name);
        std::fs::copy(&src, &f).unwrap();
        let before = std::fs::read(&f).unwrap();
        let cells = [1.0f32; 4];
        let mut g = shg(b"/MY BASIN/DEER CREEK/STAGE-FLOW///USGS/", &cells, 2, 2);
        g.time_stamped = false;
        g.data_type = 2;
        let e = write(&f, v, &g, &opts()).expect_err("type change");
        assert!(matches!(e, WriteError::Unsupported(_)), "v{v}: {e:?}");
        assert_eq!(std::fs::read(&f).unwrap(), before, "v{v}: file changed");
    }
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Word snapshots and checks
// ---------------------------------------------------------------------------------

fn words7(b: &[u8]) -> Vec<i64> {
    b.chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// DSS7: pathname (upper) -> bin entry + info + area words.
fn records7(w: &[i64]) -> BTreeMap<Vec<u8>, Vec<i64>> {
    let (ht, mh, bs) = (w[19], w[15], w[23]);
    let mut out = BTreeMap::new();
    for slot in 0..mh {
        let mut a = w[(ht + slot) as usize];
        while a != 0 {
            let mut ic = 0i64;
            while ic + 7 < bs && w[(a + ic) as usize] != 0 {
                let e = (a + ic) as usize;
                let pl = w[e + 2];
                let (nch, pw) = ((pl as u32) as usize, pl >> 32);
                if w[e + 1] == 0 || w[e + 1] == 1 {
                    let path: Vec<u8> = w[e + 7..e + 7 + pw as usize]
                        .iter()
                        .flat_map(|x| x.to_le_bytes())
                        .take(nch)
                        .collect();
                    let info = w[e + 3] as usize;
                    let mut v: Vec<i64> = w[e..e + 7 + pw as usize].to_vec();
                    v.extend_from_slice(&w[info..info + 30 + pw as usize]);
                    for k in [13usize, 15, 17, 19, 21, 23] {
                        let (ad, n) = (w[info + k] as usize, w[info + k + 1]);
                        v.extend_from_slice(&w[ad..ad + ((n + 1) / 2) as usize]);
                    }
                    out.insert(path.to_ascii_uppercase(), v);
                }
                ic += 7 + pw;
                if ic >= bs - 2 {
                    break;
                }
            }
            a = w[(a + bs - 1) as usize];
        }
    }
    out
}

/// DSS6: pathname -> bin entry, info block and the record's words.
fn records6(b: &[u8]) -> BTreeMap<Vec<u8>, Vec<i32>> {
    let mut w = vec![0i32];
    for r in b.chunks(512) {
        let n = r.len().min(508) / 4;
        w.extend((0..n).map(|i| i32::from_le_bytes(r[4 * i..4 * i + 4].try_into().unwrap())));
    }
    let (mut addr, kbnblk) = (w[20] as usize, w[17]);
    let mut nbin = kbnblk;
    let mut out = BTreeMap::new();
    while addr != 0 {
        let bn = w[addr..addr + 127].to_vec();
        let mut j = 1usize;
        while j <= 125 {
            let st = bn[j - 1];
            if st == 0 || st == -1 {
                break;
            }
            let np = bn[j] as usize;
            let p = (np - 1) / 4 + 1;
            if st == 1 || st == 11 {
                let name: Vec<u8> = bn[j + 1..j + 1 + p]
                    .iter()
                    .flat_map(|x| x.to_le_bytes())
                    .take(np)
                    .collect();
                let ia = bn[j + 1 + p] as usize;
                let info = &w[ia..ia + 28 + p];
                let f = |k: usize| info[3 + p + k];
                let mut v = bn[j - 1..j - 1 + p + 11].to_vec();
                v.extend_from_slice(info);
                v.extend_from_slice(&w[f(17) as usize..(f(17) + f(18)) as usize]);
                v.extend_from_slice(&w[f(2) as usize..(f(2) + f(3)) as usize]);
                out.insert(name, v);
            }
            j += p + 11;
        }
        nbin -= 1;
        if nbin == 0 {
            addr = bn[126] as usize;
            nbin = kbnblk;
        } else {
            addr += 127;
        }
    }
    out
}

fn assert_others_identical(v: u8, before: &[u8], after: &[u8], target: &[u8]) {
    if v == 7 {
        let (a, b) = (records7(&words7(before)), records7(&words7(after)));
        let t = target.to_ascii_uppercase();
        for (k, x) in &a {
            if *k != t {
                assert_eq!(
                    b.get(k),
                    Some(x),
                    "record {} changed",
                    String::from_utf8_lossy(k)
                );
            }
        }
        assert!(b.keys().all(|k| *k == t || a.contains_key(k)));
    } else {
        let (a, b) = (records6(before), records6(after));
        for (k, x) in &a {
            if k.as_slice() != target {
                assert_eq!(
                    b.get(k),
                    Some(x),
                    "record {} changed",
                    String::from_utf8_lossy(k)
                );
            }
        }
        assert!(b
            .keys()
            .all(|k| k.as_slice() == target || a.contains_key(k)));
    }
}

/// The file is consistent: DSS7 catalog without anomalies and `knumberRecords` equal to
/// the live entries; DSS6 `check_file6` clean. Every live grid reads.
fn assert_clean(path: &Path, v: u8, what: &str) {
    if v == 7 {
        let mut f = Dss7File::open(path).unwrap();
        let cat = f.catalog().unwrap();
        assert!(
            cat.catalog.anomalies.is_empty(),
            "{what}: {:?}",
            cat.catalog.anomalies
        );
        let live: Vec<(Vec<u8>, i32)> = cat
            .catalog
            .live()
            .map(|e| (e.pathname.clone(), e.record_type))
            .collect();
        assert_eq!(f.header().number_records, live.len() as i64, "{what}");
        for (p, t) in live {
            if (400..=431).contains(&t) {
                f.read_grid(&p).unwrap_or_else(|e| panic!("{what}: {e}"));
            }
        }
        assert_eq!(
            f.header().words[29],
            0,
            "{what}: in-progress marker left set"
        );
    } else {
        let r = check_file6(path).unwrap();
        assert!(r.is_clean(), "{what}: {:?}", r.problems);
        let f = Dss6File::open(path).unwrap();
        for e in f.catalog().unwrap().live() {
            if (400..=431).contains(&e.record_type) {
                f.read_grid(&e.pathname)
                    .unwrap_or_else(|x| panic!("{what}: {x}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------------
// Crash at every step
// ---------------------------------------------------------------------------------

#[test]
fn crash_after_every_step_then_recover() {
    let d = tmp("crash");
    let steps7 = WriteStep::ALL.to_vec();
    let steps6 = DSS6_STEP_ORDER.to_vec();
    for v in [7u8, 6] {
        let steps = if v == 7 { &steps7 } else { &steps6 };
        let base = new_file(&d, v);
        let cells: Vec<Vec<f32>> = (0..4).map(|i| precip(30, 10 + i)).collect();
        let paths: Vec<String> = (0..4)
            .map(|i| {
                format!(
                    "/SHG/CRASH/PRECIP/0{}SEP2018:0100/0{}SEP2018:0200/X/",
                    i + 1,
                    i + 1
                )
            })
            .collect();
        for (p, c) in paths.iter().zip(&cells) {
            write(&base, v, &shg(p.as_bytes(), c, 6, 5), &opts()).unwrap();
        }
        let newc = precip(30, 99);
        let scenarios: [(&str, &[u8]); 2] = [
            ("add", b"/SHG/CRASH/PRECIP/09SEP2018:0100/09SEP2018:0200/X/"),
            ("replace", paths[0].as_bytes()),
        ];
        for (name, target) in scenarios {
            for &step in steps.iter() {
                let what = format!("v{v} {name} {step:?}");
                let f = d.join(format!("c{v}_{name}_{step:?}.dss"));
                std::fs::copy(&base, &f).unwrap();
                let before = std::fs::read(&f).unwrap();
                let o = WriteOptions {
                    fault: Some(FaultPoint { block: 0, step }),
                    ..opts()
                };
                let input = shg(target, &newc, 6, 5);
                let e = write(&f, v, &input, &o).expect_err(&what);
                assert!(matches!(e, WriteError::InjectedFault { .. }), "{what}: {e}");
                let after = std::fs::read(&f).unwrap();
                let t = if v == 6 {
                    target.to_ascii_uppercase()
                } else {
                    target.to_vec()
                };
                assert_others_identical(v, &before, &after, &t);
                // The target reads as the old or the new grid, or is not there yet.
                let old = if name == "replace" {
                    Some(&cells[0])
                } else {
                    None
                };
                let got = if v == 7 {
                    Dss7File::open(&f).unwrap().read_grid(&t).ok()
                } else {
                    Dss6File::open(&f).unwrap().read_grid(&t).ok()
                };
                if let Some(g) = got {
                    let new_bits = bits(&expected_cells(v, &input));
                    let ok_new = bits(&g.data) == new_bits;
                    let ok_old = old.is_some_and(|o| {
                        bits(&g.data) == bits(&expected_cells(v, &shg(target, o, 6, 5)))
                    });
                    assert!(ok_new || ok_old, "{what}: target is neither old nor new");
                }
                // The next native write repairs the file and stores the grid.
                write(&f, v, &input, &opts()).unwrap();
                assert_clean(&f, v, &what);
                assert_eq!(
                    bits(&read(&f, v, &t).data),
                    bits(&expected_cells(v, &input)),
                    "{what}"
                );
                for (p, c) in paths.iter().zip(&cells) {
                    let pp = if v == 6 {
                        p.to_ascii_uppercase()
                    } else {
                        p.clone()
                    };
                    if pp.as_bytes() != t.as_slice() {
                        assert_eq!(
                            bits(&read(&f, v, pp.as_bytes()).data),
                            bits(&expected_cells(v, &shg(p.as_bytes(), c, 6, 5))),
                            "{what}: {p}"
                        );
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Lock
// ---------------------------------------------------------------------------------

/// Windows: a whole-file lock held by another handle (as the writer itself holds one,
/// P20 §2) is seen by the guard: the write is refused and the file is unchanged.
#[test]
#[cfg(windows)]
#[allow(clippy::incompatible_msrv)]
fn locked_file_is_refused() {
    let d = tmp("lock");
    for v in [7u8, 6] {
        let f = new_file(&d, v);
        let cells = precip(12, 5);
        let p = b"/SHG/L/PRECIP/01SEP2018:0100/01SEP2018:0200/X/";
        let before = std::fs::read(&f).unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&f)
            .unwrap();
        holder.try_lock().unwrap();
        let e = write(&f, v, &shg(p, &cells, 4, 3), &opts()).expect_err("locked");
        // Either the guard's refusal, or the guard's read hitting the held byte range
        // (ERROR_LOCK_VIOLATION, 33): both stop the write before anything is written.
        let refused = matches!(&e, WriteError::Locked(_))
            || matches!(&e, WriteError::Lock(dss_core::lock::LockError::Io(io))
                if io.raw_os_error() == Some(33));
        assert!(refused, "v{v}: {e:?}");
        drop(holder);
        assert_eq!(std::fs::read(&f).unwrap(), before, "v{v}: file changed");
        write(&f, v, &shg(p, &cells, 4, 3), &opts()).unwrap();
    }
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// Fuzz
// ---------------------------------------------------------------------------------

#[test]
fn fuzz_input_validation() {
    let d = tmp("fuzz");
    let parts: [&[u8]; 8] = [
        b"01SEP2018:0100",
        b"01SEP2018:2400",
        b"31FEB2018:0100",
        b"",
        b"X",
        b"01sep2018:0130",
        b"01SEP2018:0200",
        b"99XXX9999:9999",
    ];
    let mut accepted = 0;
    for v in [7u8, 6] {
        let f = new_file(&d, v);
        let mut r = Rng(0x5eed + u64::from(v));
        for i in 0..800 {
            let (dpart, epart) = if r.below(2) == 0 {
                (parts[0], parts[6])
            } else {
                (parts[r.below(8) as usize], parts[r.below(8) as usize])
            };
            let mut p = b"/SHG/F".to_vec();
            p.extend_from_slice(format!("{}/PRECIP/", i % 7).as_bytes());
            p.extend_from_slice(dpart);
            p.push(b'/');
            p.extend_from_slice(epart);
            p.extend_from_slice(b"/X/");
            if r.below(20) == 0 {
                p.push(b'/');
            }
            if r.below(30) == 0 {
                p[3] = 0x01;
            }
            let nx = r.below(6) as usize;
            let ny = r.below(6) as usize;
            let n = if r.below(10) == 0 {
                nx * ny + 1
            } else {
                nx * ny
            };
            let data: Vec<f32> = (0..n)
                .map(|_| match r.below(12) {
                    0 => GRID_NODATA,
                    1 => f32::NAN,
                    2 => -(r.below(1000) as f32),
                    3 => f32::from_bits(r.next() as u32),
                    _ => r.below(100_000) as f32 / 100.0,
                })
                .collect();
            let compression = match r.below(4) {
                0 => GridCompression::Zlib,
                1 => GridCompression::Uncompressed,
                2 => GridCompression::Precip2Byte {
                    scale_factor: None,
                    base: None,
                },
                _ => GridCompression::Precip2Byte {
                    scale_factor: Some(r.below(200) as f32),
                    base: Some(-(r.below(5) as f32)),
                },
            };
            let mut input = shg(&p, &data, nx, ny);
            input.compression = compression;
            input.data_type = r.below(5) as i32;
            input.cell_size =
                [2000.0, 0.5, 0.0, -1.0, f32::INFINITY][r.below(5).min(r.below(5)) as usize];
            input.time_stamped = r.below(8) != 0;
            input.lower_left_cell_x =
                [0, -5, 1_000_000, i32::MAX, i32::MIN][r.below(5).min(r.below(5)) as usize];
            let before = std::fs::read(&f).unwrap();
            match write(&f, v, &input, &opts()) {
                Ok(_) => {
                    accepted += 1;
                    let stored = if v == 6 {
                        p.to_ascii_uppercase()
                    } else {
                        p.clone()
                    };
                    let g = read(&f, v, &stored);
                    assert_eq!((g.nx, g.ny), (nx, ny));
                }
                Err(WriteError::BadPathname(_))
                | Err(WriteError::BadInput(_))
                | Err(WriteError::Unsupported(_)) => {
                    assert_eq!(
                        std::fs::read(&f).unwrap(),
                        before,
                        "refused write changed the file"
                    );
                }
                Err(e) => panic!("untyped refusal: {e:?}"),
            }
        }
        assert_clean(&f, v, "fuzz");
    }
    assert!(accepted > 50, "only {accepted} accepted writes");
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------------
// HEC-written fixture (HEC Monolith 3.3.27, synthetic values; P24 REPORT section 2)
// ---------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

/// One operation of `p24_hec_grid_ops.tsv`: version, then the P24Monolith columns.
struct Op {
    version: u8,
    cols: Vec<String>,
    cells: Vec<f32>,
}

fn fixture_ops() -> Vec<Op> {
    let z = std::fs::read(fixture("p24_hec_grid_cells.zlib")).unwrap();
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&z).unwrap();
    let all: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let text = std::fs::read_to_string(fixture("p24_hec_grid_ops.tsv")).unwrap();
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let c: Vec<String> = l.split('\t').map(str::to_string).collect();
            let (off, n) = c[13][1..].split_once(':').unwrap();
            let (off, n): (usize, usize) = (off.parse().unwrap(), n.parse().unwrap());
            Op {
                version: c[0].parse().unwrap(),
                cols: c[1..].to_vec(),
                cells: all[off..off + n].to_vec(),
            }
        })
        .collect()
}

/// The input Monolith's `storeGriddedData` received for an operation (compression 0:
/// Monolith's choice for a GridInfo left at its default).
fn op_input(op: &Op) -> GridInput<'_> {
    let c = &op.cols;
    let f = |s: &str| s.parse::<f32>().unwrap();
    let (mut x0, mut y0) = (0.0, 0.0);
    let projection = match c[1].as_str() {
        "hrap" => GridProjection::Hrap {
            data_source: c[13].as_bytes().to_vec(),
        },
        "albers" => {
            let a: Vec<&str> = c[13].split(';').collect();
            x0 = f(a[8]);
            y0 = f(a[9]);
            GridProjection::Albers(AlbersParameters {
                datum: a[0].parse().unwrap(),
                units: a[1].as_bytes().to_vec(),
                first_standard_parallel: f(a[2]),
                second_standard_parallel: f(a[3]),
                central_meridian: f(a[4]),
                latitude_of_projection_origin: f(a[5]),
                false_easting: f(a[6]),
                false_northing: f(a[7]),
            })
        }
        _ => {
            let s: Vec<&str> = c[13].split('|').collect();
            x0 = f(s[2]);
            y0 = f(s[3]);
            GridProjection::Specified(SpecifiedReference {
                srs_name: s[0].as_bytes().to_vec(),
                srs_definition: s[1].as_bytes().to_vec(),
                srs_definition_type: 0,
                null_value: f(s[4]),
                time_zone_id: s[5].as_bytes().to_vec(),
                time_zone_raw_offset: s[6].parse().unwrap(),
                is_interval: s[7] == "1",
            })
        }
    };
    let data_type: i32 = c[8].parse().unwrap();
    let parts: Vec<&str> = c[0].split('/').collect();
    let t = |p: &str| dss_core::grid_write::parse_grid_time(p.as_bytes());
    let one_hour = matches!((t(parts[4]), t(parts[5])), (Some(a), Some(b))
        if (b.0 - a.0) * 86_400 + b.1 - a.1 == 3_600);
    let explicit = (c[10] != "-" && c[11] != "-").then(|| (f(&c[10]), f(&c[11])));
    let p2b = GridCompression::Precip2Byte {
        scale_factor: explicit.map(|e| e.0),
        base: explicit.map(|e| e.1),
    };
    let specified = matches!(projection, GridProjection::Specified(_));
    let compression = match (c[9].as_str(), op.version) {
        ("26", _) => GridCompression::Zlib,
        ("101001", _) => p2b,
        (_, 7) => GridCompression::Zlib,
        // GriddedData.storeGriddedData offsets 288-363.
        _ if !specified && one_hour && data_type == 1 => p2b,
        _ => GridCompression::Zlib,
    };
    GridInput {
        pathname: c[0].as_bytes(),
        projection,
        time_stamped: true,
        data: &op.cells,
        nx: c[2].parse().unwrap(),
        ny: c[3].parse().unwrap(),
        lower_left_cell_x: c[4].parse().unwrap(),
        lower_left_cell_y: c[5].parse().unwrap(),
        cell_size: f(&c[6]),
        x_coord_of_grid_cell_zero: x0,
        y_coord_of_grid_cell_zero: y0,
        units: c[7].as_bytes(),
        data_type,
        compression,
    }
}

fn inflate(ints: &[i32], bytes: usize) -> Vec<u8> {
    let b: Vec<u8> = ints
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .take(bytes)
        .collect();
    miniz_oxide::inflate::decompress_to_vec_zlib(&b).unwrap()
}

/// DSS7 record parts: info words, then ih, h2, uh, v1, v2, v3 (ints).
fn parts7(b: &[u8], path: &[u8]) -> (Vec<i64>, [Vec<i32>; 6]) {
    let w = words7(b);
    let mut f = Dss7File::from_bytes(b).unwrap();
    let r = f.lookup(path).unwrap().unwrap();
    let info = r.info_address as usize;
    let iw = w[info..info + 30].to_vec();
    let area = |k: usize| -> Vec<i32> {
        let (ad, n) = (iw[k] as usize, iw[k + 1] as usize);
        w[ad..ad + n.div_ceil(2)]
            .iter()
            .flat_map(|x| [*x as i32, (*x >> 32) as i32])
            .take(n)
            .collect()
    };
    let a = [area(13), area(15), area(17), area(19), area(21), area(23)];
    (iw, a)
}

/// DSS6 record parts: the 25 info fields after the pathname (`KIPASS`..`KIUNUS`), the
/// internal header and the data words.
fn parts6(b: &[u8], path: &[u8]) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let mut w = vec![0i32];
    for r in b.chunks(512) {
        let n = r.len().min(508) / 4;
        w.extend((0..n).map(|i| i32::from_le_bytes(r[4 * i..4 * i + 4].try_into().unwrap())));
    }
    let f = Dss6File::from_bytes(b).unwrap();
    let rec = f.lookup(path).unwrap().unwrap();
    let ia = rec.entry.info_address as usize;
    let p = (path.len() - 1) / 4 + 1;
    let fields = w[ia + 3 + p..ia + 3 + p + 25].to_vec();
    let ih = w[fields[17] as usize..(fields[17] + fields[18]) as usize].to_vec();
    let data = w[fields[2] as usize..(fields[2] + fields[3]) as usize].to_vec();
    (fields, ih, data)
}

#[test]
fn hec_fixture_replay_and_read() {
    let ops = fixture_ops();
    assert_eq!(ops.iter().filter(|o| o.version == 6).count(), 19);
    assert_eq!(ops.iter().filter(|o| o.version == 7).count(), 18);
    let d = tmp("fixture");
    for v in [7u8, 6] {
        let hec_path = fixture(&format!("p24_hec_grid{v}.dss"));
        let hec = std::fs::read(&hec_path).unwrap();
        let f = new_file(&d, v);
        for op in ops.iter().filter(|o| o.version == v) {
            write(&f, v, &op_input(op), &opts()).unwrap();
        }
        let nat = std::fs::read(&f).unwrap();
        assert_clean(&f, v, "native replay");
        let (mut identical, mut deflate_only) = (0, 0);
        for op in ops.iter().filter(|o| o.version == v) {
            let input = op_input(op);
            let path = if v == 6 {
                input.pathname.to_ascii_uppercase()
            } else {
                input.pathname.to_vec()
            };
            let what = format!("v{v} {}", String::from_utf8_lossy(&path));
            let zlib = !matches!(input.compression, GridCompression::Precip2Byte { .. });
            if v == 7 {
                let (hi, [hih, hh2, huh, hv1, hv2, hv3]) = parts7(&hec, &path);
                let (ni, [nih, nh2, nuh, nv1, nv2, nv3]) = parts7(&nat, &path);
                // Type/version, first/last date, area sizes except values3, number data.
                // Without a D part heclib leaves kdataFirstDate/kdataLastDate as the
                // previous store on the handle set them (zspatialGridStore.c lines
                // 297-317), so HEC's words are stale there; the writer stores 0.
                let timed = !input
                    .pathname
                    .split(|&b| b == b'/')
                    .nth(4)
                    .unwrap()
                    .is_empty();
                for k in [2usize, 4, 5, 9, 10, 14, 16, 18, 20, 22, 26, 27] {
                    if timed || (k != 9 && k != 10) {
                        assert_eq!(hi[k], ni[k], "{what}: info word {k}");
                    }
                }
                if !timed {
                    assert_eq!((ni[9], ni[10]), (0, 0), "{what}");
                }
                assert_eq!(hih[..9], nih[..9], "{what}: ih");
                assert_eq!(hih[10..], nih[10..], "{what}: ih");
                assert_eq!((hh2, hv1, hv2), (nh2, nv1, nv2), "{what}: h2/v1/v2");
                // Null value: Monolith stores 0.0 for HRAP and Albers, the writer -FLT_MAX.
                assert_eq!(huh[..3], nuh[..3], "{what}: geometry");
                assert_eq!(huh[4..], nuh[4..], "{what}: user header");
                if matches!(input.projection, GridProjection::Specified(_)) {
                    assert_eq!(huh[3], nuh[3], "{what}: null value");
                } else {
                    assert_eq!((huh[3], nuh[3]), (0, GRID_NODATA.to_bits() as i32));
                }
                let hc = inflate(&hv3, hih[9] as usize);
                let nc = inflate(&nv3, nih[9] as usize);
                assert_eq!(hc, nc, "{what}: inflated cells");
                if hv3 == nv3 {
                    identical += 1;
                } else {
                    deflate_only += 1;
                }
            } else {
                let (hf, hih, hd) = parts6(&hec, &path);
                let (nf, nih, nd) = parts6(&nat, &path);
                // KIVER, KITYPE, KICOMP, KIQUAL, KINIHE, KINCHE, KINUHE, KIPREC.
                for k in [5usize, 14, 15, 16, 18, 20, 22, 23] {
                    assert_eq!(hf[k], nf[k], "{what}: info field {k}");
                }
                if zlib {
                    assert_eq!(hih[..16], nih[..16], "{what}: header");
                    assert_eq!(hih[17..], nih[17..], "{what}: header");
                    let hc = inflate(&hd, hih[16] as usize);
                    let nc = inflate(&nd, nih[16] as usize);
                    assert_eq!(hc, nc, "{what}: inflated cells");
                    if hd == nd {
                        identical += 1;
                    } else {
                        deflate_only += 1;
                    }
                } else {
                    // KINDAT, KILNDA.
                    assert_eq!((hf[3], hf[4]), (nf[3], nf[4]), "{what}: sizes");
                    assert_eq!(hih, nih, "{what}: header");
                    assert_eq!(hd, nd, "{what}: PRECIP_2_BYTE data");
                    identical += 1;
                }
            }
            // The engine reads the HEC-written record as the input implies.
            let g = read(&hec_path, v, &path);
            if zlib {
                assert_eq!(
                    bits(&g.data),
                    bits(&expected_cells(v, &input)),
                    "{what}: cells"
                );
            } else {
                for (x, y) in input.data.iter().zip(&g.data) {
                    if *x == GRID_NODATA {
                        assert_eq!(*y, GRID_UNDEFINED, "{what}");
                    } else {
                        let tol = 0.5 / g.compression_scale_factor + 1e-6;
                        assert!((x - y).abs() <= tol, "{what}");
                    }
                }
            }
        }
        let n = ops.iter().filter(|o| o.version == v).count();
        assert!(identical > 0 && identical + deflate_only == n, "v{v}");
        eprintln!("v{v}: compressed cells word-identical in {identical} records, deflate streams differ in {deflate_only}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Review m2/m3/m1: DSS6 times beyond its 32-bit minutes, Albers units that would break
/// the WKT, and a non-finite null value are refused before anything is written.
#[test]
fn range_and_text_refusals() {
    let d = tmp("ranges");
    let cells = precip(12, 6);
    for v in [7u8, 6] {
        let f = new_file(&d, v);
        let before = std::fs::read(&f).unwrap();
        let far = shg(
            b"/SHG/R/PRECIP/01JAN6000:0100/01JAN6000:0200/X/",
            &cells,
            4,
            3,
        );
        let r = write(&f, v, &far, &opts());
        if v == 6 {
            assert!(matches!(r, Err(WriteError::BadPathname(_))), "{r:?}");
            assert_eq!(std::fs::read(&f).unwrap(), before);
        } else {
            assert_eq!(r.unwrap(), BlockOutcome::Added);
        }
        let before = std::fs::read(&f).unwrap();
        let p = b"/SHG/R/PRECIP/01JAN2020:0100/01JAN2020:0200/X/";
        for units in [&b"ME\"TERS"[..], b"M]", b"M[", b"M,1"] {
            let mut g = shg(p, &cells, 4, 3);
            let mut a = AlbersParameters::shg();
            a.units = units.to_vec();
            g.projection = GridProjection::Albers(a);
            assert!(matches!(
                write(&f, v, &g, &opts()),
                Err(WriteError::BadInput(_))
            ));
        }
        for null in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            let mut g = specified(p, &cells, 4, 3);
            if let GridProjection::Specified(s) = &mut g.projection {
                s.null_value = null;
            }
            assert!(matches!(
                write(&f, v, &g, &opts()),
                Err(WriteError::BadInput(_))
            ));
        }
        assert_eq!(std::fs::read(&f).unwrap(), before);
    }
    let _ = std::fs::remove_dir_all(&d);
}

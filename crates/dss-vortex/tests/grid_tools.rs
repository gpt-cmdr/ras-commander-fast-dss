//! The grid tools, with no oracle at run time, so CI on every platform checks the same rules and bits.
//!
//! The first part writes small DSS files and states each rule, including each observed Vortex
//! behaviour we did not implement. The second compares `fixtures/public/vortex_p53/expected.json`,
//! which holds what Vortex 1.1.0 wrote from its own test resources (`oracle/vortex/make_p53_fixture.py`).

use dss_core::dss6::write::{create_dss6, write_grid6};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, write_grid, AlbersParameters, FaultPoint, GridProjection, SpecifiedReference,
    WriteOptions, WriteStep,
};
use dss_core::dss7::Dss7File;
use dss_core::grid::Grid;
use dss_core::grid_write::{GridCompression, GridInput, GRID_NODATA, INST_VAL, PER_AVER, PER_CUM};
use dss_vortex::crs::Envelope;
use dss_vortex::grid_tools::java_order::hash_set_order;
use dss_vortex::grid_tools::{
    calculate, calculate_raster, clip, normalize, sanitize, ClipBounds, ClipRequest,
    NormalizeRequest, Operation, OutputOptions, RasterRequest, Thresholds,
};
use dss_vortex::import::{Failure, ImportError};
use dss_vortex::warp::{Align, Method};
use dss_vortex::{Crs, DataType, NamedCrs, VortexError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const NX: usize = 3;
const NY: usize = 2;
const LL: (i32, i32) = (-1000, 1000);
const M: f32 = GRID_NODATA;
const H0: &str = "01JAN2017:0000/01JAN2017:0100";
const H1: &str = "01JAN2017:0100/01JAN2017:0200";
const SHG_PRJ: &str = "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]";
const GEO_WKT: &str = "GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";

fn fresh_dir(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("grid_tools")
        .join(format!("{name}.{}", NEXT.fetch_add(1, Ordering::SeqCst)));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

struct Put<'a> {
    pathname: &'a str,
    cells: &'a [f32],
    units: &'a str,
    data_type: i32,
    ll: (i32, i32),
    nx: usize,
    ny: usize,
    projection: GridProjection,
}

fn shg<'a>(pathname: &'a str, cells: &'a [f32]) -> Put<'a> {
    Put {
        pathname,
        cells,
        units: "MM",
        data_type: PER_CUM,
        ll: LL,
        nx: NX,
        ny: NY,
        projection: GridProjection::Albers(AlbersParameters::shg()),
    }
}

fn put(path: &Path, version: u8, record: Put) {
    if !path.exists() {
        if version == 6 {
            create_dss6(path).unwrap();
        } else {
            create_dss7(path).unwrap();
        }
    }
    let input = GridInput {
        pathname: record.pathname.as_bytes(),
        projection: record.projection,
        time_stamped: true,
        data: record.cells,
        nx: record.nx,
        ny: record.ny,
        lower_left_cell_x: record.ll.0,
        lower_left_cell_y: record.ll.1,
        cell_size: 2000.0,
        x_coord_of_grid_cell_zero: 0.0,
        y_coord_of_grid_cell_zero: 0.0,
        units: record.units.as_bytes(),
        data_type: record.data_type,
        compression: GridCompression::Zlib,
    };
    if version == 6 {
        write_grid6(path, &input, &WriteOptions::default()).unwrap();
    } else {
        write_grid(path, &input, &WriteOptions::default()).unwrap();
    }
}

fn path_of(_: &Path, c: &str, window: &str) -> String {
    format!("///{c}/{window}//")
}

fn read(path: &Path) -> BTreeMap<String, Grid> {
    let mut out = BTreeMap::new();
    let head = fs::read(path).unwrap();
    if dss_core::FileSignature::from_bytes(&head).unwrap().version == dss_core::DssVersion::V6 {
        let f = Dss6File::open(path).unwrap();
        for e in f.catalog().unwrap().entries.iter().filter(|e| e.is_live()) {
            let name = String::from_utf8_lossy(&e.pathname).into_owned();
            match f.read_grid(&e.pathname) {
                Ok(g) => {
                    out.insert(name, g);
                }
                Err(dss_core::GridError::NotFound) => {} // Stopped before it was published.
                Err(other) => panic!("{other}"),
            }
        }
    } else {
        let mut f = Dss7File::open(path).unwrap();
        let entries: Vec<_> = f.catalog().unwrap().catalog.entries;
        for e in entries.iter().filter(|e| e.is_live()) {
            let name = String::from_utf8_lossy(&e.pathname).into_owned();
            match f.read_grid(&e.pathname) {
                Ok(g) => {
                    out.insert(name, g);
                }
                Err(dss_core::GridError::NotFound) => {} // Stopped before it was published.
                Err(other) => panic!("{other}"),
            }
        }
    }
    out
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn refused(result: Result<impl std::fmt::Debug, ImportError>, text: &str) -> ImportError {
    let e = result.expect_err("expected a refusal");
    assert!(e.to_string().contains(text), "{e}");
    assert!(
        matches!(
            e.failure,
            Failure::Vortex(VortexError::Refused { .. } | VortexError::Unsupported { .. })
                | Failure::BadRequest(_)
        ),
        "{e}"
    );
    e
}

fn one_grid(path: &Path) -> Grid {
    let mut all = read(path);
    assert_eq!(all.len(), 1);
    all.pop_first().unwrap().1
}

// Missing, in every tool ------------------------------------------------------------------------

#[test]
fn constant_keeps_missing_cells_stamps_units_and_type() {
    let d = fresh_dir("constant");
    let input = d.join("in.dss");
    let pathname = path_of(&d, "PRECIPITATION", H0);
    put(&input, 7, shg(&pathname, &[1.0, M, 3.0, 4.0, 5.0, 6.5]));
    let out = d.join("out.dss");
    let records = calculate(
        &input,
        None,
        Operation::Multiply,
        0.5,
        &OutputOptions::new(&out),
    )
    .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].imported.record.pathname, pathname);
    assert_eq!(records[0].source_units, "MM");
    assert_eq!(records[0].source_data_type, Some(DataType::PerCum));
    assert!(!records[0].converted);
    let g = one_grid(&out);
    assert_eq!(bits(&g.data), bits(&[0.5, M, 1.5, 2.0, 2.5, 3.25]));
    assert_eq!((g.data_units.as_str(), g.data_type), ("MM", PER_CUM));
    assert_eq!((g.lower_left_cell_x, g.lower_left_cell_y), LL);
}

#[test]
fn old_markers_in_a_dss_input_stay_missing() {
    // Observed behaviour we did not implement: Vortex computes on -901 and -902 as numbers.
    let d = fresh_dir("old_markers");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(&input, 7, shg(&p, &[1.0, -901.0, -902.0, 4.0, 5.0, 6.0]));
    let out = d.join("out.dss");
    calculate(
        &input,
        None,
        Operation::Multiply,
        2.0,
        &OutputOptions::new(&out),
    )
    .unwrap();
    assert_eq!(
        bits(&one_grid(&out).data),
        bits(&[2.0, M, M, 8.0, 10.0, 12.0])
    );
}

#[test]
fn a_specified_grids_null_value_stays_missing() {
    // Observed behaviour we did not implement: Vortex computes on it as a number.
    let d = fresh_dir("null_value");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(
        &input,
        7,
        Put {
            projection: GridProjection::Specified(SpecifiedReference {
                srs_name: b"NAD83".to_vec(),
                srs_definition: GEO_WKT.as_bytes().to_vec(),
                srs_definition_type: 0,
                null_value: -9999.0,
                time_zone_id: b"UTC".to_vec(),
                time_zone_raw_offset: 0,
                is_interval: true,
            }),
            ..shg(&p, &[1.0, -9999.0, 3.0, 4.0, 5.0, 6.0])
        },
    );
    let out = d.join("out.dss");
    calculate(
        &input,
        None,
        Operation::Multiply,
        2.0,
        &OutputOptions::new(&out),
    )
    .unwrap();
    assert_eq!(
        bits(&one_grid(&out).data),
        bits(&[2.0, M, 6.0, 8.0, 10.0, 12.0])
    );
}

fn asc(d: &Path, name: &str, rows: &str, marker: f32) -> PathBuf {
    let p = d.join(name);
    let text = format!(
        "ncols {NX}\nnrows {NY}\nxllcorner {}\nyllcorner {}\ncellsize 2000\nNODATA_value {marker}\n{rows}\n",
        LL.0 * 2000,
        LL.1 * 2000
    );
    fs::write(&p, text).unwrap();
    fs::write(p.with_extension("prj"), SHG_PRJ).unwrap();
    p
}

#[test]
fn raster_calculator_never_uses_a_marker_as_a_number() {
    // Observed behaviour we did not implement: `GridCalculator` applies the operation to both markers.
    let d = fresh_dir("raster");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    // South to north: the DSS row 0 is the raster's last row.
    put(&input, 7, shg(&p, &[1.0, M, 3.0, 4.0, 5.0, 6.0]));
    let raster = asc(&d, "r.asc", "0.5 0.5 0.5\n0.5 0.5 -9999", -9999.0);
    let out = d.join("out.dss");
    let request = RasterRequest {
        raster,
        raster_variable: None,
        operation: Operation::Multiply,
        method: Method::Near,
        static_raster: false,
    };
    calculate_raster(&input, None, &request, &OutputOptions::new(&out)).unwrap();
    assert_eq!(
        bits(&one_grid(&out).data),
        bits(&[0.5, M, M, 2.0, 2.5, 3.0])
    );
}

#[test]
fn a_raster_that_does_not_land_on_the_grid_is_refused() {
    let d = fresh_dir("raster_grid");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(&input, 7, shg(&p, &[1.0; 6]));
    // Half the cell size: the same extent, twice the cells each way.
    let raster = d.join("fine.asc");
    fs::write(
        &raster,
        format!(
            "ncols 6\nnrows 4\nxllcorner {}\nyllcorner {}\ncellsize 1000\nNODATA_value -9999\n{}",
            LL.0 * 2000,
            LL.1 * 2000,
            "1 1 1 1 1 1\n".repeat(4)
        ),
    )
    .unwrap();
    fs::write(raster.with_extension("prj"), SHG_PRJ).unwrap();
    let out = d.join("out.dss");
    let request = RasterRequest {
        raster,
        raster_variable: None,
        operation: Operation::Add,
        method: Method::Near,
        static_raster: false,
    };
    refused(
        calculate_raster(&input, None, &request, &OutputOptions::new(&out)),
        "resamples to",
    );
    assert!(!out.exists());
}

#[test]
fn raster_calculator_refuses_grids_in_two_places() {
    // Observed behaviour we did not implement: Vortex resamples for the first and applies it to all.
    let d = fresh_dir("two_places");
    let input = d.join("in.dss");
    let raster = asc(&d, "r.asc", "1 1 1\n1 1 1", -9999.0);
    put(&input, 7, shg(&path_of(&d, "PRECIPITATION", H0), &[1.0; 6]));
    let moved = path_of(&d, "PRECIPITATION", H1);
    put(
        &input,
        7,
        Put {
            ll: (LL.0 + 1, LL.1),
            ..shg(&moved, &[1.0; 6])
        },
    );
    let out = d.join("out.dss");
    let request = RasterRequest {
        raster,
        raster_variable: None,
        operation: Operation::Add,
        method: Method::Near,
        static_raster: false,
    };
    refused(
        calculate_raster(&input, None, &request, &OutputOptions::new(&out)),
        "not all on one grid",
    );
    assert!(!out.exists());
}

#[test]
fn sanitizer_replaces_only_valid_cells_inclusively() {
    let d = fresh_dir("sanitize");
    let input = d.join("in.dss");
    put(
        &input,
        7,
        shg(
            &path_of(&d, "PRECIPITATION", H0),
            &[0.0, M, 1.0, 2.0, 3.0, 4.0],
        ),
    );
    let out = d.join("out.dss");
    let t = Thresholds {
        minimum: 1.0,
        maximum: 3.0,
        minimum_replacement: 7.0,
        maximum_replacement: 8.0,
    };
    sanitize(&input, None, t, &OutputOptions::new(&out)).unwrap();
    // Observed behaviour we did not implement: Vortex makes the marker 7, being below the minimum.
    assert_eq!(
        bits(&one_grid(&out).data),
        bits(&[7.0, M, 7.0, 2.0, 8.0, 8.0])
    );
    let nan = d.join("nan.dss");
    let t = Thresholds {
        minimum: f64::NAN,
        ..Thresholds::default()
    };
    refused(sanitize(&input, None, t, &OutputOptions::new(&nan)), "NaN");
    let t = Thresholds {
        minimum_replacement: f32::INFINITY,
        ..Thresholds::default()
    };
    refused(
        sanitize(&input, None, t, &OutputOptions::new(&nan)),
        "infinite",
    );
    assert!(!nan.exists());
}

// Normalizer -------------------------------------------------------------------------------------

fn julian(day: u32) -> i64 {
    dss_core::time::ymd_to_julian(2017, 1, day)
}

fn two_hours(normals: PathBuf) -> NormalizeRequest {
    NormalizeRequest {
        normals,
        source_variables: None,
        normals_variables: None,
        start: (julian(1), 0),
        end: (julian(1), 7200),
        interval_seconds: 7200,
    }
}

const WINDOW: &str = "01JAN2017:0000/01JAN2017:0200";

#[test]
fn normalized_precipitation_keeps_its_stamps_and_type() {
    let d = fresh_dir("normalize");
    let input = d.join("in.dss");
    let normals = d.join("normals.dss");
    let (a, b) = (
        path_of(&d, "PRECIPITATION", H0),
        path_of(&d, "PRECIPITATION", H1),
    );
    put(&input, 7, shg(&a, &[1.0, 2.0, 3.0, 4.0, 0.0, M]));
    put(&input, 7, shg(&b, &[3.0, 2.0, 3.0, 4.0, 0.0, 5.0]));
    // Cell 2's normal is missing, so that cell is written as missing in both frames.
    put(
        &normals,
        7,
        shg(
            &path_of(&d, "PRECIPITATION", WINDOW),
            &[8.0, 2.0, M, 4.0, 9.0, 10.0],
        ),
    );
    let out = d.join("out.dss");
    let outcome = normalize(&input, &two_hours(normals), &OutputOptions::new(&out)).unwrap();
    assert_eq!(outcome.records.len(), 2);
    assert_eq!(outcome.intervals.len(), 1);
    assert!(outcome.sources_left_out.is_empty());
    let all = read(&out);
    let g = &all[&a];
    assert_eq!((g.data_units.as_str(), g.data_type), ("MM", PER_CUM));
    assert_eq!(
        (g.start_time.is_some(), g.end_time.is_some()),
        (true, true),
        "a period stays a period"
    );
    // Total of cell 5 leaves the missing cell out, as NaN is left out there; the input stays missing.
    assert_eq!(bits(&g.data), bits(&[2.0, 1.0, M, 2.0, 0.0, M]));
    assert_eq!(bits(&all[&b].data), bits(&[6.0, 1.0, M, 2.0, 0.0, 10.0]));
    assert_eq!(outcome.cells_made_missing_by_normal, 2);
}

#[test]
fn normalizer_refuses_what_vortex_combines_or_skips() {
    let d = fresh_dir("normalize_refuse");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(&input, 7, shg(&p, &[1.0; 6]));
    let normal = path_of(&d, "PRECIPITATION", WINDOW);
    let out = d.join("out.dss");

    let moved = d.join("moved.dss");
    put(
        &moved,
        7,
        Put {
            ll: (LL.0, LL.1 + 1),
            ..shg(&normal, &[1.0; 6])
        },
    );
    refused(
        normalize(&input, &two_hours(moved), &OutputOptions::new(&out)),
        "compares only the size",
    );

    let inches = d.join("inches.dss");
    put(
        &inches,
        7,
        Put {
            units: "IN",
            ..shg(&normal, &[1.0; 6])
        },
    );
    refused(
        normalize(&input, &two_hours(inches), &OutputOptions::new(&out)),
        "a scale needs one unit",
    );

    let shape = d.join("shape.dss");
    put(
        &shape,
        7,
        Put {
            nx: 2,
            ny: 3,
            ..shg(&normal, &[1.0; 6])
        },
    );
    refused(
        normalize(&input, &two_hours(shape), &OutputOptions::new(&out)),
        "compares only the size",
    );

    let other_crs = d.join("other_crs.dss");
    geographic(&normal, &[1.0; 6], &other_crs);
    refused(
        normalize(&input, &two_hours(other_crs), &OutputOptions::new(&out)),
        "compares only the size",
    );

    let wide = d.join("wide.dss");
    put(
        &wide,
        7,
        shg(
            &path_of(&d, "PRECIPITATION", "01JAN2017:0000/01JAN2017:0300"),
            &[1.0; 6],
        ),
    );
    refused(
        normalize(&input, &two_hours(wide.clone()), &OutputOptions::new(&out)),
        "no normals",
    );

    let mut late = two_hours(wide);
    late.start = (julian(5), 0);
    late.end = (julian(6), 0);
    refused(
        normalize(&input, &late, &OutputOptions::new(&out)),
        "no source grid lies",
    );
    assert!(!out.exists(), "a refusal creates no file");
}

#[test]
fn an_instant_on_an_interval_edge_is_refused_not_overwritten() {
    let d = fresh_dir("edge");
    let input = d.join("in.dss");
    let snow = |t: &str| format!("///SWE/{t}///");
    let instant = snow("01JAN2017:0200");
    put(
        &input,
        7,
        Put {
            data_type: INST_VAL,
            ..shg(&instant, &[1.0; 6])
        },
    );
    let normals = d.join("normals.dss");
    for t in ["01JAN2017:0100", "01JAN2017:0300"] {
        let name = snow(t);
        put(
            &normals,
            7,
            Put {
                data_type: INST_VAL,
                ..shg(&name, &[2.0; 6])
            },
        );
    }
    let mut request = two_hours(normals);
    request.end = (julian(1), 14400);
    let out = d.join("out.dss");
    refused(
        normalize(&input, &request, &OutputOptions::new(&out)),
        "written twice",
    );
    assert!(!out.exists());
}

#[test]
fn the_order_of_a_float_sum_is_vortexs() {
    // The fixture's order, as `DataReader.getVariables` printed it on Vortex 1.1.0 and 0.13.3.
    let e = expected();
    for key in ["source_order", "normals_order"] {
        let want: Vec<String> = e["java_order"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(want.len() >= 2);
        let catalog = dss_vortex::grid_tools::DssSource::open(&fixture(
            e["java_order"][format!("{key}_file")].as_str().unwrap(),
        ))
        .unwrap()
        .pathnames();
        assert_eq!(hash_set_order(&catalog).0, want, "{key}");
    }
}

// Clipper ---------------------------------------------------------------------------------------

fn envelope_on_cells() -> ClipRequest {
    let (x, y) = (f64::from(LL.0) * 2000.0, f64::from(LL.1) * 2000.0);
    ClipRequest {
        bounds: ClipBounds::Envelope {
            envelope: Envelope {
                min_x: x + 2000.0,
                max_x: x + 6000.0,
                min_y: y,
                max_y: y + 2000.0,
            },
            crs: Crs::Named(NamedCrs::Shg),
        },
    }
}

#[test]
fn clip_takes_whole_cells_and_refuses_an_extent_off_them() {
    let d = fresh_dir("clip");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(&input, 7, shg(&p, &[1.0, 2.0, M, 4.0, 5.0, 6.0]));
    let out = d.join("out.dss");
    clip(
        &input,
        None,
        &envelope_on_cells(),
        &OutputOptions::new(&out),
    )
    .unwrap();
    let g = one_grid(&out);
    assert_eq!((g.nx, g.ny), (2, 1));
    assert_eq!((g.lower_left_cell_x, g.lower_left_cell_y), (LL.0 + 1, LL.1));
    assert_eq!(bits(&g.data), bits(&[2.0, M]));
    assert_eq!((g.data_units.as_str(), g.data_type), ("MM", PER_CUM));

    // Off whole cells: refused by default (V6), and Vortex's record with `align`.
    let (x, y) = (f64::from(LL.0) * 2000.0, f64::from(LL.1) * 2000.0);
    let off = ClipRequest {
        bounds: ClipBounds::Envelope {
            envelope: Envelope {
                min_x: x + 2100.0,
                max_x: x + 6100.0,
                min_y: y + 100.0,
                max_y: y + 2100.0,
            },
            crs: Crs::Named(NamedCrs::Shg),
        },
    };
    let refused_out = d.join("refused.dss");
    refused(
        clip(&input, None, &off, &OutputOptions::new(&refused_out)),
        "whole cells",
    );
    assert!(!refused_out.exists());
    let aligned = d.join("aligned.dss");
    let options = OutputOptions {
        align: Align::Vortex,
        ..OutputOptions::new(&aligned)
    };
    clip(&input, None, &off, &options).unwrap();
    assert_eq!(one_grid(&aligned).nx, 2);
}

fn geographic(pathname: &str, cells: &[f32], path: &Path) {
    put(
        path,
        7,
        Put {
            projection: GridProjection::Specified(SpecifiedReference {
                srs_name: b"NAD83".to_vec(),
                srs_definition: GEO_WKT.as_bytes().to_vec(),
                srs_definition_type: 0,
                null_value: GRID_NODATA,
                time_zone_id: b"UTC".to_vec(),
                time_zone_raw_offset: 0,
                is_interval: true,
            }),
            ..shg(pathname, cells)
        },
    );
}

#[test]
fn clip_never_guesses_a_crs() {
    let d = fresh_dir("clip_crs");
    let input = d.join("in.dss");
    put(&input, 7, shg(&path_of(&d, "PRECIPITATION", H0), &[1.0; 6]));
    let out = d.join("out.dss");
    // A shapefile with no .prj: Vortex 1.1.0 and 0.13.3 fail on it, on a null reference.
    let bare = d.join("bare.shp");
    fs::write(&bare, [0u8; 100]).unwrap();
    let request = ClipRequest {
        bounds: ClipBounds::Shapefile(bare),
    };
    refused(
        clip(&input, None, &request, &OutputOptions::new(&out)),
        "no .prj",
    );
    let absent = ClipRequest {
        bounds: ClipBounds::Shapefile(d.join("absent.shp")),
    };
    refused(
        clip(&input, None, &absent, &OutputOptions::new(&out)),
        "does not exist",
    );
    let undefined = ClipRequest {
        bounds: ClipBounds::Envelope {
            envelope: Envelope {
                min_x: 0.0,
                max_x: 1.0,
                min_y: 0.0,
                max_y: 1.0,
            },
            crs: Crs::Undefined,
        },
    };
    assert!(clip(&input, None, &undefined, &OutputOptions::new(&out)).is_err());
    // A grid in a geographic CRS: Vortex logs it and writes nothing.
    let geo = d.join("geo.dss");
    geographic(&path_of(&d, "PRECIPITATION", H0), &[1.0; 6], &geo);
    refused(
        clip(&geo, None, &envelope_on_cells(), &OutputOptions::new(&out)),
        "geographic",
    );
    assert!(!out.exists());
}

// Records ----------------------------------------------------------------------------------------

#[test]
fn a_change_of_data_type_must_be_stated() {
    let d = fresh_dir("types");
    let input = d.join("in.dss");
    let p = "/A/B/ZZTOTAL/01JAN2017:0000/01JAN2017:0100/F1/".to_string();
    put(&input, 7, shg(&p, &[1.0; 6]));
    let out = d.join("out.dss");
    refused(
        calculate(&input, None, Operation::Add, 1.0, &OutputOptions::new(&out)),
        "State data_type",
    );
    assert!(!out.exists());
    let options = OutputOptions {
        data_type: Some(DataType::PerAver),
        part_f: "*".into(),
        ..OutputOptions::new(&out)
    };
    let r = calculate(&input, None, Operation::Add, 1.0, &options).unwrap();
    // A and B stay empty, as in Vortex. `*` keeps a DSS input's F part, and only that.
    assert_eq!(
        r[0].imported.record.pathname,
        "///ZZTOTAL/01JAN2017:0000/01JAN2017:0100/F1/"
    );
    assert_eq!(one_grid(&out).data_type, PER_AVER);
    assert_eq!(r[0].source_data_type, Some(DataType::PerCum));
}

#[test]
fn a_conversion_is_stated_on_the_record() {
    let d = fresh_dir("kelvin");
    let input = d.join("in.dss");
    let p = path_of(&d, "TEMPERATURE", H0);
    put(
        &input,
        7,
        Put {
            units: "K",
            data_type: PER_AVER,
            ..shg(&p, &[300.0; 6])
        },
    );
    let out = d.join("out.dss");
    let r = calculate(&input, None, Operation::Add, 1.0, &OutputOptions::new(&out)).unwrap();
    assert!(r[0].converted);
    assert_eq!(r[0].source_units, "K");
    assert_eq!(r[0].imported.record.units, "DEG C");
    assert_eq!(one_grid(&out).data[0], 301.0f32 + -273.15f32);
}

#[test]
fn requests_that_name_nothing_are_refused() {
    let d = fresh_dir("names");
    let input = d.join("in.dss");
    put(&input, 7, shg(&path_of(&d, "PRECIPITATION", H0), &[1.0; 6]));
    let out = d.join("out.dss");
    let o = OutputOptions::new(&out);
    let absent = vec!["///PRECIPITATION/09JAN2017:0000/09JAN2017:0100//".to_string()];
    refused(
        calculate(&input, Some(&absent), Operation::Add, 1.0, &o),
        "does not hold it",
    );
    refused(
        calculate(&input, Some(&[]), Operation::Add, 1.0, &o),
        "holds no grids",
    );
    refused(
        calculate(&input, None, Operation::Add, f64::NAN, &o),
        "must be finite",
    );
    refused(
        calculate(&input, None, Operation::Add, 1e300, &o),
        "must be finite",
    );
    refused(
        calculate(&d.join("absent.dss"), None, Operation::Add, 1.0, &o),
        "does not exist",
    );
    // One file with no grid records.
    let empty = d.join("empty.dss");
    create_dss7(&empty).unwrap();
    refused(
        calculate(&empty, None, Operation::Add, 1.0, &o),
        "holds no grids",
    );
    // A short or strange input is a typed error, not a panic.
    let junk = d.join("junk.dss");
    fs::write(&junk, b"ZDSS").unwrap();
    assert!(calculate(&junk, None, Operation::Add, 1.0, &o).is_err());
    assert!(!out.exists());
}

// Files ------------------------------------------------------------------------------------------

#[test]
fn a_refused_value_leaves_the_output_as_it_was() {
    let d = fresh_dir("value");
    let input = d.join("in.dss");
    // The second record's result is what refuses: the first must not have been written.
    let (a, b) = (
        path_of(&d, "PRECIPITATION", H0),
        path_of(&d, "PRECIPITATION", H1),
    );
    put(&input, 7, shg(&a, &[1.0; 6]));
    put(&input, 7, shg(&b, &[1.0, 1.0, f32::MAX, 1.0, 1.0, 1.0]));
    let new = d.join("new.dss");
    let e = refused(
        calculate(
            &input,
            None,
            Operation::Multiply,
            2.0,
            &OutputOptions::new(&new),
        ),
        "cannot hold",
    );
    assert_eq!(e.phase, "planning");
    assert!(e.completed.is_empty());
    assert!(!new.exists());

    let existing = d.join("existing.dss");
    let other = path_of(&d, "OTHER", H0);
    put(
        &existing,
        7,
        Put {
            data_type: PER_AVER,
            ..shg(&other, &[9.0; 6])
        },
    );
    let before = fs::read(&existing).unwrap();
    refused(
        calculate(
            &input,
            None,
            Operation::Multiply,
            2.0,
            &OutputOptions::new(&existing),
        ),
        "cannot hold",
    );
    assert_eq!(fs::read(&existing).unwrap(), before);
}

#[test]
fn the_input_cannot_be_the_output() {
    let d = fresh_dir("same");
    let input = d.join("in.dss");
    put(&input, 7, shg(&path_of(&d, "PRECIPITATION", H0), &[1.0; 6]));
    let before = fs::read(&input).unwrap();
    refused(
        calculate(
            &input,
            None,
            Operation::Add,
            1.0,
            &OutputOptions::new(&input),
        ),
        "also the input",
    );
    assert_eq!(fs::read(&input).unwrap(), before);
}

#[test]
fn other_records_survive_and_a_replace_replaces() {
    let d = fresh_dir("existing");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(&input, 7, shg(&p, &[1.0; 6]));
    let out = d.join("out.dss");
    let other = path_of(&d, "OTHER", H0);
    put(
        &out,
        7,
        Put {
            data_type: PER_AVER,
            ..shg(&other, &[9.0; 6])
        },
    );
    let o = OutputOptions::new(&out);
    let first = calculate(&input, None, Operation::Add, 1.0, &o).unwrap();
    assert_eq!(first[0].imported.record.outcome, "added");
    let second = calculate(&input, None, Operation::Add, 2.0, &o).unwrap();
    assert_eq!(second[0].imported.record.outcome, "replaced");
    let all = read(&out);
    assert_eq!(all.len(), 2);
    assert_eq!(all[&p].data, [3.0; 6]);
    assert_eq!(all[&other].data, [9.0; 6]);
}

#[test]
fn crash_at_every_write_step_leaves_whole_records_and_a_rerun_heals() {
    for version in [7u8, 6] {
        let d = fresh_dir("crash");
        let input = d.join("in.dss");
        let names = [
            path_of(&d, "PRECIPITATION", H0),
            path_of(&d, "PRECIPITATION", H1),
            path_of(&d, "PRECIPITATION", "01JAN2017:0200/01JAN2017:0300"),
        ];
        for (i, name) in names.iter().enumerate() {
            put(&input, 7, shg(name, &[i as f32 + 1.0; 6]));
        }
        let clean = d.join("clean.dss");
        let options = |out: &Path| OutputOptions {
            version: Some(version),
            ..OutputOptions::new(out)
        };
        calculate(&input, None, Operation::Add, 1.0, &options(&clean)).unwrap();
        let reference = read(&clean);
        assert_eq!(reference.len(), 3);
        let mut stopped = 0;
        for (i, step) in WriteStep::ALL.into_iter().enumerate() {
            let out = d.join(format!("crash{i}.dss"));
            let mut o = options(&out);
            o.fault = Some((1, FaultPoint { block: 0, step }));
            let Err(e) = calculate(&input, None, Operation::Add, 1.0, &o) else {
                continue; // This step is not on this write's path.
            };
            stopped += 1;
            assert_eq!(e.phase, "writing");
            assert_eq!(e.completed.len(), 1);
            // The second record was being written. It may be visible, whole, so `completed` alone
            // does not say what the file holds: every visible record is acknowledged or in doubt.
            let in_doubt = e.in_doubt.clone().expect("the failed record is in doubt");
            let (acknowledged, doubted) = (&e.completed[0].record.pathname, &names[1]);
            let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
            assert!(same(&in_doubt, doubted), "{in_doubt}");
            assert!(e.to_string().contains("may or may not"), "{e}");
            let after = read(&out);
            assert!(after.len() <= 2, "{step:?}");
            for (name, grid) in &after {
                assert!(
                    same(name, acknowledged) || same(name, &in_doubt),
                    "{step:?}: {name} is neither acknowledged nor in doubt"
                );
                assert_eq!(bits(&grid.data), bits(&reference[name].data), "{step:?}");
            }
            o.fault = None;
            calculate(&input, None, Operation::Add, 1.0, &o).unwrap();
            let healed = read(&out);
            assert_eq!(healed.len(), 3, "{step:?}");
            for (name, grid) in &healed {
                assert_eq!(bits(&grid.data), bits(&reference[name].data), "{step:?}");
            }
        }
        assert!(stopped >= 5, "only {stopped} steps stopped a write");
    }
}

#[test]
fn dss6_reads_and_writes_and_a_file_keeps_its_version() {
    let d = fresh_dir("dss6");
    let input = d.join("in.dss");
    let p = path_of(&d, "PRECIPITATION", H0);
    put(&input, 6, shg(&p, &[1.0, M, 3.0, 4.0, 5.0, 6.0]));
    let out = d.join("out.dss");
    let o = OutputOptions {
        version: Some(6),
        ..OutputOptions::new(&out)
    };
    calculate(&input, None, Operation::Multiply, 2.0, &o).unwrap();
    assert_eq!(
        bits(&one_grid(&out).data),
        bits(&[2.0, M, 6.0, 8.0, 10.0, 12.0])
    );
    let asked = OutputOptions {
        version: Some(7),
        ..OutputOptions::new(&out)
    };
    refused(
        calculate(&input, None, Operation::Add, 1.0, &asked),
        "Writers keep a file's version",
    );
}

// Vortex's records, with no oracle ---------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/vortex_p53")
        .join(name)
}

fn expected() -> Value {
    serde_json::from_str(&fs::read_to_string(fixture("expected.json")).unwrap()).unwrap()
}

fn digest(cells: &[f32]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in cells {
        for b in c.to_bits().to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

fn operation(name: &str) -> Operation {
    match name {
        "multiply" => Operation::Multiply,
        "divide" => Operation::Divide,
        "add" => Operation::Add,
        _ => Operation::Subtract,
    }
}

fn run_case(case: &Value, out: &Path) {
    let f = case["write_f"].as_str().unwrap_or_default().to_string();
    let part = |key: &str| case[key].as_str().unwrap_or_default().to_string();
    let options = OutputOptions {
        part_a: part("write_a"),
        part_b: part("write_b"),
        part_f: f,
        align: if case["align"].as_str() == Some("vortex") {
            Align::Vortex
        } else {
            Align::Refuse
        },
        ..OutputOptions::new(out)
    };
    let input = fixture(case["input"].as_str().unwrap());
    let variables: Option<Vec<String>> = case["variables"].as_array().map(|v| {
        v.iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    });
    let num = |k: &str| case[k].as_f64();
    let time = |k: &str| {
        let t: Vec<i64> = case[k]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        (
            dss_core::time::ymd_to_julian(t[0], t[1] as u32, t[2] as u32),
            t[3] * 3600 + t[4] * 60,
        )
    };
    match case["tool"].as_str().unwrap() {
        "calc" => {
            calculate(
                &input,
                variables.as_deref(),
                operation(case["operation"].as_str().unwrap()),
                num("value").unwrap(),
                &options,
            )
            .unwrap();
        }
        "rcalc" => {
            let request = RasterRequest {
                raster: fixture(case["raster"].as_str().unwrap()),
                raster_variable: None,
                operation: operation(case["operation"].as_str().unwrap()),
                method: match case["method"].as_str().unwrap() {
                    "near" => Method::Near,
                    "bilinear" => Method::Bilinear,
                    _ => Method::Average,
                },
                static_raster: false,
            };
            calculate_raster(&input, variables.as_deref(), &request, &options).unwrap();
        }
        "san" => {
            let d = Thresholds::default();
            let t = Thresholds {
                minimum: num("minimum").unwrap_or(d.minimum),
                maximum: num("maximum").unwrap_or(d.maximum),
                minimum_replacement: num("minimum_replacement").map_or(f32::NAN, |v| v as f32),
                maximum_replacement: num("maximum_replacement").map_or(f32::NAN, |v| v as f32),
            };
            sanitize(&input, variables.as_deref(), t, &options).unwrap();
        }
        "norm" => {
            let list = |k: &str| {
                case[k]
                    .as_array()
                    .map(|v| v.iter().map(|s| s.as_str().unwrap().to_string()).collect())
            };
            let request = NormalizeRequest {
                normals: fixture(case["normals"].as_str().unwrap()),
                source_variables: list("source_variables"),
                normals_variables: list("normals_variables"),
                start: time("start"),
                end: time("end"),
                interval_seconds: case["interval"].as_i64().unwrap(),
            };
            normalize(&input, &request, &options).unwrap();
        }
        "clip" => {
            let b = case["envelope"].as_array().unwrap();
            let v = |i: usize| b[i].as_f64().unwrap();
            let request = ClipRequest {
                bounds: ClipBounds::Envelope {
                    envelope: Envelope {
                        min_x: v(0),
                        min_y: v(1),
                        max_x: v(2),
                        max_y: v(3),
                    },
                    crs: Crs::Wkt(case["envelope_crs"].as_str().unwrap().to_string()),
                },
            };
            clip(&input, variables.as_deref(), &request, &options).unwrap();
        }
        other => panic!("{other}"),
    }
}

fn hex(v: f32) -> u32 {
    v.to_bits()
}

// Every difference between one file and the fixture's records. Empty means identical.
fn differences(case: &Value, grids: &BTreeMap<String, Grid>) -> Vec<String> {
    let want = case["records"].as_array().unwrap();
    let mut out = Vec::new();
    if want.is_empty() {
        out.push("the case lists no record".to_string());
    }
    if grids.len() != want.len() {
        out.push(format!("{} records for {}", grids.len(), want.len()));
    }
    for w in want {
        let name = w["pathname"].as_str().unwrap();
        let Some(g) = grids.get(name) else {
            out.push(format!("no {name}"));
            continue;
        };
        let u = |k: &str| w[k].as_u64().unwrap();
        let i = |k: &str| w[k].as_i64().unwrap();
        let checks = [
            ("record_type", i64::from(g.record_type), i("record_type")),
            ("data_type", i64::from(g.data_type), i("data_type")),
            ("nx", g.nx as i64, i("nx")),
            ("ny", g.ny as i64, i("ny")),
            ("llx", i64::from(g.lower_left_cell_x), i("llx")),
            ("lly", i64::from(g.lower_left_cell_y), i("lly")),
        ];
        for (key, have, expect) in checks {
            if have != expect {
                out.push(format!("{name}: {key} {have}, expected {expect}"));
            }
        }
        if g.data_units != w["units"].as_str().unwrap() {
            out.push(format!("{name}: units {}", g.data_units));
        }
        if u64::from(hex(g.cell_size)) != u("cell_bits") {
            out.push(format!("{name}: cell size {}", g.cell_size));
        }
        for (key, have) in [
            ("min_bits", g.min_data_value),
            ("max_bits", g.max_data_value),
            ("mean_bits", g.mean_data_value),
        ] {
            if have.map(|v| u64::from(hex(v))) != w[key].as_u64() {
                out.push(format!("{name}: {key} {have:?}"));
            }
        }
        if g.data.len() as u64 != u("cells") {
            out.push(format!("{name}: {} cells", g.data.len()));
        }
        if digest(&g.data) != w["digest"].as_str().unwrap() {
            out.push(format!("{name}: cells differ"));
        }
    }
    out
}

#[test]
fn cases_match_vortex_1_1_0_by_the_bit() {
    let e = expected();
    let cases = e["cases"].as_array().unwrap();
    assert_eq!(cases.len(), e["case_count"].as_u64().unwrap() as usize);
    assert!(cases.len() >= 10);
    let mut records = 0;
    for case in cases {
        let d = fresh_dir("fixture");
        let out = d.join("out.dss");
        run_case(case, &out);
        let grids = read(&out);
        records += grids.len();
        let problems = differences(case, &grids);
        assert!(
            problems.is_empty(),
            "{}: {:?}",
            case["id"].as_str().unwrap(),
            &problems[..problems.len().min(5)]
        );
    }
    assert_eq!(records as u64, e["record_count"].as_u64().unwrap());
}

// The comparison above must be able to fail.
#[test]
fn the_fixture_comparison_can_fail() {
    let e = expected();
    let case = e["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["records"].as_array().unwrap().len() >= 2)
        .unwrap();
    let d = fresh_dir("controls");
    let out = d.join("out.dss");
    run_case(case, &out);
    let good = read(&out);
    assert!(differences(case, &good).is_empty());
    // One cell, by one bit.
    let mut one_bit = good.clone();
    let g = one_bit.values_mut().next().unwrap();
    g.data[0] = f32::from_bits(g.data[0].to_bits() ^ 1);
    assert!(differences(case, &one_bit)
        .iter()
        .any(|p| p.contains("cells differ")));
    // A missing record, an extra one, short cells, and a case listing nothing.
    let mut less = good.clone();
    less.pop_first();
    assert!(!differences(case, &less).is_empty());
    let mut more = good.clone();
    let any = more.values().next().unwrap().clone();
    more.insert("/EXTRA/".into(), any);
    assert!(differences(case, &more)
        .iter()
        .any(|p| p.contains("records for")));
    let mut short = good.clone();
    short.values_mut().next().unwrap().data.pop();
    assert!(differences(case, &short)
        .iter()
        .any(|p| p.contains("cells")));
    let mut none = case.clone();
    none["records"] = Value::Array(Vec::new());
    assert!(differences(&none, &BTreeMap::new())
        .iter()
        .any(|p| p.contains("lists no record")));
    // Metadata: units, type, place, each statistic.
    for change in [
        (|g: &mut Grid| g.data_units = "IN".into()) as fn(&mut Grid),
        |g| g.data_type = 2,
        |g| g.lower_left_cell_x += 1,
        |g| g.min_data_value = Some(-1.0),
        |g| g.mean_data_value = None,
    ] {
        let mut changed = good.clone();
        change(changed.values_mut().next().unwrap());
        assert!(!differences(case, &changed).is_empty());
    }
}

// The order matters on these very records, so the port is not a formality.
#[test]
fn summing_in_time_order_gives_other_bits() {
    let e = expected();
    let names: Vec<String> = e["java_order"]["source_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let grids = read(&fixture(
        e["java_order"]["source_order_file"].as_str().unwrap(),
    ));
    let mut by_time = names.clone();
    by_time.sort();
    assert_ne!(by_time, names);
    let sum = |order: &[String]| {
        let mut total = vec![0f32; grids[&order[0]].data.len()];
        for n in order {
            for (t, c) in total.iter_mut().zip(&grids[n].data) {
                *t += *c;
            }
        }
        total
    };
    assert_ne!(bits(&sum(&names)), bits(&sum(&by_time)));
}

// Review findings: each of these failed before its fix ----------------------------------------

mod review_p53 {
    use super::*;
    use dss_vortex::crs::{CrsDef, DatumPolicy, Transformer};

    const P: &str = "///PRECIPITATION/01JAN2017:0000/01JAN2017:0100//";

    fn subtract_gives(result: f32) {
        let d = fresh_dir("reserved");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
        let existing = d.join("existing.dss");
        put(&existing, 7, shg(P, &[9.0; 6]));
        let before = fs::read(&existing).unwrap();
        let out = OutputOptions::new(&existing);
        refused(
            calculate(
                &input,
                None,
                Operation::Subtract,
                f64::from(1.0 - result),
                &out,
            ),
            "take as missing",
        );
        assert_eq!(fs::read(&existing).unwrap(), before);
    }

    #[test]
    fn a_result_the_reader_takes_as_missing_is_refused() {
        subtract_gives(-901.0);
        subtract_gives(-902.0);
    }

    #[test]
    fn every_valid_result_survives_a_round_trip_and_a_chain() {
        // Next to the reserved values: stored, and read back as values, through two operations.
        let d = fresh_dir("chain");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[-900.0, -903.0, -901.5, 1.0, 2.0, 3.0]));
        let first = d.join("first.dss");
        calculate(
            &input,
            None,
            Operation::Add,
            0.0,
            &OutputOptions::new(&first),
        )
        .unwrap();
        let grid = one_grid(&first);
        assert!(grid.data.iter().all(|v| !grid.is_missing(*v)));
        let second = d.join("second.dss");
        calculate(
            &first,
            None,
            Operation::Multiply,
            2.0,
            &OutputOptions::new(&second),
        )
        .unwrap();
        assert_eq!(
            bits(&one_grid(&second).data),
            bits(&[-1800.0, -1806.0, -1803.0, 2.0, 4.0, 6.0])
        );
        // A chain that reaches a reserved value stops there, and the first output is unchanged.
        let before = fs::read(&first).unwrap();
        let third = d.join("third.dss");
        refused(
            calculate(
                &first,
                None,
                Operation::Subtract,
                1.0,
                &OutputOptions::new(&third),
            ),
            "take as missing",
        );
        assert!(!third.exists());
        assert_eq!(fs::read(&first).unwrap(), before);
    }

    #[test]
    fn the_raster_calculator_refuses_a_reserved_result() {
        let d = fresh_dir("reserved_raster");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0; 6]));
        let raster = asc(&d, "r.asc", "902 0 0\n0 0 0", -9999.0);
        let request = RasterRequest {
            operation: Operation::Subtract,
            ..raster_request(raster)
        };
        let out = d.join("out.dss");
        refused(
            calculate_raster(&input, None, &request, &OutputOptions::new(&out)),
            "take as missing",
        );
        assert!(!out.exists());
    }

    fn raster_request(raster: PathBuf) -> RasterRequest {
        RasterRequest {
            raster,
            raster_variable: None,
            operation: Operation::Multiply,
            method: Method::Near,
            static_raster: false,
        }
    }

    #[test]
    fn the_sanitizer_refuses_a_reserved_replacement() {
        let d = fresh_dir("reserved_san");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
        for replacement in [-901.0f32, -902.0] {
            let t = Thresholds {
                minimum: 2.0,
                minimum_replacement: replacement,
                ..Thresholds::default()
            };
            let out = d.join("out.dss");
            refused(
                sanitize(&input, None, t, &OutputOptions::new(&out)),
                "take as missing",
            );
            assert!(!out.exists());
        }
    }

    #[test]
    fn a_raster_inputs_reserved_value_is_refused_not_lost() {
        // A GeoTIFF or ASCII grid may hold -901 as a value. Unchanged, it would read back as missing.
        let d = fresh_dir("reserved_input");
        let input = asc(&d, "in.asc", "-901 1 2\n3 4 5", -9999.0);
        let out = d.join("out.dss");
        let o = OutputOptions {
            part_c: Some("PRECIPITATION".into()),
            ..OutputOptions::new(&out)
        };
        refused(
            sanitize(&input, None, Thresholds::default(), &o),
            "take as missing",
        );
        assert!(!out.exists());
    }

    #[test]
    fn the_normalizer_refuses_a_reserved_result() {
        let d = fresh_dir("reserved_norm");
        let second = "///PRECIPITATION/01JAN2017:0100/01JAN2017:0200//";
        let window = "///PRECIPITATION/01JAN2017:0000/01JAN2017:0200//";
        let request = |normals| NormalizeRequest {
            end: (julian(1), 7200),
            interval_seconds: 7200,
            ..two_hours(normals)
        };
        // Sources -450.5 and -450.5, so a total of -901. A normal total of -901 scales by 1.
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[-450.5, 1.0, 1.0, 1.0, 1.0, 1.0]));
        put(&input, 7, shg(second, &[-450.5, 1.0, 1.0, 1.0, 1.0, 1.0]));
        // Normal -1802 is a value and scales each by 2, which gives exactly -901.
        let normals = d.join("normals.dss");
        put(
            &normals,
            7,
            shg(window, &[-1802.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
        );
        let out = d.join("out.dss");
        refused(
            normalize(&input, &request(normals), &OutputOptions::new(&out)),
            "take as missing",
        );
        assert!(!out.exists());
        // The control: normal -3604 gives -1802, a value, and the run writes.
        let normals = d.join("normals2.dss");
        put(
            &normals,
            7,
            shg(window, &[-3604.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
        );
        let out = d.join("out2.dss");
        normalize(&input, &request(normals), &OutputOptions::new(&out)).unwrap();
        assert_eq!(read(&out).len(), 2);
    }

    fn linked(d: &Path, input: &Path, name: &str) -> PathBuf {
        let alias = d.join(name);
        fs::hard_link(input, &alias).unwrap();
        alias
    }

    #[test]
    fn a_hard_link_to_an_input_is_refused_and_both_names_keep_their_bytes() {
        let d = fresh_dir("hard_link");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
        let alias = linked(&d, &input, "alias.dss");
        let before = fs::read(&input).unwrap();
        refused(
            calculate(
                &input,
                None,
                Operation::Multiply,
                2.0,
                &OutputOptions::new(&alias),
            ),
            "also the input",
        );
        assert_eq!(fs::read(&input).unwrap(), before);
        assert_eq!(fs::read(&alias).unwrap(), before);
    }

    #[test]
    fn a_hard_link_to_a_normal_or_a_raster_is_refused() {
        let d = fresh_dir("hard_link_other");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0; 6]));
        let normals = d.join("normals.dss");
        put(&normals, 7, shg(P, &[2.0; 6]));
        let alias = linked(&d, &normals, "alias.dss");
        let before = fs::read(&normals).unwrap();
        let request = NormalizeRequest {
            end: (julian(1), 3600),
            interval_seconds: 3600,
            ..two_hours(normals.clone())
        };
        refused(
            normalize(&input, &request, &OutputOptions::new(&alias)),
            "also the input",
        );
        assert_eq!(fs::read(&normals).unwrap(), before);
        assert_eq!(fs::read(&alias).unwrap(), before);

        let raster = d.join("raster.dss");
        put(&raster, 7, shg(P, &[2.0; 6]));
        let alias = linked(&d, &raster, "alias2.dss");
        let before = fs::read(&raster).unwrap();
        refused(
            calculate_raster(
                &input,
                None,
                &raster_request(raster.clone()),
                &OutputOptions::new(&alias),
            ),
            "also the input",
        );
        assert_eq!(fs::read(&raster).unwrap(), before);
        assert_eq!(fs::read(&alias).unwrap(), before);
    }

    fn hrap<'a>(pathname: &'a str, cells: &'a [f32]) -> Put<'a> {
        Put {
            ll: (300, 400),
            projection: GridProjection::Hrap {
                data_source: Vec::new(),
            },
            ..shg(pathname, cells)
        }
    }

    // `zStructSpatialGrid.h` HRAP_SRC_DEFINITION, as GDAL 3.2.1 re-exports it in both Vortex versions.
    const HRAP_VORTEX: &str = "PROJCS[\"Stereographic_CONUS_HRAP\",GEOGCS[\"GCS_Sphere_LFM\",DATUM[\"D_Sphere_LFM\",SPHEROID[\"Shpere_LFM\",6371200,0]],PRIMEM[\"Greenwich\",0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Polar_Stereographic\"],PARAMETER[\"latitude_of_origin\",60],PARAMETER[\"central_meridian\",-105],PARAMETER[\"false_easting\",1909762.5],PARAMETER[\"false_northing\",7624762.5],UNIT[\"metre\",1,AUTHORITY[\"EPSG\",\"9001\"]],AXIS[\"Easting\",SOUTH],AXIS[\"Northing\",SOUTH]]";

    #[test]
    fn an_hrap_grid_computes_and_says_what_vortex_says() {
        for version in [7u8, 6] {
            let d = fresh_dir("hrap");
            let input = d.join("in.dss");
            put(&input, version, hrap(P, &[1.0, 2.0, 3.0, 4.0, M, 6.0]));
            let out = d.join("out.dss");
            calculate(
                &input,
                None,
                Operation::Multiply,
                2.0,
                &OutputOptions::new(&out),
            )
            .unwrap();
            let grid = one_grid(&out);
            assert_eq!(bits(&grid.data), bits(&[2.0, 4.0, 6.0, 8.0, M, 12.0]));
            assert_eq!(grid.srs_definition, HRAP_VORTEX);
            assert_eq!((grid.lower_left_cell_x, grid.lower_left_cell_y), (300, 400));
        }
    }

    #[test]
    fn hrap_cell_numbers_count_from_the_grids_own_origin() {
        // The pole is HRAP point (401, 1601), so the false origin is what puts cell numbers on the map.
        let def = CrsDef::parse_wkt(HRAP_VORTEX).unwrap();
        let lonlat = CrsDef::sphere_lonlat(6371200.0);
        let t = Transformer::new(&def, &lonlat, DatumPolicy::default()).unwrap();
        let cell = 4762.5;
        let (_, lat) = t.transform(401.0 * cell, 1601.0 * cell).unwrap();
        assert!((lat - 90.0).abs() < 1e-9, "{lat}");
        // NWS's published formula, written out: r = 6371200 (1 + sin 60) tan((90 - lat) / 2).
        let (lon, lat) = t.transform(300.0 * cell, 400.0 * cell).unwrap();
        let (dx, dy) = (300.0 - 401.0, 400.0 - 1601.0);
        let r = f64::hypot(dx, dy) * cell;
        let k = 6371200.0 * (1.0 + 60f64.to_radians().sin());
        let expected_lat = 90.0 - (2.0 * (r / k).atan()).to_degrees();
        let expected_lon = -105.0 + f64::atan2(dx, -dy).to_degrees();
        assert!((lat - expected_lat).abs() < 1e-9, "{lat} {expected_lat}");
        assert!((lon - expected_lon).abs() < 1e-9, "{lon} {expected_lon}");
    }

    #[test]
    fn an_hrap_grid_goes_through_all_four_tools() {
        let d = fresh_dir("hrap_tools");
        let input = d.join("in.dss");
        put(&input, 7, hrap(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
        let out = d.join("san.dss");
        let t = Thresholds {
            minimum: 2.0,
            minimum_replacement: 0.0,
            ..Thresholds::default()
        };
        sanitize(&input, None, t, &OutputOptions::new(&out)).unwrap();
        assert_eq!(one_grid(&out).data, [0.0, 0.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(one_grid(&out).srs_definition, HRAP_VORTEX);

        let normals = d.join("normals.dss");
        put(&normals, 7, hrap(P, &[2.0; 6]));
        let request = NormalizeRequest {
            end: (julian(1), 3600),
            interval_seconds: 3600,
            ..two_hours(normals)
        };
        let out = d.join("norm.dss");
        normalize(&input, &request, &OutputOptions::new(&out)).unwrap();
        assert_eq!(
            bits(&one_grid(&out).data),
            bits(&[
                2.0 / 1.0 * 1.0,
                2.0 / 2.0 * 2.0,
                2.0 / 3.0 * 3.0,
                2.0 / 4.0 * 4.0,
                2.0 / 5.0 * 5.0,
                2.0f32 / 6.0 * 6.0,
            ])
        );

        let cell = 2000.0;
        let clip_request = ClipRequest {
            bounds: ClipBounds::Envelope {
                envelope: Envelope {
                    min_x: 300.0 * cell,
                    max_x: 302.0 * cell,
                    min_y: 400.0 * cell,
                    max_y: 402.0 * cell,
                },
                crs: Crs::Wkt(HRAP_VORTEX.to_string()),
            },
        };
        let out = d.join("clip.dss");
        clip(&input, None, &clip_request, &OutputOptions::new(&out)).unwrap();
        let grid = one_grid(&out);
        assert_eq!((grid.nx, grid.ny), (2, 2));
        assert_eq!(grid.data, [1.0, 2.0, 4.0, 5.0]);
        assert_eq!(grid.srs_definition, HRAP_VORTEX);

        let raster = d.join("raster.dss");
        put(&raster, 7, hrap(P, &[2.0; 6]));
        let out = d.join("rcalc.dss");
        calculate_raster(
            &input,
            None,
            &raster_request(raster),
            &OutputOptions::new(&out),
        )
        .unwrap();
        assert_eq!(one_grid(&out).data, [2.0, 4.0, 6.0, 8.0, 10.0, 12.0]);
    }

    #[test]
    fn albers_units_other_than_metres_are_refused_and_never_become_metres() {
        let other = |units: &[u8]| AlbersParameters {
            first_standard_parallel: 30.0,
            second_standard_parallel: 46.0,
            central_meridian: -100.0,
            latitude_of_projection_origin: 25.0,
            units: units.to_vec(),
            ..AlbersParameters::shg()
        };
        let shg_params = |units: &[u8]| AlbersParameters {
            units: units.to_vec(),
            ..AlbersParameters::shg()
        };
        let d = fresh_dir("albers_units");
        let refused = [
            ("foot", shg_params(b"foot")),
            ("US survey foot", shg_params(b"US survey foot")),
            ("Feet", shg_params(b"Feet")),
            ("km", shg_params(b"km")),
            ("blank", shg_params(b"")),
            ("feet", other(b"feet")),
            ("km", other(b"km")),
            ("furlong", other(b"furlong")),
            ("blank", other(b"")),
            ("spaced", other(b" Meters")),
        ];
        for (i, (label, params)) in refused.into_iter().enumerate() {
            let input = d.join(format!("in{i}.dss"));
            put(
                &input,
                7,
                Put {
                    projection: GridProjection::Albers(params),
                    ..shg(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
                },
            );
            let out = d.join(format!("out{i}.dss"));
            let got = calculate(
                &input,
                None,
                Operation::Multiply,
                2.0,
                &OutputOptions::new(&out),
            );
            assert!(got.is_err(), "{label} must be refused");
            assert!(!out.exists(), "{label} must write nothing");
        }
        for (i, units) in [
            &b"Meters"[..],
            b"METERS",
            b"Meter",
            b"metre",
            b"Metres",
            b"M",
        ]
        .into_iter()
        .enumerate()
        {
            let input = d.join(format!("ok{i}.dss"));
            put(
                &input,
                7,
                Put {
                    projection: GridProjection::Albers(shg_params(units)),
                    ..shg(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
                },
            );
            let out = d.join(format!("okout{i}.dss"));
            calculate(
                &input,
                None,
                Operation::Multiply,
                2.0,
                &OutputOptions::new(&out),
            )
            .unwrap();
            assert_eq!(one_grid(&out).data, [2.0, 4.0, 6.0, 8.0, 10.0, 12.0]);
        }
    }

    #[test]
    fn an_albers_grid_other_than_shg_computes_and_says_what_vortex_says() {
        let d = fresh_dir("albers");
        let input = d.join("in.dss");
        put(
            &input,
            7,
            Put {
                projection: GridProjection::Albers(AlbersParameters {
                    first_standard_parallel: 30.0,
                    second_standard_parallel: 46.0,
                    central_meridian: -100.0,
                    latitude_of_projection_origin: 25.0,
                    units: b"metre".to_vec(),
                    ..AlbersParameters::shg()
                }),
                ..shg(P, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
            },
        );
        let out = d.join("out.dss");
        calculate(
            &input,
            None,
            Operation::Multiply,
            2.0,
            &OutputOptions::new(&out),
        )
        .unwrap();
        let grid = one_grid(&out);
        assert_eq!(grid.data, [2.0, 4.0, 6.0, 8.0, 10.0, 12.0]);
        assert_eq!(grid.grid_type, dss_core::grid::GridType::Specified);
        assert_eq!(
            grid.srs_definition,
            "PROJCS[\"unnamed\",GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101,AUTHORITY[\"EPSG\",\"7019\"]],AUTHORITY[\"EPSG\",\"6269\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4269\"]],PROJECTION[\"Albers_Conic_Equal_Area\"],PARAMETER[\"latitude_of_center\",25],PARAMETER[\"longitude_of_center\",-100],PARAMETER[\"standard_parallel_1\",30],PARAMETER[\"standard_parallel_2\",46],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0],UNIT[\"Meter\",1],AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]]"
        );
    }

    #[test]
    fn a_grid_whose_cell_zero_is_not_at_the_crs_origin_is_refused() {
        // `dssToDto` ignores these coordinates, and so moves the grid with no message.
        let d = fresh_dir("cell_zero");
        let input = d.join("in.dss");
        create_dss7(&input).unwrap();
        let cells = [1.0f32; 6];
        let record = GridInput {
            pathname: P.as_bytes(),
            projection: GridProjection::Albers(AlbersParameters::shg()),
            time_stamped: true,
            data: &cells,
            nx: NX,
            ny: NY,
            lower_left_cell_x: LL.0,
            lower_left_cell_y: LL.1,
            cell_size: 2000.0,
            x_coord_of_grid_cell_zero: 2000.0,
            y_coord_of_grid_cell_zero: 0.0,
            units: b"MM",
            data_type: PER_CUM,
            compression: GridCompression::Zlib,
        };
        write_grid(&input, &record, &WriteOptions::default()).unwrap();
        let out = d.join("out.dss");
        refused(
            calculate(&input, None, Operation::Add, 1.0, &OutputOptions::new(&out)),
            "cell zero",
        );
        assert!(!out.exists());
    }

    #[test]
    fn a_planning_refusal_has_nothing_in_doubt() {
        let d = fresh_dir("not_in_doubt");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0; 6]));
        let out = d.join("out.dss");
        let e = refused(
            calculate(
                &input,
                None,
                Operation::Divide,
                0.0,
                &OutputOptions::new(&out),
            ),
            "cannot hold",
        );
        assert_eq!(e.in_doubt, None);
    }

    // One record, one hour, over the normal given.
    fn normalized_once(name: &str, source: &[f32], normal: &[f32], version: u8) -> (u64, PathBuf) {
        let d = fresh_dir(name);
        let input = d.join("in.dss");
        put(&input, 7, shg(P, source));
        let normals = d.join("normals.dss");
        put(&normals, 7, shg(P, normal));
        let request = NormalizeRequest {
            end: (julian(1), 3600),
            interval_seconds: 3600,
            ..two_hours(normals)
        };
        let out = d.join("out.dss");
        let options = OutputOptions {
            version: Some(version),
            ..OutputOptions::new(&out)
        };
        let outcome = normalize(&input, &request, &options).unwrap();
        assert_eq!(outcome.records.len(), 1);
        (outcome.cells_made_missing_by_normal, out)
    }

    #[test]
    fn a_missing_normal_is_written_missing_and_counted() {
        for version in [7, 6] {
            let (count, out) = normalized_once(
                &format!("missing_normal_{version}"),
                &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
                &[2.0, M, 2.0, M, 2.0, 2.0],
                version,
            );
            // Counted once, though planning computes each frame as well.
            assert_eq!(count, 2);
            assert_eq!(
                bits(&one_grid(&out).data),
                bits(&[2.0, M, 2.0, M, 2.0, 2.0])
            );
        }
    }

    #[test]
    fn statistics_follow_the_mask_of_a_missing_normal() {
        // The largest source cell is the one with no normal. Passing it through would make it the
        // maximum, and would raise the mean.
        for version in [7, 6] {
            let (_, out) = normalized_once(
                &format!("missing_normal_stats_{version}"),
                &[1.0, 1.0, 1.0, 1.0, 1.0, 900.0],
                &[2.0, 2.0, 2.0, 2.0, 2.0, M],
                version,
            );
            let g = one_grid(&out);
            assert_eq!(bits(&g.data), bits(&[2.0, 2.0, 2.0, 2.0, 2.0, M]));
            assert_eq!(
                (g.min_data_value, g.max_data_value, g.mean_data_value),
                (Some(2.0), Some(2.0), Some(2.0))
            );
        }
    }

    #[test]
    fn a_source_cell_missing_too_is_not_counted() {
        let (count, out) = normalized_once(
            "missing_both",
            &[M, 2.0, 3.0, M, 5.0, 6.0],
            &[M, M, 2.0, 2.0, 2.0, 2.0],
            7,
        );
        assert_eq!(count, 1);
        assert_eq!(bits(&one_grid(&out).data), bits(&[M, M, 2.0, M, 2.0, 2.0]));
    }

    #[test]
    fn a_zero_total_with_no_normal_is_missing_and_not_zero() {
        let (count, out) = normalized_once(
            "missing_normal_zero",
            &[0.0, 0.0, 3.0, 4.0, 5.0, 6.0],
            &[M, 2.0, 3.0, 4.0, 5.0, 6.0],
            7,
        );
        assert_eq!(count, 1);
        assert_eq!(
            bits(&one_grid(&out).data),
            bits(&[M, 0.0, 3.0, 4.0, 5.0, 6.0])
        );
    }

    #[test]
    fn a_normal_missing_everywhere_writes_an_all_missing_record_and_counts_it() {
        for version in [7, 6] {
            let (count, out) = normalized_once(
                &format!("missing_everywhere_{version}"),
                &[1.0, 2.0, M, 0.0, 5.0, 6.0],
                &[M; 6],
                version,
            );
            // Every valid source cell, the zero included, and not the one already missing.
            assert_eq!(count, 5);
            assert_eq!(bits(&one_grid(&out).data), bits(&[M; 6]));
        }
    }

    #[test]
    fn a_stated_static_raster_is_reused_and_equal_times_need_no_option() {
        let d = fresh_dir("time_static");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0; 6]));
        let second = "///PRECIPITATION/01JAN2017:0100/01JAN2017:0200//";
        put(&input, 7, shg(second, &[3.0; 6]));
        let raster = d.join("raster.dss");
        put(&raster, 7, shg(P, &[2.0; 6]));
        // One record of the series is at the raster's time, and the other is not.
        let out = d.join("no.dss");
        refused(
            calculate_raster(
                &input,
                None,
                &raster_request(raster.clone()),
                &OutputOptions::new(&out),
            ),
            "static_raster",
        );
        assert!(!out.exists());
        let out = d.join("static.dss");
        let request = RasterRequest {
            static_raster: true,
            ..raster_request(raster.clone())
        };
        calculate_raster(&input, None, &request, &OutputOptions::new(&out)).unwrap();
        let all = read(&out);
        assert_eq!(all[P].data, [2.0; 6]);
        assert_eq!(all[second].data, [6.0; 6]);
        // The same time, stated, is no disagreement.
        let out = d.join("paired.dss");
        let names = [P.to_string()];
        calculate_raster(
            &input,
            Some(&names),
            &raster_request(raster),
            &OutputOptions::new(&out),
        )
        .unwrap();
        assert_eq!(read(&out)[P].data, [2.0; 6]);
        // A raster with no time is static by nature.
        let plain = asc(&d, "r.asc", "2 2 2\n2 2 2", -9999.0);
        let out = d.join("plain.dss");
        calculate_raster(
            &input,
            None,
            &raster_request(plain),
            &OutputOptions::new(&out),
        )
        .unwrap();
        assert_eq!(read(&out).len(), 2);
    }

    #[test]
    fn a_dss_raster_at_another_time_is_refused() {
        let d = fresh_dir("time");
        let input = d.join("in.dss");
        put(&input, 7, shg(P, &[1.0; 6]));
        let raster = d.join("raster.dss");
        let jan2 = "///PRECIPITATION/02JAN2017:0000/02JAN2017:0100//";
        put(&raster, 7, shg(jan2, &[2.0; 6]));
        let out = d.join("out.dss");
        refused(
            calculate_raster(
                &input,
                None,
                &raster_request(raster),
                &OutputOptions::new(&out),
            ),
            "static_raster",
        );
        assert!(!out.exists());
    }
}

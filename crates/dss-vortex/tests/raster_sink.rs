//! The raster sink: round trips through this crate's reader, refusals, and atomic replacement.

use dss_vortex::crs::CrsDef;
use dss_vortex::sink_raster::{
    encode_asc_to, encode_geotiff_to, write_asc, write_geotiff, AscGeoref, AscNodata, AscOptions,
    NodataPolicy, TiffOptions,
};
use dss_vortex::source::raster::{CrsStatus, RasterFile};
use dss_vortex::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
use std::fs;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("p50_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Every entry, so a stray temporary file fails a test.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cells() -> Vec<f32> {
    let mut out: Vec<f32> = (0..12).map(|i| i as f32 * 0.1 + 0.125).collect();
    out[1] = -9999.0;
    out[5] = 3.4e-42; // subnormal
    out[6] = -0.0;
    out
}

fn frame_with(
    crs: Crs,
    transform: GeoTransform,
    order: RowOrder,
    nodata: Option<f32>,
) -> GridFrame {
    GridFrame::new(GridFrameParts {
        cells: cells(),
        nx: 4,
        ny: 3,
        row_order: order,
        transform,
        crs,
        nodata,
        time: FrameTime::Invariant,
        data_type: DataType::InstVal,
        units: String::new(),
        variable: String::new(),
        description: None,
    })
    .unwrap()
}

fn north_up() -> GeoTransform {
    GeoTransform::axis_aligned(-646000.0, 2000.0, 2124000.0, -2000.0)
}

fn frame(crs: Crs) -> GridFrame {
    frame_with(crs, north_up(), RowOrder::NorthToSouth, Some(-9999.0))
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

fn crs_cases() -> Vec<(&'static str, Crs)> {
    let lcc = CrsDef {
        datum: dss_vortex::crs::Datum::Sphere,
        a: 6371229.0,
        rf: 0.0,
        projection: dss_vortex::crs::Projection::Lambert {
            lat_1: 38.5,
            lat_2: 38.5,
            lat_0: 38.5,
            lon_0: -97.5,
            k_0: 1.0,
            x_0: 0.0,
            y_0: 0.0,
        },
    };
    let stere = CrsDef {
        datum: dss_vortex::crs::Datum::Wgs84,
        a: 6378137.0,
        rf: 298.257223563,
        projection: dss_vortex::crs::Projection::Stereographic {
            lat_0: 40.0,
            lon_0: -100.0,
            k_0: 0.9,
            lat_ts: None,
            x_0: 10.0,
            y_0: 20.0,
        },
    };
    vec![
        ("undefined", Crs::Undefined),
        ("shg", Crs::Named(NamedCrs::Shg)),
        ("hrap", Crs::Named(NamedCrs::Hrap)),
        ("wkt_lcc", Crs::Wkt(lcc.to_wkt())),
        ("wkt_stere", Crs::Wkt(stere.to_wkt())),
        ("wkt_lonlat", Crs::Wkt(CrsDef::sphere_lonlat(6371000.0).to_wkt())),
        (
            "esri_text",
            Crs::Wkt(
                "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]"
                    .to_string(),
            ),
        ),
        (
            // Transverse Mercator in feet: a CRS the crs module refuses to transform, kept as text.
            "not_transformable",
            Crs::Wkt(
                "PROJCS[\"NAD_1983_Florida_East_ftUS\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"False_Easting\",656166.667],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-81.0],PARAMETER[\"Scale_Factor\",0.999941177],PARAMETER[\"Latitude_Of_Origin\",24.33333333333333],UNIT[\"Foot_US\",0.3048006096012192]]"
                    .to_string(),
            ),
        ),
    ]
}

#[test]
fn what_is_written_reads_back_the_crs_it_was_given() {
    let scratch = Scratch::new("crs_round_trip");
    for (name, crs) in crs_cases() {
        for (ext, write) in [("tif", true), ("asc", false)] {
            let path = scratch.path(&format!("{name}.{ext}"));
            let input = frame(crs.clone());
            if write {
                write_geotiff(&input, &path, &TiffOptions::default()).unwrap();
            } else {
                write_asc(&input, &path, &AscOptions::default()).unwrap();
            }
            let read = RasterFile::open(&path).unwrap();
            assert_eq!(read.frame().parts().crs, crs, "{name}.{ext}");
            assert_eq!(
                read.crs_status(),
                &if crs == Crs::Undefined {
                    CrsStatus::NotInFile
                } else {
                    CrsStatus::Stated
                },
                "{name}.{ext}"
            );
            assert!(
                same_bits(read.frame().cells(), input.cells()),
                "{name}.{ext}"
            );
        }
    }
}

#[test]
fn every_transform_form_round_trips_through_geotiff() {
    let scratch = Scratch::new("transforms");
    let rotated = GeoTransform {
        origin_x: 1000.5,
        dx: 1990.0,
        row_rotation: 150.0,
        origin_y: 2000.25,
        column_rotation: 120.0,
        dy: -1995.0,
    };
    let cases = [
        ("north_up", north_up(), RowOrder::NorthToSouth),
        (
            "south_up",
            GeoTransform::axis_aligned(-646000.0, 2000.0, 1922000.0, 2000.0),
            RowOrder::SouthToNorth,
        ),
        ("rotated", rotated, RowOrder::NorthToSouth),
        (
            "fine",
            GeoTransform::axis_aligned(
                -125.020833333333,
                0.041666666666666664,
                49.9375,
                -0.041666666666666664,
            ),
            RowOrder::NorthToSouth,
        ),
    ];
    for (name, transform, order) in cases {
        for options in [
            TiffOptions::default(),
            TiffOptions {
                bigtiff: false,
                deflate: false,
                ..TiffOptions::default()
            },
        ] {
            let input = frame_with(Crs::Named(NamedCrs::Shg), transform, order, Some(-9999.0));
            let path = scratch.path(&format!("{name}.tif"));
            write_geotiff(&input, &path, &options).unwrap();
            let got = RasterFile::open(&path).unwrap().into_frame();
            assert_eq!(got.parts().transform, transform, "{name}");
            assert_eq!(got.parts().row_order, order, "{name}");
            assert!(same_bits(got.cells(), input.cells()), "{name}");
        }
    }
}

#[test]
fn nodata_round_trips_including_nan_and_none() {
    let scratch = Scratch::new("nodata");
    for (name, nodata) in [
        ("none", None),
        ("nan", Some(f32::NAN)),
        ("zero", Some(0.0)),
        ("neg_zero", Some(-0.0)),
        ("frac", Some(0.1)),
        ("lowest", Some(f32::MIN)),
        ("subnormal", Some(1e-40)),
        ("inf", Some(f32::INFINITY)),
    ] {
        let input = frame_with(Crs::Undefined, north_up(), RowOrder::NorthToSouth, nodata);
        let path = scratch.path(&format!("{name}.tif"));
        write_geotiff(&input, &path, &TiffOptions::default()).unwrap();
        let got = RasterFile::open(&path).unwrap().into_frame();
        match (got.parts().nodata, nodata) {
            (None, None) => {}
            (Some(a), Some(b)) if b.is_nan() => assert!(a.is_nan()),
            (Some(a), Some(b)) => assert_eq!(a.to_bits(), b.to_bits(), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn an_asc_of_a_south_up_frame_comes_back_flipped() {
    let scratch = Scratch::new("asc_flip");
    let input = frame_with(
        Crs::Undefined,
        GeoTransform::axis_aligned(-646000.0, 2000.0, 1922000.0, 2000.0),
        RowOrder::SouthToNorth,
        Some(-9999.0),
    );
    let path = scratch.path("a.asc");
    write_asc(&input, &path, &AscOptions::default()).unwrap();
    let got = RasterFile::open(&path).unwrap().into_frame();
    let want = input.with_row_order(RowOrder::NorthToSouth).unwrap();
    assert_eq!(got.parts().transform, want.parts().transform);
    assert!(same_bits(got.cells(), want.cells()));
}

#[test]
fn an_asc_keeps_a_transform_whose_text_needs_more_than_twelve_decimals() {
    let scratch = Scratch::new("asc_exact");
    let transform = GeoTransform::axis_aligned(
        -125.020833333333,
        0.041666666666666664,
        49.9375,
        -0.041666666666666664,
    );
    let input = frame_with(Crs::Undefined, transform, RowOrder::NorthToSouth, None);
    let path = scratch.path("a.asc");
    write_asc(&input, &path, &AscOptions::default()).unwrap();
    let got = RasterFile::open(&path).unwrap().into_frame();
    assert_eq!(got.parts().transform, transform);

    let square_fails = input.clone();
    let vortex = AscOptions {
        nodata: AscNodata::Keep,
        georef: AscGeoref::Vortex,
    };
    write_asc(&square_fails, &path, &vortex).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("cellsize     0.041666666667\n"), "{text}");
}

#[test]
fn unequal_cell_sizes_are_written_as_dx_dy_and_refused_under_vortex_rules() {
    let scratch = Scratch::new("asc_dxdy");
    let transform = GeoTransform::axis_aligned(0.0, 2.5, 100.0, -3.5);
    let input = frame_with(Crs::Undefined, transform, RowOrder::NorthToSouth, None);
    let path = scratch.path("a.asc");
    write_asc(&input, &path, &AscOptions::default()).unwrap();
    assert_eq!(
        RasterFile::open(&path).unwrap().frame().parts().transform,
        transform
    );
    let refused = scratch.path("b.asc");
    let error = write_asc(&input, &refused, &AscOptions::vortex())
        .unwrap_err()
        .to_string();
    assert!(error.contains("observed behaviour"), "{error}");
    assert!(!refused.exists());
}

#[test]
fn vortex_rules_reset_the_marker_as_resetnodatavalue_does() {
    let scratch = Scratch::new("reset");
    let mut values = cells();
    values[0] = f32::NAN;
    values[1] = -1.0;
    let input = GridFrame::new(GridFrameParts {
        cells: values,
        nodata: Some(f32::NAN),
        ..frame(Crs::Undefined).into_parts()
    })
    .unwrap();
    let path = scratch.path("a.tif");
    write_geotiff(&input, &path, &TiffOptions::vortex()).unwrap();
    let got = RasterFile::open(&path).unwrap().into_frame();
    assert_eq!(got.parts().nodata, Some(-9999.0));
    assert_eq!(got.cells()[0], -9999.0);
    assert_eq!(got.cells()[1], -1.0);

    let policy = TiffOptions {
        nodata: NodataPolicy::Reset(7.0),
        ..TiffOptions::default()
    };
    write_geotiff(&input, &path, &policy).unwrap();
    let got = RasterFile::open(&path).unwrap().into_frame();
    assert_eq!(got.parts().nodata, Some(7.0));
    assert_eq!(got.cells()[0], 7.0);
}

fn assert_refused_and_clean(
    scratch: &Scratch,
    name: &str,
    result: Result<(), dss_vortex::VortexError>,
    before: &[String],
) {
    let error = result.expect_err(name);
    assert!(
        matches!(error, dss_vortex::VortexError::Refused { .. }),
        "{name}: {error}"
    );
    assert_eq!(&scratch.names(), before, "{name}: left files behind");
}

#[test]
fn a_refused_write_leaves_nothing_and_an_existing_target_unchanged() {
    let scratch = Scratch::new("refused");
    let good = frame(Crs::Named(NamedCrs::Shg));
    let tif = scratch.path("a.tif");
    let asc = scratch.path("a.asc");
    write_geotiff(&good, &tif, &TiffOptions::default()).unwrap();
    write_asc(&good, &asc, &AscOptions::default()).unwrap();
    let (tif_before, asc_before, prj_before) = (
        fs::read(&tif).unwrap(),
        fs::read(&asc).unwrap(),
        fs::read(scratch.path("a.prj")).unwrap(),
    );
    let names = scratch.names();

    let bad_crs = [
        ("proj", Crs::Proj("+proj=longlat".to_string())),
        ("garbage", Crs::Wkt("not a crs".to_string())),
        ("wkt2", Crs::Wkt("PROJCRS[\"x\"]".to_string())),
        ("unbalanced", Crs::Wkt("PROJCS[\"x\"".to_string())),
    ];
    for (name, crs) in bad_crs {
        let bad = frame(crs);
        assert_refused_and_clean(
            &scratch,
            name,
            write_geotiff(&bad, &tif, &TiffOptions::default()),
            &names,
        );
        assert_refused_and_clean(
            &scratch,
            name,
            write_asc(&bad, &asc, &AscOptions::default()),
            &names,
        );
    }
    let pipe = frame(Crs::Wkt(
        "GEOGCS[\"a|b\",DATUM[\"unknown\",SPHEROID[\"s\",6371000,0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"
            .to_string(),
    ));
    assert_refused_and_clean(
        &scratch,
        "pipe",
        write_geotiff(&pipe, &tif, &TiffOptions::default()),
        &names,
    );
    let long = format!(
        "PROJCS[\"{}\",PROJECTION[\"Transverse_Mercator\"]]",
        "x".repeat(4000)
    );
    assert_refused_and_clean(
        &scratch,
        "long",
        write_geotiff(&frame(Crs::Wkt(long)), &tif, &TiffOptions::default()),
        &names,
    );
    let rotated = frame_with(
        Crs::Undefined,
        GeoTransform {
            row_rotation: 1.0,
            ..north_up()
        },
        RowOrder::NorthToSouth,
        None,
    );
    assert_refused_and_clean(
        &scratch,
        "rotated",
        write_asc(&rotated, &asc, &AscOptions::default()),
        &names,
    );
    let mut infinite = frame(Crs::Undefined).into_parts();
    infinite.cells[2] = f32::INFINITY;
    assert_refused_and_clean(
        &scratch,
        "infinite",
        write_asc(
            &GridFrame::new(infinite).unwrap(),
            &asc,
            &AscOptions::default(),
        ),
        &names,
    );
    let mut all_nan = frame(Crs::Undefined).into_parts();
    all_nan.cells = vec![f32::NAN; 12];
    all_nan.nodata = Some(f32::NAN);
    assert_refused_and_clean(
        &scratch,
        "all nan, which GDAL would read as integers",
        write_asc(
            &GridFrame::new(all_nan).unwrap(),
            &asc,
            &AscOptions::default(),
        ),
        &names,
    );

    assert_eq!(fs::read(&tif).unwrap(), tif_before);
    assert_eq!(fs::read(&asc).unwrap(), asc_before);
    assert_eq!(fs::read(scratch.path("a.prj")).unwrap(), prj_before);
}

#[test]
fn a_sidecar_that_would_override_the_write_is_refused() {
    let scratch = Scratch::new("aux");
    fs::write(scratch.path("a.tif.aux.xml"), "<PAMDataset/>").unwrap();
    let error = write_geotiff(
        &frame(Crs::Undefined),
        &scratch.path("a.tif"),
        &TiffOptions::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains(".aux.xml"), "{error}");
    assert_eq!(scratch.names(), ["a.tif.aux.xml"]);
}

#[test]
fn a_failed_rename_leaves_no_temporary_file() {
    let scratch = Scratch::new("rename");
    let target = scratch.path("a.tif");
    fs::create_dir(&target).unwrap();
    let error = write_geotiff(&frame(Crs::Undefined), &target, &TiffOptions::default());
    assert!(matches!(error, Err(dss_vortex::VortexError::Io(_))));
    assert_eq!(scratch.names(), ["a.tif"]);
    assert!(target.is_dir());
}

#[test]
fn a_missing_folder_is_an_io_error_and_creates_nothing() {
    let scratch = Scratch::new("missing");
    let target = scratch.path("no/such/a.asc");
    let error = write_asc(&frame(Crs::Undefined), &target, &AscOptions::default());
    assert!(matches!(error, Err(dss_vortex::VortexError::Io(_))));
    assert!(scratch.names().is_empty());
}

/// A writer that fails after a byte budget, as a full disk does.
struct Failing {
    inner: Cursor<Vec<u8>>,
    budget: usize,
}

impl Write for Failing {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.inner.position() as usize + data.len() > self.budget {
            return Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "full"));
        }
        self.inner.write(data)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Failing {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(to)
    }
}

#[test]
fn a_write_error_is_typed_at_every_point() {
    let good = frame(Crs::Named(NamedCrs::Shg));
    let mut full = Cursor::new(Vec::new());
    encode_geotiff_to(&mut full, &good, &TiffOptions::default()).unwrap();
    let size = full.into_inner().len();
    assert!(size > 100);
    for budget in (0..size).step_by(7) {
        let writer = Failing {
            inner: Cursor::new(Vec::new()),
            budget,
        };
        let result = encode_geotiff_to(writer, &good, &TiffOptions::default());
        assert!(result.is_err(), "budget {budget} of {size}");
    }
    let mut text = Vec::new();
    encode_asc_to(&mut text, &good, AscGeoref::RoundTrip).unwrap();
    for budget in 0..text.len() {
        let writer = Failing {
            inner: Cursor::new(Vec::new()),
            budget,
        };
        let result = encode_asc_to(writer, &good, AscGeoref::RoundTrip);
        assert!(
            matches!(result, Err(dss_vortex::VortexError::Io(_))),
            "budget {budget} of {}",
            text.len()
        );
    }
}

#[test]
fn replacing_an_asc_removes_a_stale_prj() {
    let scratch = Scratch::new("stale_prj");
    let path = scratch.path("a.asc");
    write_asc(
        &frame(Crs::Named(NamedCrs::Shg)),
        &path,
        &AscOptions::default(),
    )
    .unwrap();
    assert!(scratch.path("a.prj").exists());
    write_asc(&frame(Crs::Undefined), &path, &AscOptions::default()).unwrap();
    assert!(!scratch.path("a.prj").exists());
    let read = RasterFile::open(&path).unwrap();
    assert_eq!(read.frame().parts().crs, Crs::Undefined);
    assert_eq!(scratch.names(), ["a.asc"]);
}

#[test]
fn a_target_is_replaced_whole() {
    let scratch = Scratch::new("replace");
    let path = scratch.path("a.tif");
    write_geotiff(&frame(Crs::Undefined), &path, &TiffOptions::default()).unwrap();
    let other = GridFrame::new(GridFrameParts {
        cells: vec![1.0; 12],
        ..frame(Crs::Named(NamedCrs::Shg)).into_parts()
    })
    .unwrap();
    write_geotiff(&other, &path, &TiffOptions::default()).unwrap();
    let got = RasterFile::open(&path).unwrap().into_frame();
    assert_eq!(got.cells(), other.cells());
    assert_eq!(got.parts().crs, Crs::Named(NamedCrs::Shg));
    assert_eq!(scratch.names(), ["a.tif"]);
}

#[test]
fn a_temporary_file_does_not_pass_as_a_raster_name() {
    // What a crash can leave: a truncated file under a name the readers do not accept as ASCII,
    // and which is not the target's.
    let scratch = Scratch::new("crash");
    let leftover = scratch.path(".a.asc.1.0.tmp");
    fs::write(
        &leftover,
        "ncols 4\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\n 1 2",
    )
    .unwrap();
    assert!(RasterFile::open(&leftover).is_err());
    assert!(!Path::new(&scratch.path("a.asc")).exists());
}

/// Where a TIFF's directory and each out-of-line tag value begin, read from the bytes.
fn directory_and_value_offsets(bytes: &[u8]) -> (u64, Vec<u64>) {
    let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let width = |kind: u16| match kind {
        1 | 2 | 6 | 7 => 1u64,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        other => panic!("tag type {other}"),
    };
    let big = u16_at(2) == 43;
    let directory = if big { u64_at(8) } else { u64::from(u32_at(4)) };
    let at = directory as usize;
    let (count, first, size, inline) = if big {
        (u64_at(at) as usize, at + 8, 20, 8)
    } else {
        (u16_at(at) as usize, at + 2, 12, 4)
    };
    let mut values = Vec::new();
    for entry in (0..count).map(|i| first + i * size) {
        let kind = u16_at(entry + 2);
        let items = if big {
            u64_at(entry + 4)
        } else {
            u64::from(u32_at(entry + 4))
        };
        if width(kind) * items > inline {
            values.push(if big {
                u64_at(entry + 12)
            } else {
                u64::from(u32_at(entry + 8))
            });
        }
    }
    (directory, values)
}

fn aligned(bytes: &[u8], to: u64) -> bool {
    let (directory, values) = directory_and_value_offsets(bytes);
    directory % to == 0 && values.iter().all(|v| v % to == 0)
}

/// "All values must begin at an 8-byte-aligned address" (BigTIFF specification), and TIFF 6.0's
/// word boundary for a classic file.
#[test]
fn directories_and_tag_values_are_aligned_as_each_format_requires() {
    let scratch = Scratch::new("alignment");
    let wkt = Crs::Wkt(
        "GEOGCS[\"g\",DATUM[\"d\",SPHEROID[\"s\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"
            .to_string(),
    );
    let transform = GeoTransform::axis_aligned(0.5, 1.5, 9.5, -1.5);
    let mut checked = 0;
    // Different cell text and CRS forms move what comes before the directory by each residue.
    for (index, crs) in [Crs::Named(NamedCrs::Shg), wkt, Crs::Undefined]
        .into_iter()
        .enumerate()
    {
        let input = frame_with(crs, transform, RowOrder::NorthToSouth, Some(-9999.0));
        for (name, bigtiff, deflate, to) in [
            ("big_deflate", true, true, 8),
            ("big_plain", true, false, 8),
            ("classic_deflate", false, true, 2),
            ("classic_plain", false, false, 2),
        ] {
            let path = scratch.path(&format!("{name}_{index}.tif"));
            let options = TiffOptions {
                bigtiff,
                deflate,
                ..TiffOptions::default()
            };
            write_geotiff(&input, &path, &options).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert!(aligned(&bytes, to), "{name} {index}");
            let (_, values) = directory_and_value_offsets(&bytes);
            assert!(!values.is_empty(), "{name} {index}: nothing out of line");
            checked += 1;

            if bigtiff {
                // The check itself: a directory two bytes off fails it.
                let mut shifted = bytes.clone();
                let (directory, _) = directory_and_value_offsets(&bytes);
                shifted.splice(16..16, [0u8; 2]);
                shifted[8..16].copy_from_slice(&(directory + 2).to_le_bytes());
                assert!(
                    !aligned(&shifted, 8),
                    "{name} {index}: the check cannot fail"
                );
            }
        }
    }
    assert_eq!(checked, 12);
}

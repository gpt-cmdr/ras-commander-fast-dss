//! Grid to point: Vortex's masks and statistics on committed public fixtures, hostile zone files,
//! refusals that leave files alone, crash recovery, and the optional area-weighted mean.
//!
//! `fixtures/public/vortex_p51/expected.json` was recorded from Vortex 1.1.0 by
//! `oracle/vortex/make_p51_fixture.py`. The full matrix against both Vortex versions is
//! `oracle/vortex/run_zonal.py`, in the P51 report.

use dss_core::dss7::write::{
    create_dss7, write_rts, FaultPoint, RtsInput, RtsValues, WriteError, WriteOptions, WriteStep,
};
use dss_core::dss7::Dss7File;
use dss_core::TsValues;
use dss_vortex::crs::{CrsDef, DatumPolicy};
use dss_vortex::zonal::{
    self, Compat, GridPlacement, GridToPointFailure, GridToPointRequest, Part, PointParts, Pt,
    Statistic, ZonalRequest, ZoneLimits, ZoneSet, MISSING,
};
use dss_vortex::{Crs, VortexError};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/vortex_p51")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("dss_p51_{}", std::process::id()))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(grids: &Path, zones: &Path, field: &str) -> ZonalRequest {
    ZonalRequest {
        grids: grids.to_path_buf(),
        variables: None,
        zones: zones.to_path_buf(),
        field: field.to_string(),
        zones_crs: None,
        datum_policy: DatumPolicy::Refuse,
        compat: Compat::Engine,
        limits: ZoneLimits::default(),
        area_weighted: false,
    }
}

fn run_request(grids: &Path, zones: &Path, field: &str, output: &Path) -> GridToPointRequest {
    GridToPointRequest {
        zonal: request(grids, zones, field),
        output: output.to_path_buf(),
        statistics: vec![
            Statistic::Average,
            Statistic::Min,
            Statistic::Max,
            Statistic::Median,
            Statistic::FirstQuartile,
            Statistic::ThirdQuartile,
            Statistic::PctGtZero,
            Statistic::PctGtFirstQuartile,
        ],
        accumulate: true,
        parts: PointParts::default(),
        version: None,
        allow_undetectable_readers: true,
        allow_in_place: false,
        fault: None,
    }
}

const SHG_WKT: &str = "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]";

fn sample_grid() -> GridPlacement {
    GridPlacement {
        nx: 60,
        ny: 40,
        transform: [-100000.0, 2000.0, 0.0, 1000000.0, 0.0, -2000.0],
        crs: CrsDef::shg(),
    }
}

fn pt(col: f64, row: f64) -> Pt {
    Pt {
        x: -100000.0 + col * 2000.0,
        y: 1000000.0 - row * 2000.0,
    }
}

fn bits(text: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(text, 16).unwrap())
}

#[test]
fn masks_and_statistics_are_vortexs_on_the_fixtures() {
    let dir = fixtures();
    let expected: Value =
        serde_json::from_slice(&std::fs::read(dir.join("expected.json")).unwrap()).unwrap();
    let cases = expected["cases"].as_array().unwrap();
    assert!(cases.len() >= 3, "the fixture must hold its cases");
    let (mut cells, mut compared, mut not_missing) = (0usize, 0usize, 0usize);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let table = zonal::compute(&request(
            &dir.join(case["grids"].as_str().unwrap()),
            &dir.join(case["zones"].as_str().unwrap()),
            case["field"].as_str().unwrap(),
        ))
        .unwrap();
        let zones = case["masks"].as_object().unwrap();
        assert_eq!(zones.len(), table.zones.len(), "{id}: zone count");
        for zone in &table.zones {
            let want: Vec<u32> = zones[&zone.id]
                .as_array()
                .unwrap_or_else(|| panic!("{id}: {} is extra", zone.id))
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            assert_eq!(want, zone.cells, "{id}: {}", zone.id);
            cells += want.len();
        }
        let frames = case["frames"].as_array().unwrap();
        assert_eq!(frames.len(), table.frames.len(), "{id}: frames");
        for (f, want) in frames.iter().enumerate() {
            assert_eq!(table.frames[f].start, want["start"].as_i64(), "{id}");
            assert_eq!(table.frames[f].end, want["end"].as_i64(), "{id}");
            assert_eq!(table.frames[f].description, want["description"], "{id}");
            for (z, zone) in table.zones.iter().enumerate() {
                let s = &table.stats[f * table.zones.len() + z];
                let w = &want["stats"][&zone.id];
                for (name, got) in [
                    ("average", s.average),
                    ("min", s.min),
                    ("max", s.max),
                    ("median", s.median),
                    ("q1", s.first_quartile),
                    ("q3", s.third_quartile),
                    ("pct0", s.pct_gt_zero),
                    ("pctq1", s.pct_gt_first_quartile),
                ] {
                    let value = bits(w[name].as_str().unwrap());
                    assert_eq!(
                        got.to_bits(),
                        value.to_bits(),
                        "{id} frame {f} {} {name}: {got} and {value}",
                        zone.id
                    );
                    compared += 1;
                    not_missing += usize::from(value != MISSING);
                }
            }
        }
    }
    // The comparison cannot pass on nothing.
    assert!(cells > 1000, "{cells} cells");
    assert!(compared > 1000, "{compared} statistics");
    assert!(not_missing > compared / 2, "{not_missing} of {compared}");
}

#[test]
fn a_changed_cell_or_mask_fails_the_comparison_above() {
    // Negative control: the same statistics on another cell set differ.
    let mut a = [1.5, 2.5, 3.5, 4.5, 5.5];
    let mut b = [1.5, 2.5, 3.5, 4.5];
    let (sa, sb) = (
        zonal::statistics_of(&mut a, MISSING, Compat::Engine),
        zonal::statistics_of(&mut b, MISSING, Compat::Engine),
    );
    assert_ne!(sa.average.to_bits(), sb.average.to_bits());
}

// A small polygon shapefile, written directly.
fn write_shp(base: &Path, shapes: &[Vec<Vec<Pt>>], names: &[&str], prj: Option<&str>) {
    let mut shp = Vec::new();
    let mut shx = Vec::new();
    let mut at = 50i32;
    let mut records = Vec::new();
    for (i, parts) in shapes.iter().enumerate() {
        let total: usize = parts.iter().map(Vec::len).sum();
        let mut c: Vec<u8> = Vec::new();
        c.extend(5i32.to_le_bytes());
        c.extend([0u8; 32]);
        c.extend((parts.len() as i32).to_le_bytes());
        c.extend((total as i32).to_le_bytes());
        let mut offset = 0i32;
        for p in parts {
            c.extend(offset.to_le_bytes());
            offset += p.len() as i32;
        }
        for p in parts.iter().flatten() {
            c.extend(p.x.to_le_bytes());
            c.extend(p.y.to_le_bytes());
        }
        let words = (c.len() / 2) as i32;
        records.extend(((i + 1) as i32).to_be_bytes());
        records.extend(words.to_be_bytes());
        records.extend(c);
        shx.extend(at.to_be_bytes());
        shx.extend(words.to_be_bytes());
        at += 4 + words;
    }
    let header = |words: i32| {
        let mut h = Vec::new();
        h.extend(9994i32.to_be_bytes());
        h.extend([0u8; 20]);
        h.extend(words.to_be_bytes());
        h.extend(1000i32.to_le_bytes());
        h.extend(5i32.to_le_bytes());
        h.extend([0u8; 64]);
        h
    };
    shp.extend(header(at));
    shp.extend(records);
    let mut index = header(50 + 4 * shapes.len() as i32);
    index.extend(shx);
    std::fs::write(base.with_extension("shp"), shp).unwrap();
    std::fs::write(base.with_extension("shx"), index).unwrap();
    let mut dbf = vec![3u8, 126, 1, 1];
    dbf.extend((names.len() as u32).to_le_bytes());
    dbf.extend(65u16.to_le_bytes());
    dbf.extend(17u16.to_le_bytes());
    dbf.extend([0u8; 20]);
    let mut desc = [0u8; 32];
    desc[..4].copy_from_slice(b"Name");
    desc[11] = b'C';
    desc[16] = 16;
    dbf.extend(desc);
    dbf.push(0x0D);
    for n in names {
        dbf.push(b' ');
        dbf.extend(format!("{n:<16}").as_bytes());
    }
    std::fs::write(base.with_extension("dbf"), dbf).unwrap();
    if let Some(text) = prj {
        std::fs::write(base.with_extension("prj"), text).unwrap();
    }
}

fn cw_box(c0: f64, r0: f64, c1: f64, r1: f64) -> Vec<Pt> {
    vec![pt(c0, r0), pt(c1, r0), pt(c1, r1), pt(c0, r1), pt(c0, r0)]
}

fn read(path: &Path) -> Result<ZoneSet, VortexError> {
    ZoneSet::read(path, "Name", None, &ZoneLimits::default())
}

#[test]
fn every_byte_of_a_zone_file_can_be_wrong() {
    // None of these may panic or abort: the run would not reach a refusal otherwise.
    let dir = scratch("mutate");
    let base = dir.join("z");
    write_shp(
        &base,
        &[
            vec![cw_box(2.0, 2.0, 8.0, 6.0), cw_box(4.0, 3.0, 5.0, 4.0)],
            vec![cw_box(10.0, 2.0, 14.0, 6.0)],
        ],
        &["A", "B"],
        Some(SHG_WKT),
    );
    let grid = sample_grid();
    for extension in ["shp", "shx", "dbf"] {
        let path = base.with_extension(extension);
        let original = std::fs::read(&path).unwrap();
        for position in 0..original.len() {
            for value in [0u8, 0x7F, 0xFF] {
                let mut bytes = original.clone();
                bytes[position] = value;
                std::fs::write(&path, &bytes).unwrap();
                if let Ok(set) = read(&base.with_extension("shp")) {
                    // Whatever parsed must rasterize or refuse; NaN and huge coordinates included.
                    let _ = zonal::zone_masks(
                        &set,
                        &grid,
                        DatumPolicy::Refuse,
                        &ZoneLimits {
                            max_scan_work: 1 << 28,
                            ..ZoneLimits::default()
                        },
                    );
                }
            }
        }
        for keep in 0..original.len() {
            std::fs::write(&path, &original[..keep]).unwrap();
            let _ = read(&base.with_extension("shp"));
        }
        std::fs::write(&path, &original).unwrap();
    }
    assert_eq!(read(&base.with_extension("shp")).unwrap().zones.len(), 2);
}

#[test]
fn declared_counts_are_never_believed() {
    // Each would ask for terabytes if a count were trusted; an abort here ends the whole test run.
    let dir = scratch("counts");
    let base = dir.join("z");
    write_shp(
        &base,
        &[vec![cw_box(2.0, 2.0, 8.0, 6.0)]],
        &["A"],
        Some(SHG_WKT),
    );
    let shp = base.with_extension("shp");
    let good = std::fs::read(&shp).unwrap();
    // Record content starts at 108: type, box, parts at +36, points at +40.
    for (at, what) in [(108 + 36, "parts"), (108 + 40, "points")] {
        let mut bytes = good.clone();
        bytes[at..at + 4].copy_from_slice(&i32::MAX.to_le_bytes());
        std::fs::write(&shp, &bytes).unwrap();
        assert!(read(&shp).is_err(), "{what}");
    }
    std::fs::write(&shp, &good).unwrap();
    let dbf = base.with_extension("dbf");
    let mut bytes = std::fs::read(&dbf).unwrap();
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&dbf, &bytes).unwrap();
    assert!(read(&shp).is_err(), "records");
}

#[test]
fn limits_refuse_before_the_work() {
    let dir = scratch("limits");
    let base = dir.join("z");
    write_shp(
        &base,
        &[vec![cw_box(2.0, 2.0, 8.0, 6.0)]],
        &["A"],
        Some(SHG_WKT),
    );
    let shp = base.with_extension("shp");
    let refused = |limits: ZoneLimits| {
        ZoneSet::read(&shp, "Name", None, &limits)
            .and_then(|set| zonal::zone_masks(&set, &sample_grid(), DatumPolicy::Refuse, &limits))
    };
    let default = ZoneLimits::default();
    assert!(refused(default).is_ok());
    for limits in [
        ZoneLimits {
            max_file_bytes: 10,
            ..default
        },
        ZoneLimits {
            max_points: 2,
            ..default
        },
        ZoneLimits {
            max_features: 0,
            ..default
        },
        ZoneLimits {
            max_cells: 100,
            ..default
        },
        ZoneLimits {
            max_scan_work: 10,
            ..default
        },
        ZoneLimits {
            max_mask_entries: 3,
            ..default
        },
    ] {
        assert!(
            matches!(refused(limits), Err(VortexError::Refused { .. })),
            "{limits:?}"
        );
    }
}

#[test]
fn geojson_nesting_and_rubbish_are_refused_without_a_crash() {
    let dir = scratch("geojson");
    let path = dir.join("z.geojson");
    let cases: Vec<Vec<u8>> = vec![
        vec![b'['; 5_000_000],
        [b"{\"a\":".repeat(2_000_000), b"1".to_vec()].concat(),
        b"".to_vec(),
        b"{".to_vec(),
        b"{\"type\":\"FeatureCollection\",\"features\":[".to_vec(),
        b"{\"type\":\"FeatureCollection\"}".to_vec(),
        b"{\"type\":\"Point\",\"coordinates\":[0,0]}".to_vec(),
        b"\"\\ud800\"".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Name\":\"\xff\"},\"geometry\":null}".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Name\":\"A\"},\"geometry\":{\"type\":\"Polygon\",\"coordinates\":[[[1e999,0]]]}}".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Name\":\"A\"},\"geometry\":{\"type\":\"Blob\",\"coordinates\":[]}}".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Name\":true},\"geometry\":null}".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Name\":1.5},\"geometry\":null}".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Other\":\"A\"},\"geometry\":null}".to_vec(),
        b"{\"type\":\"Feature\",\"properties\":{\"Name\":\"A\"},\"geometry\":null} trailing".to_vec(),
    ];
    for (i, bytes) in cases.iter().enumerate() {
        std::fs::write(&path, bytes).unwrap();
        assert!(read(&path).is_err(), "case {i} should be refused");
    }
}

#[test]
fn geojson_reads_each_geometry_and_names_by_field_with_no_case() {
    let dir = scratch("geojson_ok");
    let path = dir.join("z.geojson");
    std::fs::write(
        &path,
        br#"{"type":"FeatureCollection","features":[
          {"type":"Feature","properties":{"NAME":"A"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}},
          {"type":"Feature","properties":{"name":7},"geometry":{"type":"MultiPoint","coordinates":[[0,0],[1,1]]}},
          {"type":"Feature","properties":{"Name":"A"},"geometry":{"type":"GeometryCollection","geometries":[
              {"type":"LineString","coordinates":[[0,0],[1,1,5]]},{"type":"Point","coordinates":[2,2]}]}}
        ]}"#,
    )
    .unwrap();
    let set = read(&path).unwrap();
    assert_eq!(set.zones.len(), 2);
    assert_eq!(set.zones[0].id, "A");
    assert_eq!(set.zones[1].id, "7");
    assert_eq!(
        set.zones[0].parts.len(),
        3,
        "features sharing a name combine"
    );
    assert!(matches!(set.zones[1].parts[0], Part::Point(_)));
    assert!(set.crs.is_geographic());
}

#[test]
fn a_crs_is_never_guessed() {
    let dir = scratch("crs");
    let base = dir.join("z");
    write_shp(&base, &[vec![cw_box(2.0, 2.0, 8.0, 6.0)]], &["A"], None);
    let shp = base.with_extension("shp");
    let text = read(&shp).unwrap_err().to_string();
    assert!(text.contains("no .prj"), "{text}");
    // Stated by the caller, it works, and gives the same mask as the same CRS in a .prj.
    let stated = ZoneSet::read(
        &shp,
        "Name",
        Some(&Crs::Wkt(SHG_WKT.into())),
        &ZoneLimits::default(),
    )
    .unwrap();
    let cells = zonal::zone_masks(
        &stated,
        &sample_grid(),
        DatumPolicy::Refuse,
        &ZoneLimits::default(),
    )
    .unwrap();
    assert_eq!(cells[0].cells.len(), 6 * 4);
    // Both at once are refused, whichever is right.
    std::fs::write(base.with_extension("prj"), SHG_WKT).unwrap();
    assert!(ZoneSet::read(
        &shp,
        "Name",
        Some(&Crs::Wkt(SHG_WKT.into())),
        &ZoneLimits::default()
    )
    .is_err());
    // A datum this library does not shift.
    std::fs::write(base.with_extension("prj"), "GEOGCS[\"NAD27\",DATUM[\"North_American_Datum_1927\",SPHEROID[\"Clarke 1866\",6378206.4,294.9786982]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]").unwrap();
    let set = read(&shp).unwrap();
    assert!(zonal::zone_masks(
        &set,
        &sample_grid(),
        DatumPolicy::Refuse,
        &ZoneLimits::default()
    )
    .is_err());
    // Web Mercator is for zones only.
    let web = "PROJCS[\"WGS_1984_Web_Mercator_Auxiliary_Sphere\",GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Mercator_Auxiliary_Sphere\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",0.0],PARAMETER[\"Standard_Parallel_1\",0.0],PARAMETER[\"Auxiliary_Sphere_Type\",0.0],UNIT[\"Meter\",1.0]]";
    assert!(CrsDef::parse_wkt(web).is_err());
    let parsed = CrsDef::parse_zone_wkt(web).unwrap();
    // PROJ: 0, 45 degrees north on EPSG:3857 is y = 5621521.486192066.
    let to_lonlat = dss_vortex::crs::Transformer::new(
        &parsed,
        &CrsDef::sphere_lonlat(6378137.0),
        DatumPolicy::Refuse,
    )
    .unwrap();
    let (lon, lat) = to_lonlat
        .transform(1113194.9079327357, 5621521.486192066)
        .unwrap();
    assert!((lon - 10.0).abs() < 1e-9, "{lon}");
    assert!((lat - 45.0).abs() < 1e-9, "{lat}");
    let bad_type = web.replace("Auxiliary_Sphere_Type\",0.0", "Auxiliary_Sphere_Type\",1.0");
    assert!(CrsDef::parse_zone_wkt(&bad_type).is_err());
    // A GeoJSON crs member this library does not name.
    let gj = dir.join("g.geojson");
    std::fs::write(&gj, br#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"EPSG:26915"}},"features":[{"type":"Feature","properties":{"Name":"A"},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#).unwrap();
    assert!(read(&gj).unwrap_err().to_string().contains("EPSG:26915"));
    assert!(ZoneSet::read(
        &gj,
        "Name",
        Some(&Crs::Wkt(SHG_WKT.into())),
        &ZoneLimits::default()
    )
    .is_ok());
}

#[test]
fn missing_field_empty_name_and_refused_runs_leave_files_alone() {
    let dir = fixtures();
    let work = scratch("refusal");
    let grids = dir.join("regular_1h.dss");
    let zones = dir.join("polys.shp");
    let new_output = work.join("new.dss");
    let existing = work.join("existing.dss");
    create_dss7(&existing).unwrap();
    let before = std::fs::read(&existing).unwrap();
    for output in [&new_output, &existing] {
        let mut r = run_request(&grids, &zones, "NOPE", output);
        r.accumulate = false;
        let e = zonal::grid_to_point(&r).unwrap_err();
        assert_eq!(e.phase, "planning");
        assert!(e.completed.is_empty());
    }
    assert!(!new_output.exists(), "a refused run creates nothing");
    assert_eq!(std::fs::read(&existing).unwrap(), before);
    // Statistic asked for twice, and an empty list.
    let mut r = run_request(&grids, &zones, "Name", &new_output);
    r.statistics = vec![Statistic::Min, Statistic::Min];
    assert!(zonal::grid_to_point(&r).is_err());
    r.statistics = vec![];
    assert!(zonal::grid_to_point(&r).is_err());
    assert!(!new_output.exists());
    // Not a DSS file at the output.
    let text = work.join("text.dss");
    std::fs::write(
        &text,
        b"not a dss file at all, and long enough to be read as a header",
    )
    .unwrap();
    let r = run_request(&grids, &zones, "Name", &text);
    assert!(zonal::grid_to_point(&r).is_err());
    assert_eq!(std::fs::read(&text).unwrap()[..8], *b"not a ds");
}

fn pathnames(path: &Path) -> Vec<String> {
    let mut f = Dss7File::open(path).unwrap();
    let c = f.catalog().unwrap();
    let mut names: Vec<String> = c
        .catalog
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.pathname).into_owned())
        .collect();
    names.sort();
    names
}

fn grid_bits(path: &Path, name: &str) -> Vec<u32> {
    let mut f = Dss7File::open(path).unwrap();
    let g = f.read_grid(name.as_bytes()).unwrap();
    g.data.iter().map(|v| v.to_bits()).collect()
}

#[test]
fn a_crash_at_every_write_step_heals_on_rerun_and_other_records_stay() {
    let dir = fixtures();
    let work = scratch("crash");
    let source = dir.join("regular_1h.dss");
    let zones = dir.join("polys.shp");
    // The clean run, into a copy of the grids themselves, so other records sit beside the new ones.
    let clean = work.join("clean.dss");
    std::fs::copy(&source, &clean).unwrap();
    let grids_before = pathnames(&clean);
    assert!(!grids_before.is_empty());
    let first_grid = grids_before[0].clone();
    let cells_before = grid_bits(&clean, &first_grid);
    let mut in_place = run_request(&clean, &zones, "Name", &clean);
    in_place.allow_in_place = true;
    let outcome = zonal::grid_to_point(&in_place).unwrap();
    assert!(outcome.records.len() > 20);
    let clean_names = pathnames(&clean);
    assert_eq!(
        clean_names.len(),
        grids_before.len() + outcome.records.len()
    );
    assert_eq!(grid_bits(&clean, &first_grid), cells_before);
    for step in WriteStep::ALL {
        let copy = work.join(format!("{step:?}.dss"));
        std::fs::copy(&source, &copy).unwrap();
        let mut r = run_request(&source, &zones, "Name", &copy);
        r.allow_in_place = true;
        r.zonal.grids = copy.clone();
        r.fault = Some((0, FaultPoint { block: 0, step }));
        // A step this record's write does not reach gives `Ok`.
        if let Err(e) = zonal::grid_to_point(&r) {
            assert_eq!(e.phase, "writing", "{step:?}");
            assert!(matches!(
                e.failure,
                GridToPointFailure::Write(WriteError::InjectedFault { .. })
            ));
            assert!(e.completed.is_empty());
            assert!(e.in_doubt.is_some(), "{step:?}: a write had begun");
        }
        r.fault = None;
        zonal::grid_to_point(&r).unwrap_or_else(|e| panic!("{step:?}: {e}"));
        assert_eq!(pathnames(&copy), clean_names, "{step:?}");
        assert_eq!(grid_bits(&copy, &first_grid), cells_before, "{step:?}");
    }
}

#[test]
fn each_statistic_is_written_under_its_own_name() {
    let dir = fixtures();
    let work = scratch("identity");
    let grids = dir.join("regular_1h.dss");
    let zones = dir.join("polys.shp");
    let output = work.join("out.dss");
    let mut r = run_request(&grids, &zones, "Name", &output);
    r.parts = PointParts {
        a: Some("BASIN".into()),
        f: Some("RUN".into()),
    };
    let outcome = zonal::grid_to_point(&r).unwrap();
    let table = zonal::compute(&r.zonal).unwrap();
    let n = table.zones.len();
    let zone = table.zones.iter().position(|z| z.id == "SQUARE").unwrap();
    let mut f = Dss7File::open(&output).unwrap();
    let mut seen = 0;
    for (name, suffix, pick, units) in [
        ("average", "", 0, "MM"),
        ("min", "-MIN", 1, "MM"),
        ("max", "-MAX", 2, "MM"),
        ("median", "-MEDIAN", 3, "MM"),
        ("q1", "-Q1", 4, "MM"),
        ("q3", "-Q3", 5, "MM"),
        ("pct_gt_0", "-PCT>0", 6, "%"),
        ("pct_gt_q1", "-PCT>Q1", 7, "%"),
    ] {
        let record = outcome
            .records
            .iter()
            .find(|x| x.zone == "SQUARE" && x.statistic == name)
            .unwrap();
        let block = format!("/BASIN/SQUARE/PRECIP-INC{suffix}/01Mar2020/1Hour/RUN/");
        assert_eq!(
            record.pathname,
            format!("/BASIN/SQUARE/PRECIP-INC{suffix}//1Hour/RUN/")
        );
        assert_eq!(record.units, units);
        let read = f
            .read_ts(
                block.as_bytes(),
                &dss_core::dss7::ts::TsReadOptions::default(),
            )
            .unwrap();
        let TsValues::F32(values) = read.ts.values else {
            panic!("{name}: records store float32, as HEC's writer does from Vortex's doubles");
        };
        let stored: Vec<f32> = values.into_iter().filter(|v| *v > -1e30).collect();
        let want: Vec<f32> = (0..table.frames.len())
            .map(|i| {
                let s = &table.stats[i * n + zone];
                [
                    s.average,
                    s.min,
                    s.max,
                    s.median,
                    s.first_quartile,
                    s.third_quartile,
                    s.pct_gt_zero,
                    s.pct_gt_first_quartile,
                ][pick] as f32
            })
            .collect();
        assert_eq!(stored, want, "{name}");
        seen += 1;
    }
    assert_eq!(seen, 8);
    // Each zone has its own B part: another zone's values differ from this one's.
    let other = table
        .zones
        .iter()
        .position(|z| z.id == "CCW_OUTER")
        .unwrap();
    assert_ne!(table.stats[zone].average, table.stats[other].average);
}

#[test]
fn zones_with_no_valid_cell_are_missing_not_zero() {
    let dir = fixtures();
    let table = zonal::compute(&request(
        &dir.join("regular_1h.dss"),
        &dir.join("polys.shp"),
        "Name",
    ))
    .unwrap();
    let outside = table.zones.iter().position(|z| z.id == "OUTSIDE").unwrap();
    assert!(table.zones[outside].cells.is_empty());
    assert!(table.zones[outside].centroid_fallback);
    let s = &table.stats[outside];
    for v in [
        s.average,
        s.min,
        s.max,
        s.median,
        s.first_quartile,
        s.third_quartile,
        s.pct_gt_zero,
        s.pct_gt_first_quartile,
    ] {
        assert_eq!(v, MISSING);
    }
    // A real zero is still a zero: a control that the line above is not simply always missing.
    let dry = zonal::statistics_of(&mut [0.0, 0.0, 0.0, 0.0], MISSING, Compat::Engine);
    assert_eq!((dry.average, dry.pct_gt_zero), (0.0, 0.0));
}

#[test]
fn a_feature_with_no_geometry_is_a_zone_with_no_cells() {
    let work = scratch("null_geometry");
    let base = work.join("z");
    write_shp(
        &base,
        &[
            vec![cw_box(2.0, 2.0, 8.0, 6.0)],
            vec![cw_box(10.0, 2.0, 14.0, 6.0)],
        ],
        &["A", "NOGEOM"],
        Some(SHG_WKT),
    );
    let shp = base.with_extension("shp");
    let mut bytes = std::fs::read(&shp).unwrap();
    // The second record's type: header, then one part of five points.
    let second = 100 + (8 + 44 + 4 + 5 * 16);
    bytes[second + 8..second + 12].copy_from_slice(&0i32.to_le_bytes());
    std::fs::write(&shp, &bytes).unwrap();
    let set = read(&shp).unwrap();
    let masks = zonal::zone_masks(
        &set,
        &sample_grid(),
        DatumPolicy::Refuse,
        &ZoneLimits::default(),
    )
    .unwrap();
    assert_eq!(masks[0].cells.len(), 24);
    assert!(masks[1].cells.is_empty());
    assert!(
        !masks[1].centroid_fallback,
        "there is no extent to take a middle of"
    );
}

fn area_table(zones: &Path) -> Vec<(String, f64)> {
    let dir = fixtures();
    let mut r = request(&dir.join("regular_1h.dss"), zones, "Name");
    r.area_weighted = true;
    let t = zonal::compute(&r).unwrap();
    t.zones
        .iter()
        .enumerate()
        .map(|(z, m)| (m.id.clone(), t.area_weighted.as_ref().unwrap()[z]))
        .collect()
}

#[test]
fn the_area_weighted_mean_is_its_own_option_and_weighs_by_area() {
    let dir = fixtures();
    let work = scratch("area");
    // Cell-aligned box: every share is 1, so it equals the cell-centre average exactly enough.
    let base = work.join("aligned");
    write_shp(
        &base,
        &[vec![cw_box(2.0, 2.0, 8.0, 6.0)]],
        &["A"],
        Some(SHG_WKT),
    );
    let plain = zonal::compute(&request(
        &dir.join("regular_1h.dss"),
        &base.with_extension("shp"),
        "Name",
    ))
    .unwrap();
    assert!(
        plain.area_weighted.is_none(),
        "not computed unless asked for"
    );
    let weighted = area_table(&base.with_extension("shp"));
    let cells = 24.0;
    assert!((weighted[0].1 - plain.stats[0].average).abs() * cells < 1e-9);
    // Half of each of two cells weighs them equally, where the cell-centre rule might take either.
    let half = work.join("half");
    write_shp(
        &half,
        &[vec![cw_box(2.5, 2.0, 3.5, 3.0)]],
        &["A"],
        Some(SHG_WKT),
    );
    let a = area_table(&half.with_extension("shp"))[0].1;
    let whole = |c: f64| {
        let b = work.join(format!("c{c}"));
        write_shp(
            &b,
            &[vec![cw_box(c, 2.0, c + 1.0, 3.0)]],
            &["A"],
            Some(SHG_WKT),
        );
        area_table(&b.with_extension("shp"))[0].1
    };
    assert!((a - (whole(2.0) + whole(3.0)) / 2.0).abs() < 1e-6, "{a}");
    // A hole removes its share; the hole here is the whole of the cells in it.
    let hole = work.join("hole");
    write_shp(
        &hole,
        &[vec![cw_box(2.0, 2.0, 4.0, 3.0), {
            let mut r = cw_box(3.0, 2.0, 4.0, 3.0);
            r.reverse();
            r
        }]],
        &["A"],
        Some(SHG_WKT),
    );
    assert!((area_table(&hole.with_extension("shp"))[0].1 - whole(2.0)).abs() < 1e-6);
    // Two polygons over one cell would count it twice.
    let over = work.join("over");
    write_shp(
        &over,
        &[
            vec![cw_box(2.0, 2.0, 4.0, 4.0)],
            vec![cw_box(2.0, 2.0, 4.0, 4.0)],
        ],
        &["A", "A"],
        Some(SHG_WKT),
    );
    let mut r = request(
        &dir.join("regular_1h.dss"),
        &over.with_extension("shp"),
        "Name",
    );
    r.area_weighted = true;
    assert!(zonal::compute(&r)
        .unwrap_err()
        .to_string()
        .contains("overlap"));
    // Outside the grid: missing.
    let out = work.join("out");
    write_shp(
        &out,
        &[vec![cw_box(500.0, 500.0, 510.0, 510.0)]],
        &["A"],
        Some(SHG_WKT),
    );
    assert_eq!(area_table(&out.with_extension("shp"))[0].1, MISSING);
}

#[test]
fn a_mask_rule_by_hand() {
    // Pixel-level facts of GDAL's rule, independent of the fixture: cell centres at x.5, and an edge
    // through a centre on the left takes the cell, and on the right does not.
    let dir = scratch("rule");
    let base = dir.join("z");
    write_shp(
        &base,
        &[
            vec![cw_box(2.5, 2.0, 4.5, 3.0)],
            vec![cw_box(10.2, 2.2, 10.4, 2.4)],
            vec![cw_box(12.9, 2.2, 13.1, 2.4)],
        ],
        &["TIE", "SUB", "EDGE"],
        Some(SHG_WKT),
    );
    let set = read(&base.with_extension("shp")).unwrap();
    let m = zonal::zone_masks(
        &set,
        &sample_grid(),
        DatumPolicy::Refuse,
        &ZoneLimits::default(),
    )
    .unwrap();
    let row = 2 * 60;
    assert_eq!(m[0].cells, vec![row + 3, row + 4]);
    assert!(!m[0].centroid_fallback);
    assert_eq!(
        m[1].cells,
        vec![row + 10],
        "the cell holding the extent's middle"
    );
    assert!(m[1].centroid_fallback);
    assert_eq!(
        m[2].cells,
        Vec::<u32>::new(),
        "a middle exactly on a cell edge is in no cell"
    );
    assert!(m[2].centroid_fallback);
}

/// A pathname with no D part, as a run reports it.
fn series_name(pathname: &str) -> String {
    let parts: Vec<&str> = pathname.split('/').collect();
    format!(
        "/{}/{}/{}//{}/{}/",
        parts[1], parts[2], parts[3], parts[5], parts[6]
    )
}

fn f32s(path: &Path, block: &str) -> Option<Vec<f32>> {
    let mut f = Dss7File::open(path).ok()?;
    let read = f
        .read_ts(
            block.as_bytes(),
            &dss_core::dss7::ts::TsReadOptions::default(),
        )
        .ok()?;
    match read.ts.values {
        TsValues::F32(v) => Some(v),
        _ => None,
    }
}

const WRITE_STEPS_THAT_FOLLOW_PUBLISHING: [WriteStep; 4] = [
    WriteStep::Publish,
    WriteStep::Link,
    WriteStep::Activate,
    WriteStep::Finish,
];

#[test]
fn a_failure_accounts_for_every_series_the_file_shows() {
    let dir = fixtures();
    let work = scratch("accounting");
    let (grids, zones) = (dir.join("regular_1h.dss"), dir.join("polys.shp"));
    for step in WriteStep::ALL {
        let out = work.join(format!("{step:?}.dss"));
        let mut r = run_request(&grids, &zones, "Name", &out);
        r.statistics = vec![Statistic::Average];
        r.accumulate = false;
        r.fault = Some((2, FaultPoint { block: 0, step }));
        let Err(e) = zonal::grid_to_point(&r) else {
            continue;
        };
        assert_eq!(e.phase, "writing", "{step:?}");
        assert_eq!(e.completed.len(), 2, "{step:?}: the writes acknowledged");
        let doubted = e.in_doubt.clone().expect("the failed series is in doubt");
        assert!(!e.completed.iter().any(|c| c.pathname == doubted));
        assert!(e.to_string().contains(&doubted), "{e}");
        assert!(e.to_string().contains("does not mean"), "{e}");
        let visible: Vec<String> = pathnames(&out).iter().map(|p| series_name(p)).collect();
        for done in &e.completed {
            assert!(
                visible.contains(&done.pathname),
                "{step:?}: {}",
                done.pathname
            );
            assert_eq!(done.outcome, "added");
        }
        for name in &visible {
            assert!(
                e.completed.iter().any(|c| &c.pathname == name) || name == &doubted,
                "{step:?}: {name} is neither acknowledged nor in doubt"
            );
        }
    }
}

#[test]
fn a_failure_leaves_a_changed_series_readable_with_nothing_completed() {
    let dir = fixtures();
    let work = scratch("changed_series");
    let (grids, zones) = (dir.join("regular_1h.dss"), dir.join("polys.shp"));
    let block = "//SQUARE/PRECIP-INC/01Mar2020/1Hour//";
    let clean = work.join("clean.dss");
    let mut base = run_request(&grids, &zones, "Name", &clean);
    base.statistics = vec![Statistic::Average];
    base.accumulate = false;
    zonal::grid_to_point(&base).unwrap();
    let new_values = f32s(&clean, block).unwrap();
    let mut changed = 0;
    for step in WRITE_STEPS_THAT_FOLLOW_PUBLISHING {
        let out = work.join(format!("{step:?}.dss"));
        create_dss7(&out).unwrap();
        // The series as an earlier run left it: other values at the same stamps.
        let old: Vec<f32> = new_values.iter().map(|v| v + 1000.0).collect();
        let start = 43890 * 86400 + 3600;
        write_rts(
            &out,
            &RtsInput::new(
                block.as_bytes(),
                start,
                RtsValues::F32(&old[..6]),
                b"MM",
                b"PER-CUM",
            ),
            &WriteOptions::default(),
        )
        .unwrap();
        let before = f32s(&out, block).unwrap();
        assert_ne!(before[..6], new_values[..6]);
        let mut r = base.clone();
        r.output = out.clone();
        r.fault = Some((0, FaultPoint { block: 0, step }));
        let e = zonal::grid_to_point(&r).unwrap_err();
        assert!(e.completed.is_empty(), "{step:?}");
        assert_eq!(
            e.in_doubt.as_deref(),
            Some("//SQUARE/PRECIP-INC//1Hour//"),
            "{step:?}"
        );
        // Step by step, the old or the new values: the failure itself proves neither.
        if let Some(now) = f32s(&out, block) {
            if now[..6] == new_values[..6] {
                changed += 1;
            } else {
                assert_eq!(now[..6], before[..6], "{step:?}: a torn block");
            }
        }
        // A rerun settles it.
        r.fault = None;
        zonal::grid_to_point(&r).unwrap();
        assert_eq!(f32s(&out, block).unwrap()[..6], new_values[..6]);
    }
    assert!(
        changed > 0,
        "no step left the new values readable, which this test exists to show"
    );
}

/// Hourly frames across the end of March, so one series takes two monthly blocks.
fn across_two_blocks(out: &Path, value: f32) {
    use dss_vortex::import::{self, ArrayInput, ImportRequest, Selection};
    use dss_vortex::warp::Method;
    use dss_vortex::{FrameTime, GeoTransform, NamedCrs};
    let at = |h: i64| {
        let t = 43921 * 86400 - 3600 + h * 3600;
        (t.div_euclid(86400), t.rem_euclid(86400))
    };
    let times: Vec<FrameTime> = (0..3)
        .map(|h| FrameTime::Period {
            start: at(h),
            end: at(h + 1),
        })
        .collect();
    let (nx, ny) = (60, 40);
    let input = ArrayInput {
        cells: vec![value; nx * ny * times.len()],
        nx,
        ny,
        transform: GeoTransform::axis_aligned(-100000.0, 2000.0, 1000000.0, -2000.0),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: f32::NAN,
        times,
        variable: "PRECIPITATION".into(),
        description: None,
        units: "MM".into(),
        file_name: String::new(),
    };
    let mut request = ImportRequest::new(
        Vec::new(),
        out.to_path_buf(),
        Selection::Variable("PRECIPITATION".into()),
        Method::Near,
    );
    request.allow_undetectable_readers = true;
    import::import_arrays(&input, &request, &mut |_| {
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
}

#[test]
fn a_failure_between_blocks_leaves_a_series_part_new_and_says_so() {
    let dir = fixtures();
    let work = scratch("two_blocks");
    let zones = dir.join("polys.shp");
    let (first, second) = (work.join("first.dss"), work.join("second.dss"));
    across_two_blocks(&first, 10.0);
    across_two_blocks(&second, 20.0);
    let out = work.join("out.dss");
    let mut r = run_request(&first, &zones, "Name", &out);
    r.statistics = vec![Statistic::Average];
    r.accumulate = false;
    zonal::grid_to_point(&r).unwrap();
    // With no window a read takes the whole series: March holds one value here, and April two.
    let series = |path: &Path| -> Vec<f32> {
        f32s(path, "//SQUARE/PRECIP-INC/01Mar2020/1Hour//")
            .unwrap()
            .into_iter()
            .filter(|v| *v > -1e30 && *v != -901.0)
            .collect()
    };
    assert_eq!(series(&out), vec![10.0, 10.0, 10.0]);
    assert_eq!(
        pathnames(&out)
            .iter()
            .filter(|p| p.contains("/SQUARE/PRECIP-INC/"))
            .count(),
        2,
        "one series in two blocks"
    );
    r.zonal.grids = second;
    r.fault = Some((
        0,
        FaultPoint {
            block: 1,
            step: WriteStep::Data,
        },
    ));
    let e = zonal::grid_to_point(&r).unwrap_err();
    assert!(e.completed.is_empty());
    assert_eq!(e.in_doubt.as_deref(), Some("//SQUARE/PRECIP-INC//1Hour//"));
    assert_eq!(
        series(&out),
        vec![20.0, 10.0, 10.0],
        "the first block committed, and the second did not"
    );
    r.fault = None;
    let done = zonal::grid_to_point(&r).unwrap();
    assert_eq!(done.records[0].outcome, "window_updated");
    assert_eq!(series(&out), vec![20.0, 20.0, 20.0]);
}

#[test]
fn an_output_that_is_an_input_is_refused_by_the_file_itself() {
    let dir = fixtures();
    let work = scratch("aliases");
    // Copied, as a hard link cannot cross drives, and scratch need not be on the fixtures' drive.
    for ext in ["shp", "shx", "dbf", "prj"] {
        let name = format!("polys.{ext}");
        if dir.join(&name).exists() {
            std::fs::copy(dir.join(&name), work.join(&name)).unwrap();
        }
    }
    let zones = work.join("polys.shp");
    let grids = work.join("grids.dss");
    std::fs::copy(dir.join("regular_1h.dss"), &grids).unwrap();
    let link = work.join("link.dss");
    std::fs::hard_link(&grids, &link).unwrap();
    let before = std::fs::read(&grids).unwrap();
    let spelled = work.join(".").join("grids.dss");
    for output in [&grids, &link, &spelled] {
        let e = zonal::grid_to_point(&run_request(&grids, &zones, "Name", output)).unwrap_err();
        assert_eq!(e.phase, "planning");
        assert!(e.in_doubt.is_none());
        assert!(e.to_string().contains("also the input"), "{e}");
        assert_eq!(std::fs::read(&grids).unwrap(), before);
    }
    // A zone file is no output, even when asked.
    let zone_link = work.join("zones.dss");
    std::fs::hard_link(&zones, &zone_link).unwrap();
    let mut r = run_request(&grids, &zones, "Name", &zone_link);
    r.allow_in_place = true;
    let e = zonal::grid_to_point(&r).unwrap_err();
    assert!(e.to_string().contains("also the input"), "{e}");
    // Stated in place: the series go beside the grids, through either name.
    let names_before = pathnames(&grids);
    let first = names_before[0].clone();
    let cells = grid_bits(&grids, &first);
    let mut r = run_request(&grids, &zones, "Name", &link);
    r.allow_in_place = true;
    let done = zonal::grid_to_point(&r).unwrap();
    assert_eq!(
        pathnames(&grids).len(),
        names_before.len() + done.records.len()
    );
    assert_eq!(grid_bits(&grids, &first), cells);
    assert_ne!(
        std::fs::read(&grids).unwrap(),
        before,
        "in place does change the file"
    );
}

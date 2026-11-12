//! P71: PRISM BIL, rasters in ZIP archives, and SNODAS, through `import::open_source`, against
//! Vortex 1.1.0's and 0.13.3's reading of the same real public files (`fixtures/public/p71`; Vortex's
//! reading is recorded in its `expected.json`).

use dss_vortex::import::{open_source, OpenOptions};
use dss_vortex::source::raster::{RasterFile, ReadOptions};
use dss_vortex::source::snodas::{
    self, SnodasFile, SnodasGeoreference, SnodasOptions, SnodasTimes,
};
use dss_vortex::source::zipped::read_zip;
use dss_vortex::{DataType, FrameTime};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

const STEM: &str = "us_ssmv11039lL00T0024TTNATS2020010105DP000";
const PRISM: &str = "PRISM_ppt_stable_4kmD1_19990101_bil.zip";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/p71")
}

fn expected() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixtures().join("expected.json")).unwrap())
        .unwrap()
}

// SHA-256 (FIPS 180-4): the one copy in `tests/support/sha256.rs`.
#[path = "support/sha256.rs"]
mod sha256_support;
use sha256_support::sha256;

fn cells_le(cells: &[f32]) -> Vec<u8> {
    cells
        .iter()
        .flat_map(|c| c.to_bits().to_le_bytes())
        .collect()
}

/// `ZonedDateTime.toString` in UTC, from the frame's (day since 1899-12-31, second).
fn java_time(t: (i64, i64)) -> String {
    // Days since 1970-01-01, then Howard Hinnant's civil_from_days.
    let z = t.0 - 25_568 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}Z[UTC]",
        t.1 / 3600,
        t.1 % 3600 / 60
    )
}

fn times(t: &FrameTime) -> (String, String) {
    match t {
        FrameTime::Invariant => ("None".into(), "None".into()),
        FrameTime::Instant(x) => (java_time(*x), java_time(*x)),
        FrameTime::Period { start, end } => (java_time(*start), java_time(*end)),
    }
}

fn java_type(t: DataType) -> &'static str {
    match t {
        DataType::PerCum => "ACCUMULATION",
        DataType::PerAver => "AVERAGE",
        _ => "INSTANTANEOUS",
    }
}

fn vortex_times() -> OpenOptions {
    OpenOptions {
        snodas: SnodasOptions {
            times: SnodasTimes::Vortex,
            ..SnodasOptions::default()
        },
        ..OpenOptions::default()
    }
}

fn double(v: &Value) -> f64 {
    v.as_str().unwrap().parse().unwrap()
}

#[test]
fn fixtures_are_the_downloaded_bytes() {
    let e = expected();
    for (name, meta) in e["files"].as_object().unwrap() {
        let bytes = std::fs::read(fixtures().join(name)).unwrap();
        assert_eq!(
            bytes.len() as u64,
            meta["bytes"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(sha256(&bytes), meta["sha256"].as_str().unwrap(), "{name}");
    }
}

#[test]
fn prism_bil_zip_reads_as_both_vortex_versions_do() {
    let source = open_source(&fixtures().join(PRISM), &OpenOptions::default()).unwrap();
    assert_eq!(source.variables(), vec!["precipitation".to_string()]);
    let frame = source.frame("precipitation", 0).unwrap();
    let p = frame.parts();
    let e = expected();
    for version in ["1.1.0", "0.13.3"] {
        let v = &e["prism_bil_zip"][version];
        assert_eq!(p.nx as u64, v["nx"].as_u64().unwrap());
        assert_eq!(p.ny as u64, v["ny"].as_u64().unwrap());
        assert_eq!(p.transform.dx.to_bits(), double(&v["dx"]).to_bits());
        assert_eq!(p.transform.dy.to_bits(), double(&v["dy"]).to_bits());
        assert_eq!(
            p.transform.origin_x.to_bits(),
            double(&v["originX"]).to_bits()
        );
        assert_eq!(
            p.transform.origin_y.to_bits(),
            double(&v["originY"]).to_bits()
        );
        assert_eq!(p.units, v["units"].as_str().unwrap());
        assert_eq!(
            source.short_name("precipitation").unwrap(),
            v["shortName"].as_str().unwrap()
        );
        let (start, end) = times(&p.time);
        assert_eq!(start, v["startTime"].as_str().unwrap());
        assert_eq!(end, v["endTime"].as_str().unwrap());
        assert_eq!(java_type(p.data_type), v["dataType"].as_str().unwrap());
        assert_eq!(f64::from(p.nodata.unwrap()), double(&v["noDataValue"]));
        assert_eq!(
            sha256(&cells_le(&p.cells)),
            v["cells_sha256"].as_str().unwrap(),
            "{version}"
        );
    }
}

/// The fixture's two SNODAS members in a folder of their own, so a test can add files beside them.
fn snodas_folder(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("p71_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for ext in [".dat.gz", ".txt.gz"] {
        std::fs::copy(
            fixtures().join(format!("{STEM}{ext}")),
            dir.join(format!("{STEM}{ext}")),
        )
        .unwrap();
    }
    dir
}

#[test]
fn snodas_member_reads_as_vortex_does_but_for_the_stated_nodata() {
    let path = fixtures().join(format!("{STEM}.dat.gz"));
    // Vortex's timing is an explicit option.
    let source = open_source(&path, &vortex_times()).unwrap();
    let name = "Sublimation of Blowing Snow";
    assert_eq!(source.variables(), vec![name.to_string()]);
    let frame = source.frame(name, 0).unwrap();
    let p = frame.parts();
    assert_eq!(p.nodata, Some(-9999.0));
    // Vortex divides every cell by the scale, -9999 included, and sets no nodata.
    let vortex_marker = -9999.0f32 / 100_000.0f32;
    let as_vortex: Vec<f32> = p
        .cells
        .iter()
        .map(|c| if *c == -9999.0 { vortex_marker } else { *c })
        .collect();
    let markers = p.cells.iter().filter(|c| **c == -9999.0).count();
    assert!(markers > 0, "the masked grid has cells outside CONUS");
    let e = expected();
    for version in ["1.1.0", "0.13.3"] {
        let v = &e["snodas_member"][version];
        assert_eq!(
            (p.nx as u64, p.ny as u64),
            (v["nx"].as_u64().unwrap(), v["ny"].as_u64().unwrap())
        );
        assert_eq!(p.transform.dx.to_bits(), double(&v["dx"]).to_bits());
        assert_eq!(p.transform.dy.to_bits(), double(&v["dy"]).to_bits());
        assert_eq!(
            p.transform.origin_x.to_bits(),
            double(&v["originX"]).to_bits()
        );
        assert_eq!(
            p.transform.origin_y.to_bits(),
            double(&v["originY"]).to_bits()
        );
        assert_eq!(p.units, v["units"].as_str().unwrap());
        assert_eq!(v["noDataValue"].as_str().unwrap(), "nan");
        match &p.crs {
            dss_vortex::Crs::Wkt(text) => assert_eq!(text, v["wkt"].as_str().unwrap()),
            other => panic!("{other:?}"),
        }
        let (start, end) = times(&p.time);
        assert_eq!(start, v["startTime"].as_str().unwrap());
        assert_eq!(end, v["endTime"].as_str().unwrap());
        // 0.13.3 types every SNODAS grid INSTANTANEOUS; this reader follows 1.1.0.
        let want = if version == "0.13.3" {
            "AVERAGE"
        } else {
            v["dataType"].as_str().unwrap()
        };
        assert_eq!(java_type(p.data_type), want);
        assert_eq!(
            sha256(&cells_le(&as_vortex)),
            v["cells_sha256"].as_str().unwrap(),
            "{version}"
        );
    }
}

#[test]
fn snodas_header_options_take_the_header() {
    let dir = snodas_folder("header");
    let path = dir.join(format!("{STEM}.dat.gz"));
    let options = OpenOptions {
        snodas: SnodasOptions {
            times: SnodasTimes::Header,
            georeference: SnodasGeoreference::Header,
            ..SnodasOptions::default()
        },
        ..OpenOptions::default()
    };
    let source = open_source(&path, &options).unwrap();
    let p = source
        .frame("Sublimation of Blowing Snow", 0)
        .unwrap()
        .into_parts();
    // The header: 24 hours ending 2020-01-01 06:00, where Vortex stamps the next 24.
    let (start, end) = times(&p.time);
    assert_eq!(
        (start.as_str(), end.as_str()),
        ("2019-12-31T06:00Z[UTC]", "2020-01-01T06:00Z[UTC]")
    );
    assert_eq!(p.transform.origin_x, -124.733333333328);
    assert_eq!(p.transform.origin_y, 52.8749999999979);
    // Without a header, header options are refused at opening, the default timing among
    // them; Vortex's timing needs none.
    std::fs::remove_file(dir.join(format!("{STEM}.txt.gz"))).unwrap();
    assert!(open_source(&path, &options).is_err());
    let error = open_source(&path, &OpenOptions::default())
        .err()
        .expect("the default needs a header")
        .to_string();
    assert!(error.contains("no NOHRSC header"), "{error}");
    assert!(open_source(&path, &vortex_times()).is_ok());
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

#[test]
fn snodas_tar_gives_what_its_member_gives() {
    let mut tar = Vec::new();
    for ext in [".txt.gz", ".dat.gz"] {
        let name = format!("{STEM}{ext}");
        let bytes = std::fs::read(fixtures().join(&name)).unwrap();
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
    assert_eq!(
        file.variables(),
        vec!["Sublimation of Blowing Snow".to_string()]
    );
    let from_tar = file.frame("Sublimation of Blowing Snow").unwrap();
    let member = open_source(
        &fixtures().join(format!("{STEM}.dat.gz")),
        &OpenOptions::default(),
    )
    .unwrap()
    .frame("Sublimation of Blowing Snow", 0)
    .unwrap();
    assert_eq!(cells_le(from_tar.cells()), cells_le(member.cells()));
    // Through open_source, as a file.
    let dir = std::env::temp_dir().join(format!("p71_tar_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SNODAS_20200101.tar");
    std::fs::write(&path, &tar).unwrap();
    let source = open_source(&path, &OpenOptions::default()).unwrap();
    assert_eq!(
        source.variables(),
        vec!["Sublimation of Blowing Snow".to_string()]
    );
    let _ = std::fs::remove_dir_all(dir);
    // Every truncation is an error or a smaller archive, never a panic.
    // Headers and member boundaries are cut everywhere; the gzip member, which decodes 46 MB, at
    // three points near its end.
    for cut in (0..2048).chain([tar.len() - 1536, tar.len() - 1030, tar.len() - 1]) {
        if let Ok(f) = SnodasFile::from_tar(
            "t.tar",
            tar[..cut].to_vec(),
            SnodasOptions::default(),
            1 << 32,
        ) {
            if cut + 1100 > tar.len() {
                let _ = f.frame("Sublimation of Blowing Snow");
            }
        }
    }
    assert_eq!(snodas::NCOLS * snodas::NROWS, 6935 * 3351);
}

/// A ZIP of stored members.
fn zip(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in members {
        let mut crc = flate2::Crc::new();
        crc.update(bytes);
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(bytes);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.sum().to_le_bytes());
        central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    let n = members.len() as u16;
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

const PRJ: &[u8] = b"GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"Degree\",0.017453292519943295]]";
const ASC: &[u8] = b"ncols 3\nnrows 2\nxllcorner -100\nyllcorner 40\ncellsize 0.5\nNODATA_value -9999\n1.5 -9999 2\n3 4 5\n";

fn pam(nodata: &str) -> Vec<u8> {
    format!("<PAMDataset>\n  <PAMRasterBand band=\"1\">\n    <NoDataValue>{nodata}</NoDataValue>\n    <Metadata>\n      <MDI key=\"STATISTICS_MINIMUM\">0</MDI>\n    </Metadata>\n  </PAMRasterBand>\n</PAMDataset>\n").into_bytes()
}

#[test]
fn zipped_ascii_grid_is_read_with_its_prj_and_named_from_its_member() {
    let name = "PRISM_ppt_stable_4kmD2_20200101_asc";
    let data = zip(&[(&format!("{name}.asc"), ASC), (&format!("{name}.prj"), PRJ)]);
    let rasters = read_zip(&format!("{name}.zip"), &data, ReadOptions::default()).unwrap();
    assert_eq!(rasters.len(), 1);
    let p = rasters[0].frame.parts();
    assert_eq!(p.cells, vec![1.5, -9999.0, 2.0, 3.0, 4.0, 5.0]);
    assert_eq!(rasters[0].short_name, "precipitation");
    assert!(matches!(p.time, FrameTime::Period { .. }));
    assert!(matches!(p.crs, dss_vortex::Crs::Wkt(_)));
}

#[test]
fn zipped_geotiff_takes_pam_nodata_as_gdal_does() {
    let tif = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/raster/crs_4326.tif"),
    )
    .unwrap();
    let own = RasterFile::from_bytes("x.tif", &tif, None, ReadOptions::default())
        .unwrap()
        .frame()
        .parts()
        .nodata;
    let name = "prism_ppt_us_25m_202001_avg_30y";
    let tif_name = format!("{name}.tif");
    let aux_name = format!("{name}.tif.aux.xml");
    let same = pam(&own.map_or("-9999".to_string(), |v| format!("{v:e}")));
    let data = zip(&[(&tif_name, &tif), (&aux_name, &same)]);
    let rasters = read_zip(&format!("{name}.zip"), &data, ReadOptions::default()).unwrap();
    let p = rasters[0].frame.parts();
    assert_eq!(p.nodata, Some(own.unwrap_or(-9999.0)));
    assert_eq!(rasters[0].short_name, "precipitation");
    assert_eq!(
        p.time,
        FrameTime::Period {
            start: (43830, 0),
            end: (43861, 0)
        }
    );
    if own.is_some() {
        let other = pam("12345");
        let data = zip(&[(&tif_name, &tif), (&aux_name, &other)]);
        assert!(read_zip(&format!("{name}.zip"), &data, ReadOptions::default()).is_err());
    }
}

/// A loose GeoTIFF with a `.tif.aux.xml` is read under the rule a PRISM ZIP's member gets, and
/// gives the frame the same pair gives inside a ZIP. Run over several fixtures so that both a
/// raster with its own nodata and one without are covered.
#[test]
fn loose_geotiff_takes_pam_nodata_as_a_zipped_one_does() {
    let raster = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/raster");
    let dir = std::env::temp_dir().join(format!("p71_d73_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let name = "prism_ppt_us_25m_202001_avg_30y";
    let tif_name = format!("{name}.tif");
    let aux_name = format!("{name}.tif.aux.xml");
    let path = dir.join(&tif_name);
    let aux = dir.join(&aux_name);
    let (mut with_own, mut without_own) = (0, 0);
    for fixture in [
        "crs_4326.tif",
        "strip_none.tif",
        "tile_deflate.tif",
        "crs_utm.tif",
        "nodata_none.tif",
        "prism_ppt_us_30s_202001.tif",
    ] {
        let tif = std::fs::read(raster.join(fixture)).unwrap();
        std::fs::write(&path, &tif).unwrap();
        let _ = std::fs::remove_file(&aux);
        let plain = RasterFile::open(&path).unwrap();
        let own = plain.frame().parts().nodata;
        let stated = own.unwrap_or(-9999.0);
        std::fs::write(&aux, pam(&format!("{stated:e}"))).unwrap();
        let loose = RasterFile::open(&path).unwrap_or_else(|e| panic!("{fixture}: {e}"));
        let p = loose.frame().parts();
        assert_eq!(p.nodata, Some(stated), "{fixture}");
        assert_eq!(p.cells, plain.frame().parts().cells, "{fixture}");
        assert_eq!(p.transform, plain.frame().parts().transform, "{fixture}");
        assert_eq!(p.crs, plain.frame().parts().crs, "{fixture}");
        assert_eq!(loose.stored_wkt(), plain.stored_wkt(), "{fixture}");
        // The same pair inside a ZIP gives the same cells, nodata, georeference and time.
        let data = zip(&[(&tif_name, &tif), (&aux_name, &pam(&format!("{stated:e}")))]);
        let zipped = read_zip(&format!("{name}.zip"), &data, ReadOptions::default()).unwrap();
        let z = zipped[0].frame.parts();
        assert_eq!(
            (&z.cells, z.nodata, z.transform, &z.crs, &z.time),
            (&p.cells, p.nodata, p.transform, &p.crs, &p.time),
            "{fixture}"
        );
        // And the importer reads it.
        let source = open_source(&path, &OpenOptions::default()).unwrap();
        let variable = source.variables()[0].clone();
        assert_eq!(
            source.frame(&variable, 0).unwrap().parts().nodata,
            Some(stated)
        );
        // A PAM nodata that disagrees with the raster's own, or is no float32, is refused.
        let bad = if own.is_some() { "12345" } else { "1e40" };
        std::fs::write(&aux, pam(bad)).unwrap();
        let error = RasterFile::open(&path).unwrap_err().to_string();
        assert!(error.contains(&aux_name), "{fixture}: {error}");
        if own.is_some() {
            with_own += 1;
        } else {
            without_own += 1;
        }
    }
    assert!(with_own > 0 && without_own > 0, "{with_own} {without_own}");

    // The rule is the ZIP's: anything but nodata and statistics is refused, as it was up to 0.1.1.
    let tif = std::fs::read(raster.join("crs_4326.tif")).unwrap();
    std::fs::write(&path, &tif).unwrap();
    for refused in [
        &b"<PAMDataset><SRS>GEOGCS[]</SRS><PAMRasterBand band=\"1\"/></PAMDataset>"[..],
        b"<PAMDataset><GeoTransform>0,1,0,0,0,-1</GeoTransform><PAMRasterBand band=\"1\"/></PAMDataset>",
        b"<PAMDataset><PAMRasterBand band=\"1\"><Scale>2</Scale></PAMRasterBand></PAMDataset>",
        b"<PAMDataset/>",
        b"x",
    ] {
        std::fs::write(&aux, refused).unwrap();
        match RasterFile::open(&path) {
            Err(dss_vortex::VortexError::Refused { .. }) => {}
            other => panic!("{}: {other:?}", String::from_utf8_lossy(refused)),
        }
        assert!(open_source(&path, &OpenOptions::default()).is_err());
    }
    // GDAL matches the sidecar's name without regard to case, and so does this reader.
    std::fs::remove_file(&aux).unwrap();
    let upper = dir.join(format!("{name}.TIF.AUX.XML"));
    std::fs::write(&upper, pam("-9999")).unwrap();
    let read = RasterFile::open(&path).unwrap();
    let own = RasterFile::from_bytes(&tif_name, &tif, None, ReadOptions::default())
        .unwrap()
        .frame()
        .parts()
        .nodata;
    if own.is_none() || own == Some(-9999.0) {
        assert_eq!(read.frame().parts().nodata, Some(-9999.0));
    }
    std::fs::remove_file(&upper).unwrap();
    // The other sidecars GDAL applies stay refused beside a GeoTIFF.
    for side in [
        format!("{tif_name}.aux"),
        format!("{name}.aux"),
        format!("{tif_name}.msk"),
    ] {
        std::fs::write(dir.join(&side), b"x").unwrap();
        assert!(RasterFile::open(&path).is_err(), "{side}");
        std::fs::remove_file(dir.join(&side)).unwrap();
    }
    // The sidecar rule is for GeoTIFF: an ASCII grid's .aux.xml stays refused.
    let asc = dir.join("a.asc");
    std::fs::write(&asc, ASC).unwrap();
    assert!(RasterFile::open(&asc).is_ok());
    std::fs::write(dir.join("a.asc.aux.xml"), pam("-9999")).unwrap();
    assert!(RasterFile::open(&asc).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zips_that_would_be_read_differently_are_refused() {
    let both = zip(&[("a.asc", ASC), ("b.bil", b"x"), ("b.hdr", b"NROWS 1\n")]);
    assert!(read_zip("grids.zip", &both, ReadOptions::default()).is_err());
    // Named for Vortex's dispatch, the name chooses; a BIL with no header is refused.
    assert!(read_zip("grids_bil.zip", &both, ReadOptions::default()).is_err());
    assert_eq!(
        read_zip("grids_asc.zip", &both, ReadOptions::default())
            .unwrap()
            .len(),
        1
    );
    let srs = b"<PAMDataset><SRS>GEOGCS[]</SRS><PAMRasterBand band=\"1\"/></PAMDataset>";
    let data = zip(&[("a.asc", ASC), ("a.asc.aux.xml", srs)]);
    assert!(read_zip("a_asc.zip", &data, ReadOptions::default()).is_err());
    let msk = zip(&[("a.asc", ASC), ("a.asc.msk", b"x")]);
    assert!(read_zip("a_asc.zip", &msk, ReadOptions::default()).is_err());
    let none = zip(&[("readme.txt", b"x")]);
    assert!(read_zip("a.zip", &none, ReadOptions::default()).is_err());
    let nested = zip(&[("dir/a.asc", ASC)]);
    assert!(read_zip("a.zip", &nested, ReadOptions::default()).is_err());
}

#[test]
fn damaged_prism_zip_never_panics() {
    let data = std::fs::read(fixtures().join(PRISM)).unwrap();
    let n = data.len();
    for cut in (0..n).step_by(n / 97).chain(n - 64..n) {
        let _ = read_zip(PRISM, &data[..cut], ReadOptions::default());
    }
    // Deterministic byte flips across the central directory and the first headers.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..200 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let mut bad = data.clone();
        let at = if state.is_multiple_of(2) {
            (state as usize >> 8) % 4096
        } else {
            n - 1 - (state as usize >> 8) % 1024
        };
        bad[at] ^= (state >> 32) as u8 | 1;
        let _ = read_zip(PRISM, &bad, ReadOptions::default());
    }
}

#[test]
fn a_loose_bil_needs_its_header_and_reads_with_it() {
    let dir = std::env::temp_dir().join(format!("p71_bil_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let name = "PRISM_tmean_stable_4kmM3_201901_bil";
    let bil = dir.join(format!("{name}.bil"));
    std::fs::File::create(&bil)
        .unwrap()
        .write_all(&[0, 0, 0x80, 0x3f, 0, 0, 0, 0x40])
        .unwrap();
    assert!(open_source(&bil, &OpenOptions::default()).is_err());
    std::fs::write(
        dir.join(format!("{name}.HDR")),
        "BYTEORDER I\nNROWS 1\nNCOLS 2\nNBITS 32\nPIXELTYPE FLOAT\nULXMAP -100\nULYMAP 40\nXDIM 0.5\nYDIM 0.5\nNODATA -9999\n",
    )
    .unwrap();
    std::fs::write(dir.join(format!("{name}.prj")), PRJ).unwrap();
    let source = open_source(&bil, &OpenOptions::default()).unwrap();
    assert_eq!(source.variables(), vec!["mean temperature".to_string()]);
    let p = source.frame("mean temperature", 0).unwrap().into_parts();
    assert_eq!(p.cells, vec![1.0, 2.0]);
    assert_eq!(p.units, "Degrees C");
    assert_eq!(p.data_type, DataType::PerAver);
    std::fs::write(dir.join(format!("{name}.bil.msk")), b"x").unwrap();
    assert!(open_source(&bil, &OpenOptions::default()).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

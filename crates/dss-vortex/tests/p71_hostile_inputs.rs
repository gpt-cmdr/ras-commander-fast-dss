//! Hostile inputs on the public P71 fixtures:
//! - malformed SNODAS headers give typed errors and never panic;
//! - one ZIP archive's rasters have an aggregate budget, enforced before decoding.

use dss_vortex::import::{open_source, OpenOptions, StopFlag};
use dss_vortex::source::raster::{Limits, ReadOptions};
use dss_vortex::source::snodas::{self, SnodasFile, SnodasOptions, SnodasTimes};
use dss_vortex::source::zipped::{read_zip, read_zip_with, ArchiveBudget};
use dss_vortex::VortexError;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const STEM: &str = "us_ssmv11039lL00T0024TTNATS2020010105DP000";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/p71")
}

// ---- Finding 1 ----

fn header_text() -> String {
    let gz = std::fs::read(fixtures().join(format!("{STEM}.txt.gz"))).unwrap();
    let mut text = String::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// The header with `key`'s value replaced.
fn with_field(header: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let out: Vec<String> = header
        .lines()
        .map(|l| match l.split_once(':') {
            Some((k, _)) if k.trim() == key => {
                found = true;
                format!("{key}: {value}")
            }
            _ => l.to_string(),
        })
        .collect();
    assert!(found, "no {key} in the header");
    out.join("\n")
}

fn parse(text: &str) -> Result<snodas::Header, VortexError> {
    std::panic::catch_unwind(|| snodas::parse_header(text.as_bytes()))
        .unwrap_or_else(|_| panic!("parse_header panicked on {text:?}"))
}

#[test]
fn malformed_headers_are_typed_refusals() {
    let header = header_text();
    assert!(snodas::parse_header(header.as_bytes()).is_ok());
    let dat = std::fs::read(fixtures().join(format!("{STEM}.dat.gz"))).unwrap();
    for (key, value) in [
        ("Start year", "9223372036854775807"),
        ("Start hour", "-9223372036854775808"),
        ("Start hour", "-1"),
        ("Stop minute", "-1"),
    ] {
        let text = with_field(&header, key, value);
        match parse(&text) {
            Err(VortexError::Refused { .. }) => {}
            other => panic!("{key}={value}: {other:?}"),
        }
        // The header is parsed under either timing mode, so both refuse it at opening.
        for times in [SnodasTimes::Vortex, SnodasTimes::Header] {
            let options = SnodasOptions {
                times,
                ..SnodasOptions::default()
            };
            let opened = std::panic::catch_unwind(|| {
                SnodasFile::from_member(
                    format!("{STEM}.dat.gz"),
                    dat.clone(),
                    Some(text.as_bytes()),
                    options,
                    1 << 32,
                )
            })
            .unwrap_or_else(|_| panic!("from_member panicked: {key}={value}"));
            assert!(
                matches!(opened, Err(VortexError::Refused { .. })),
                "{key}={value} {times:?}"
            );
        }
    }
}

/// Every time field of both stamps, over boundary and extreme values and a deterministic stream
/// of arbitrary integers: a typed result, never a panic, and an accepted value is in its range.
#[test]
fn header_time_fields_never_panic() {
    let header = header_text();
    let mut values: Vec<i64> = vec![
        i64::MIN,
        i64::MIN + 1,
        -(1 << 40),
        -1_000_000,
        -3601,
        -60,
        -1,
        0,
        1,
        2,
        11,
        12,
        13,
        23,
        24,
        28,
        29,
        30,
        31,
        32,
        59,
        60,
        61,
        1899,
        1900,
        9999,
        10_000,
        i64::from(i32::MAX),
        1 << 40,
        i64::MAX - 1,
        i64::MAX,
    ];
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        values.push(state as i64);
        values.push((state % 200_000) as i64 - 100_000);
    }
    let ranges: [(&str, std::ops::RangeInclusive<i64>); 6] = [
        ("year", 1..=9999),
        ("month", 1..=12),
        ("day", 1..=31),
        ("hour", 0..=23),
        ("minute", 0..=59),
        ("second", 0..=0),
    ];
    for prefix in ["Start", "Stop"] {
        for (part, range) in &ranges {
            let key = format!("{prefix} {part}");
            for v in &values {
                match parse(&with_field(&header, &key, &v.to_string())) {
                    Ok(_) => assert!(range.contains(v), "{key}={v} accepted"),
                    Err(VortexError::Refused { .. }) => {}
                    Err(other) => panic!("{key}={v}: not a typed refusal: {other}"),
                }
            }
            for junk in ["", "x", "1e3", "1.5", "99999999999999999999", "+-1", " "] {
                assert!(
                    parse(&with_field(&header, &key, junk)).is_err(),
                    "{key}={junk:?}"
                );
            }
        }
    }
    // A day past the month's end is still refused, as before.
    let feb = with_field(&with_field(&header, "Start month", "2"), "Start day", "30");
    assert!(parse(&feb).is_err());
}

#[test]
fn header_corner_fields_never_panic() {
    let header = header_text();
    for key in [
        "Minimum x-axis coordinate",
        "Maximum x-axis coordinate",
        "Minimum y-axis coordinate",
        "Maximum y-axis coordinate",
    ] {
        for v in [
            "NaN", "inf", "-inf", "1e308", "-1e308", "181", "-181", "91", "-91", "0", "x", "",
        ] {
            let _ = parse(&with_field(&header, key, v));
        }
        for v in ["NaN", "inf", "1e308", "-1e308", "181", "-181", "x", ""] {
            assert!(parse(&with_field(&header, key, v)).is_err(), "{key}={v}");
        }
    }
    let swapped = with_field(
        &with_field(&header, "Minimum x-axis coordinate", "-66.9416666666640"),
        "Maximum x-axis coordinate",
        "-124.733333333328",
    );
    assert!(parse(&swapped).is_err());
}

// ---- Finding 2 ----

/// A ZIP of stored (method 0) or deflated (method 8) members.
fn zip(members: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let method: u8 = if deflate { 8 } else { 0 };
    for (name, bytes) in members {
        let mut crc = flate2::Crc::new();
        crc.update(bytes);
        let packed = if deflate {
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(bytes).unwrap();
            enc.finish().unwrap()
        } else {
            bytes.to_vec()
        };
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, method, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, method, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.sum().to_le_bytes());
        central.extend_from_slice(&(packed.len() as u32).to_le_bytes());
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

/// A 100 x 100 ASCII grid: 10,000 cells, 40,000 decoded bytes.
fn asc_100() -> Vec<u8> {
    let mut text =
        String::from("ncols 100\nnrows 100\nxllcorner -100\nyllcorner 40\ncellsize 0.01\n");
    for _ in 0..100 {
        text.push_str(&"1 ".repeat(100));
        text.push('\n');
    }
    text.into_bytes()
}

/// Eight deflated 100 x 100 grids, each valid on its own.
fn eight_grids() -> Vec<u8> {
    let grid = asc_100();
    let names: Vec<String> = (0..8).map(|i| format!("g{i}.asc")).collect();
    let members: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &grid[..])).collect();
    zip(&members, true)
}

fn per_grid(max_input_bytes: u64) -> ReadOptions {
    ReadOptions {
        limits: Limits {
            max_cells: 10_000,
            max_input_bytes,
        },
        ..ReadOptions::default()
    }
}

fn budget(max_decoded_bytes: u64, max_rasters: usize) -> ArchiveBudget {
    ArchiveBudget {
        max_decoded_bytes,
        max_rasters,
    }
}

fn refusal(result: Result<Vec<dss_vortex::source::zipped::ZippedRaster>, VortexError>) -> String {
    match result {
        Err(e @ VortexError::Refused { .. }) => e.to_string(),
        Err(other) => panic!("not a refusal: {other}"),
        Ok(r) => panic!("read {} rasters", r.len()),
    }
}

#[test]
fn zip_aggregate_budget_is_enforced_before_decoding() {
    let data = eight_grids();
    let options = per_grid(1 << 20);
    let read = |b: ArchiveBudget| read_zip_with("grids_asc.zip", &data, options, b, None);
    assert_eq!(read(budget(8 * 40_000, 8)).unwrap().len(), 8);
    // Room for four and a half grids: the fifth's decoder is given the 5,000 cells the budget
    // leaves, and refuses its 10,000 before allocating them.
    let error = refusal(read(budget(4 * 40_000 + 20_000, 8)));
    assert!(error.contains("decoded bytes leaves 5000 cells"), "{error}");
    // Exactly four grids: the fifth finds nothing left.
    let error = refusal(read(budget(4 * 40_000, 8)));
    assert!(error.contains("more than the limit"), "{error}");
    // Too many rasters is refused from the central directory, before any is read.
    let error = refusal(read(budget(1 << 30, 7)));
    assert!(error.contains("8 rasters"), "{error}");
}

#[test]
fn zip_members_that_inflate_past_the_input_limit_together_are_refused() {
    let data = eight_grids();
    // The archive is under the input limit; its members' stated sizes together are not.
    assert!((data.len() as u64) < 25_000);
    let error = refusal(read_zip_with(
        "grids_asc.zip",
        &data,
        per_grid(25_000),
        ArchiveBudget::default(),
        None,
    ));
    assert!(error.contains("together"), "{error}");
}

#[test]
fn zip_read_stops_when_asked() {
    let data = eight_grids();
    let stop: StopFlag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let error = refusal(read_zip_with(
        "grids_asc.zip",
        &data,
        per_grid(1 << 20),
        ArchiveBudget::default(),
        Some(&stop),
    ));
    assert!(error.contains("stopped"), "{error}");
}

#[test]
fn zip_with_many_rasters_is_refused_by_the_default_budget() {
    let grid = b"ncols 1\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1\n";
    let names: Vec<String> = (0..300).map(|i| format!("g{i:03}.asc")).collect();
    let members: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &grid[..])).collect();
    let data = zip(&members, false);
    let error = refusal(read_zip("many_asc.zip", &data, ReadOptions::default()));
    assert!(error.contains("300 rasters"), "{error}");
    // The importer carries the same budget in its limits.
    let dir = std::env::temp_dir().join(format!("p71_hostile_many_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("many_asc.zip");
    std::fs::write(&path, &data).unwrap();
    let error = match open_source(&path, &OpenOptions::default()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("opened"),
    };
    assert!(error.contains("300 rasters"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
    let defaults = dss_vortex::import::ImportLimits::default();
    assert_eq!(defaults.archive_budget(), ArchiveBudget::default());
}

// ---- Tar, BIL and renamed inputs ----

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

/// Finding 17: an oversized plain header in a SNODAS tar is refused for its size, read where it
/// lies in the archive and not copied first.
#[test]
fn snodas_tar_refuses_an_oversized_header() {
    let dat = std::fs::read(fixtures().join(format!("{STEM}.dat.gz"))).unwrap();
    let mut big = header_text().into_bytes();
    big.resize((1 << 16) + 1, b' ');
    let mut tar = Vec::new();
    for (name, bytes) in [
        (format!("{STEM}.txt"), &big),
        (format!("{STEM}.dat.gz"), &dat),
    ] {
        tar.extend(tar_header(&name, bytes.len()));
        tar.extend(bytes.iter());
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.extend(vec![0u8; 1024]);
    match SnodasFile::from_tar(
        "SNODAS_20200101.tar",
        tar,
        SnodasOptions::default(),
        1 << 32,
    ) {
        Err(VortexError::Refused { what, reason }) => {
            assert!(what.contains("header"), "{what}: {reason}");
            assert!(reason.contains("65537"), "{what}: {reason}");
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// Finding 23: a repeated `Compressed` key is refused, not read as absent.
#[test]
fn snodas_header_with_compressed_twice_is_refused() {
    // The NOHRSC header states `Compressed: no` once; without the line the key is absent.
    let once = header_text();
    assert_eq!(once.matches("Compressed:").count(), 1);
    assert!(parse(&once).is_ok());
    let absent: String = once
        .lines()
        .filter(|l| !l.starts_with("Compressed:"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(parse(&absent).is_ok());
    for second in ["Compressed: yes", "Compressed: no"] {
        match parse(&format!("{once}\n{second}\n")) {
            Err(VortexError::Refused { reason, .. }) => {
                assert!(reason.contains("Compressed is stated twice"), "{reason}")
            }
            other => panic!("{second}: {other:?}"),
        }
    }
}

const BIL_KEYS: &[&str] = &[
    "BYTEORDER I",
    "NROWS 1",
    "NCOLS 2",
    "NBITS 32",
    "PIXELTYPE FLOAT",
    "ULXMAP -100",
    "ULYMAP 40",
    "XDIM 0.5",
    "YDIM 0.5",
];
const BIL_DATA: &[u8] = &[0, 0, 0x80, 0x3f, 0, 0, 0, 0x40];

fn bil(hdr: &str, prj: Option<&[u8]>) -> Result<dss_vortex::source::bil::BilFile, VortexError> {
    use dss_vortex::source::bil::{BilFile, BilParts};
    BilFile::from_parts(
        "PRISM_tmean_stable_4kmM3_201901_bil.bil",
        BilParts {
            data: BIL_DATA,
            hdr: hdr.as_bytes(),
            prj,
            aux_xml: None,
        },
        ReadOptions::default(),
    )
}

/// Finding 19: GDAL counts every header line, blank ones included, and reads no key on line 51
/// or later; `\r\n` ends one line, not two.
#[test]
fn bil_header_lines_are_counted_as_gdal_counts_them() {
    let keys = BIL_KEYS.join("\n");
    assert!(bil(&keys, None).is_ok());
    // 41 blank lines put the last key on line 50: read.
    let blanks = |n: usize| "\n".repeat(n);
    let on_50 = format!("{}{}", blanks(41), keys);
    assert!(bil(&on_50, None).is_ok());
    // One more blank line puts it on line 51, which GDAL never reads: refused.
    let on_51 = format!("{}{}", blanks(42), keys);
    match bil(&on_51, None) {
        Err(VortexError::Refused { reason, .. }) => assert!(reason.contains("line 51"), "{reason}"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    // CRLF: 41 blank lines and the keys, each ended by \r\n, are still 50 lines.
    let crlf = format!("{}{}\r\n", "\r\n".repeat(41), BIL_KEYS.join("\r\n"));
    assert!(bil(&crlf, None).is_ok());
    // Blank lines after line 50 change nothing GDAL reads.
    let trailing = format!("{keys}{}", blanks(60));
    assert!(bil(&trailing, None).is_ok());
}

/// Finding 20: `from_parts` holds the sidecars to the 64 KiB cap the file and ZIP paths use.
#[test]
fn bil_from_parts_caps_its_sidecars() {
    let keys = BIL_KEYS.join("\n");
    let big_hdr = format!("{keys}\n{}", " ".repeat(1 << 16));
    match bil(&big_hdr, None) {
        Err(VortexError::Refused { what, reason }) => {
            assert!(
                what.contains(".hdr") && reason.contains("65536"),
                "{what}: {reason}"
            )
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    let big_prj = vec![b' '; (1 << 16) + 1];
    match bil(&keys, Some(&big_prj)) {
        Err(VortexError::Refused { what, .. }) => assert!(what.contains(".prj"), "{what}"),
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// Finding 10: a renamed file reaches its own format's reader. A GRIB2 file named as a SNODAS
/// member opens as GRIB2; an HDF5 file with a user block named `.bil`, with a `.hdr` beside it, is
/// not read as BIL.
#[test]
fn renamed_files_reach_their_own_reader() {
    let dir = std::env::temp_dir().join(format!("p71_hostile_dispatch_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let grib =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/grib/complex.grib2");
    let named = dir.join(format!("{STEM}.dat"));
    std::fs::copy(&grib, &named).unwrap();
    let source = open_source(&named, &OpenOptions::default()).unwrap();
    assert!(!source.variables().is_empty());
    // The same name over bytes that are not the masked grid and not GRIB: SNODAS's own refusal.
    std::fs::write(&named, b"not a grid").unwrap();
    match open_source(&named, &OpenOptions::default()) {
        Err(e) => assert!(
            e.to_string().contains("SNODAS") || e.to_string().contains("bytes"),
            "{e}"
        ),
        Ok(_) => panic!("opened"),
    }
    let hdf5 = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/netcdf/3B-HHR.MS.MRG.SYNTH.hdf5");
    let mut bytes = vec![0u8; 512];
    bytes.extend(std::fs::read(&hdf5).unwrap());
    let as_bil = dir.join("PRISM_ppt_stable_4kmD1_19990101_bil.bil");
    std::fs::write(&as_bil, &bytes).unwrap();
    std::fs::write(
        dir.join("PRISM_ppt_stable_4kmD1_19990101_bil.hdr"),
        BIL_KEYS.join("\n"),
    )
    .unwrap();
    // The file must open, through the NetCDF/HDF5 reader, and give
    // the fixture's own variable and cells; an error of any kind, or another reader, fails.
    // The fixture's IMERG-style name brings Vortex's file-name window rule, which the
    // renamed copy does not have; the window is not what this test is about, so both are read
    // under `WindowRule::Vortex` and compared in grid and cells, not in time.
    let options = OpenOptions {
        window_rule: dss_vortex::products::WindowRule::Vortex,
        ..OpenOptions::default()
    };
    let plain = open_source(&hdf5, &options).unwrap();
    let opened = open_source(&as_bil, &options)
        .unwrap_or_else(|e| panic!("the HDF5 file with a user block did not open: {e}"));
    assert_eq!(opened.variables(), vec!["precipitationCal".to_string()]);
    assert_eq!(opened.variables(), plain.variables());
    let (a, b) = (
        opened.frame("precipitationCal", 0).unwrap().into_parts(),
        plain.frame("precipitationCal", 0).unwrap().into_parts(),
    );
    assert_eq!((a.nx, a.ny), (b.nx, b.ny));
    assert!(a.cells.len() > 1, "{} cells", a.cells.len());
    assert_eq!(cells_bits(&a.cells), cells_bits(&b.cells));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 16 (invalid): GDAL decides "no georeference" from the values, not from which keys are
/// present (3.2.1 `frmts/raw/ehdrdataset.cpp:1380`). Measured with GDAL 3.12.2 through rasterio: a
/// header stating `ULXMAP 0.5`, `ULYMAP 0.5`, `XDIM 1`, `YDIM 1` gives the identity transform and
/// `NotGeoreferencedWarning`, as a header without those keys does. Both are refused here alike, and
/// changing any one of the four values gives a georeference.
#[test]
fn bil_default_georeference_values_are_no_georeference_as_in_gdal() {
    let base = [
        "BYTEORDER I",
        "NROWS 1",
        "NCOLS 2",
        "NBITS 32",
        "PIXELTYPE FLOAT",
    ]
    .join("\n");
    let stated = format!("{base}\nULXMAP 0.5\nULYMAP 0.5\nXDIM 1\nYDIM 1");
    for hdr in [base.clone(), stated] {
        match bil(&hdr, None) {
            Err(VortexError::Refused { reason, .. }) => {
                assert!(reason.contains("no georeference"), "{reason}")
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
    }
    for changed in [
        "ULXMAP 0.25\nULYMAP 0.5\nXDIM 1\nYDIM 1",
        "ULXMAP 0.5\nULYMAP 0.25\nXDIM 1\nYDIM 1",
        "ULXMAP 0.5\nULYMAP 0.5\nXDIM 2\nYDIM 1",
        "ULXMAP 0.5\nULYMAP 0.5\nXDIM 1\nYDIM 2",
    ] {
        assert!(
            bil(&format!("{base}\n{changed}"), None).is_ok(),
            "{changed}"
        );
    }
}

fn cells_bits(cells: &[f32]) -> Vec<u32> {
    cells.iter().map(|c| c.to_bits()).collect()
}

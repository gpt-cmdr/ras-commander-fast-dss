//! Grid writer harness for comparisons with HEC Monolith.
//!
//! ```text
//! grid_write ops <file.dss> <ops.tsv> <6|7> [--fault <step>] [--no-sync]
//!     Store every grid line of ops.tsv into an existing file of that version.
//!     Compression column: 26 ZLIB, 101001 PRECIP_2_BYTE, 1 none, 0 = what Monolith
//!     chooses for a GridInfo left at its default (DSS7: ZLIB; DSS6: PRECIP_2_BYTE for a
//!     one-hour PER-CUM HRAP or Albers grid, else ZLIB; GriddedData.storeGriddedData
//!     offsets 288-363). Scale and base "-" let the writer choose them. A fault applies
//!     to the first line only (steps: data, eof, allocation, chain, retire, publish,
//!     counters, link, activate, finish). Prints one line per record.
//! grid_write read <file.dss>
//!     DSS6: the structural check (check_file6). Both versions: read every live grid
//!     record with the engine; prints one line per record and "problem:" lines.
//! ```
//!
//! Point it only at new files or copies (never at golden files).

use dss_core::dss6::write::write_grid6;
use dss_core::dss7::write::{
    write_grid, AlbersParameters, FaultPoint, GridCompression, GridInput, GridProjection,
    SpecifiedReference, WriteOptions, WriteStep,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn step(s: &str) -> Option<WriteStep> {
    Some(match s {
        "data" => WriteStep::Data,
        "eof" => WriteStep::EndOfFile,
        "allocation" => WriteStep::Allocation,
        "chain" => WriteStep::Chain,
        "retire" => WriteStep::Retire,
        "publish" => WriteStep::Publish,
        "counters" => WriteStep::Counters,
        "link" => WriteStep::Link,
        "activate" => WriteStep::Activate,
        "finish" => WriteStep::Finish,
        _ => return None,
    })
}

fn f(s: &str) -> f32 {
    s.parse().unwrap_or_else(|_| panic!("float {s:?}"))
}

fn ops(file: &Path, tsv: &Path, version: u8, fault: Option<WriteStep>, sync: bool) -> bool {
    let dir: PathBuf = tsv.parent().unwrap_or(Path::new(".")).to_path_buf();
    let text = std::fs::read_to_string(tsv).expect("ops.tsv");
    let mut ok = true;
    for (i, line) in text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .enumerate()
    {
        let c: Vec<&str> = line.split('\t').collect();
        let (nx, ny): (usize, usize) = (c[2].parse().unwrap(), c[3].parse().unwrap());
        let raw = std::fs::read(dir.join(c[12])).expect("cells");
        let data: Vec<f32> = raw
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let (mut x0, mut y0) = (0.0f32, 0.0f32);
        let projection = match c[1] {
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
                let wkt = if s[1].is_empty() {
                    Vec::new()
                } else {
                    std::fs::read(dir.join(s[1])).expect("wkt")
                };
                x0 = f(s[2]);
                y0 = f(s[3]);
                GridProjection::Specified(SpecifiedReference {
                    srs_name: s[0].as_bytes().to_vec(),
                    srs_definition: wkt,
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
        let one_hour = match (
            dss_core::grid_write::parse_grid_time(parts[4].as_bytes()),
            dss_core::grid_write::parse_grid_time(parts[5].as_bytes()),
        ) {
            (Some(a), Some(b)) => (b.0 - a.0) * 86_400 + b.1 - a.1 == 3_600,
            _ => false,
        };
        let explicit = (c[10] != "-" && c[11] != "-").then(|| (f(c[10]), f(c[11])));
        let p2b = GridCompression::Precip2Byte {
            scale_factor: explicit.map(|e| e.0),
            base: explicit.map(|e| e.1),
        };
        let compression = match (c[9], version) {
            ("26", _) => GridCompression::Zlib,
            ("1", _) => GridCompression::Uncompressed,
            ("101001", _) => p2b,
            (_, 7) => GridCompression::Zlib,
            _ => {
                let v1 = !matches!(projection, GridProjection::Specified(_));
                if v1 && one_hour && data_type == 1 {
                    p2b
                } else {
                    GridCompression::Zlib
                }
            }
        };
        let input = GridInput {
            pathname: c[0].as_bytes(),
            projection,
            // Monolith always stores the time-stamped types (GridInfo._gridType 410/420/430).
            time_stamped: true,
            data: &data,
            nx,
            ny,
            lower_left_cell_x: c[4].parse().unwrap(),
            lower_left_cell_y: c[5].parse().unwrap(),
            cell_size: f(c[6]),
            x_coord_of_grid_cell_zero: x0,
            y_coord_of_grid_cell_zero: y0,
            units: c[7].as_bytes(),
            data_type,
            compression,
        };
        let opts = WriteOptions {
            sync,
            fault: if i == 0 {
                fault.map(|step| FaultPoint { block: 0, step })
            } else {
                None
            },
            ..WriteOptions::default()
        };
        let r = if version == 6 {
            write_grid6(file, &input, &opts)
        } else {
            write_grid(file, &input, &opts)
        };
        match r {
            Ok(rep) => {
                for b in rep.blocks {
                    println!(
                        "{} {}",
                        b.outcome.as_str(),
                        String::from_utf8_lossy(&b.pathname)
                    );
                }
            }
            Err(e) => {
                println!("error {}: {e}", c[0]);
                ok = false;
                if fault.is_some() {
                    return ok;
                }
            }
        }
    }
    ok
}

fn read(file: &Path) -> bool {
    let bytes = std::fs::read(file).expect("read");
    let mut ok = true;
    if &bytes[16..18] == b"7-" {
        let mut f = dss_core::dss7::Dss7File::open(file).expect("open");
        let cat = f.catalog().expect("catalog").catalog;
        for e in cat.entries.into_iter().filter(|e| e.is_live()) {
            if !(400..=431).contains(&e.record_type) {
                continue;
            }
            match f.read_grid(&e.pathname) {
                Ok(g) => println!(
                    "{} ok {} cells",
                    String::from_utf8_lossy(&e.pathname),
                    g.data.len()
                ),
                Err(err) => {
                    ok = false;
                    println!("{} ERROR {err}", String::from_utf8_lossy(&e.pathname))
                }
            }
        }
    } else {
        // check_file6: the DSS6 structural check of P22 (bins, counters, links).
        let r = dss_core::dss6::write::check_file6(file).expect("check_file6");
        for p in &r.problems {
            ok = false;
            println!("problem: {p}");
        }
        let f = dss_core::dss6::Dss6File::open(file).expect("dss6");
        let cat = f.catalog().expect("catalog");
        for e in cat.entries.into_iter().filter(|e| e.is_live()) {
            if !(400..=431).contains(&e.record_type) {
                continue;
            }
            match f.read_grid(&e.pathname) {
                Ok(g) => println!(
                    "{} ok {} cells",
                    String::from_utf8_lossy(&e.pathname),
                    g.data.len()
                ),
                Err(err) => {
                    ok = false;
                    println!("{} ERROR {err}", String::from_utf8_lossy(&e.pathname))
                }
            }
        }
    }
    ok
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    let ok = match a.get(1).map(String::as_str) {
        Some("ops") if a.len() >= 5 => {
            let mut fault = None;
            let mut sync = true;
            let mut i = 5;
            while i < a.len() {
                match a[i].as_str() {
                    "--fault" => {
                        fault = step(&a[i + 1]);
                        i += 1;
                    }
                    "--no-sync" => sync = false,
                    _ => {}
                }
                i += 1;
            }
            ops(
                Path::new(&a[2]),
                Path::new(&a[3]),
                a[4].parse().unwrap(),
                fault,
                sync,
            )
        }
        Some("read") if a.len() == 3 => read(Path::new(&a[2])),
        _ => {
            eprintln!("usage: grid_write ops <file> <ops.tsv> <6|7> [--fault step] | read <file>");
            false
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

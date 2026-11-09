//! Paired-data writer harness for comparisons with HEC software.
//!
//! ```text
//! pd_write create <new.dss> <6|7>
//! pd_write store <file.dss> <spec.tsv> <6|7> [--fault <step>] [--no-sync]
//!     One paired-data store per spec line, tab-separated:
//!       kind path ui ti ud td labels xp yp axis tz ncurve ords vals
//!     kind float|double; "~" = empty string (no time zone for tz); labels "~" or
//!     '|'-joined; ords/vals csv of hex bits (8 digits float, 16 double). A fault applies to
//!     the first line only. Steps: data, eof, allocation, chain, retire, publish, counters,
//!     link, activate, finish. Prints the outcome per line.
//! pd_write check6 <file.dss>
//!     check_file6 problems, then a read of every live paired-data record.
//! ```
//!
//! Point it only at new files or copies (never at golden files).

use std::path::Path;

use dss_core::dss6::write::{check_file6, create_dss6, write_pd6};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    create_dss7, write_pd, FaultPoint, PdArrays, PdInput, WriteOptions, WriteStep,
};

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

fn hex(s: &str) -> Vec<u64> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|t| u64::from_str_radix(t, 16).expect("hex bits"))
        .collect()
}

fn store(file: &str, spec: &str, version: u8, fault: Option<WriteStep>, sync: bool) -> i32 {
    let text = std::fs::read_to_string(spec).expect("spec");
    let mut rc = 0;
    let mut first = true;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let c: Vec<&str> = line.split('\t').collect();
        assert_eq!(c.len(), 14, "spec line has {} fields", c.len());
        let s = |i: usize| if c[i] == "~" { "" } else { c[i] };
        let (o, v) = (hex(c[12]), hex(c[13]));
        let (fo, fv): (Vec<f32>, Vec<f32>) = (
            o.iter().map(|&b| f32::from_bits(b as u32)).collect(),
            v.iter().map(|&b| f32::from_bits(b as u32)).collect(),
        );
        let (dor, dv): (Vec<f64>, Vec<f64>) = (
            o.iter().map(|&b| f64::from_bits(b)).collect(),
            v.iter().map(|&b| f64::from_bits(b)).collect(),
        );
        let data = if c[0] == "double" {
            PdArrays::F64 {
                ordinates: &dor,
                values: &dv,
            }
        } else {
            PdArrays::F32 {
                ordinates: &fo,
                values: &fv,
            }
        };
        let labels: Option<Vec<Vec<u8>>> =
            (c[6] != "~").then(|| c[6].split('|').map(|x| x.as_bytes().to_vec()).collect());
        let ncurve: usize = c[11].parse().expect("ncurve");
        let mut input = PdInput::new(
            c[1].as_bytes(),
            data,
            ncurve,
            s(2).as_bytes(),
            s(3).as_bytes(),
            s(4).as_bytes(),
            s(5).as_bytes(),
        );
        input.labels = labels.as_deref();
        input.x_precision = c[7].parse().expect("xp");
        input.y_precision = c[8].parse().expect("yp");
        input.independent_is_x_axis = c[9].parse().expect("axis");
        input.time_zone = (c[10] != "~").then(|| c[10].as_bytes());
        let opts = WriteOptions {
            sync,
            fault: if first {
                fault.map(|step| FaultPoint { block: 0, step })
            } else {
                None
            },
            ..WriteOptions::default()
        };
        first = false;
        let r = if version == 6 {
            write_pd6(Path::new(file), &input, &opts)
        } else {
            write_pd(Path::new(file), &input, &opts)
        };
        match r {
            Ok(r) => println!("{} {}", r.blocks[0].outcome.as_str(), c[1]),
            Err(e) => {
                println!("error {}: {e}", c[1]);
                rc = 6;
            }
        }
    }
    rc
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let code = match a.get(1).map(String::as_str) {
        Some("create") => {
            let r = if a[3] == "6" {
                create_dss6(Path::new(&a[2]))
            } else {
                create_dss7(Path::new(&a[2]))
            };
            r.map(|_| 0).unwrap_or_else(|e| {
                eprintln!("{e}");
                1
            })
        }
        Some("store") => {
            let mut fault = None;
            let mut sync = true;
            let mut i = 5;
            while i < a.len() {
                match a[i].as_str() {
                    "--fault" => {
                        fault = Some(step(&a[i + 1]).expect("step name"));
                        i += 1;
                    }
                    "--no-sync" => sync = false,
                    other => panic!("unknown option {other}"),
                }
                i += 1;
            }
            store(&a[2], &a[3], a[4].parse().expect("version"), fault, sync)
        }
        Some("check6") => {
            let c = check_file6(Path::new(&a[2])).expect("check");
            println!("live={} problems={:?}", c.live_entries, c.problems);
            let f = Dss6File::open(&a[2]).expect("open");
            let mut n = 0;
            for e in f.catalog().expect("catalog").entries {
                if e.record_type == 200 || e.record_type == 205 {
                    f.read_pd(&e.pathname).expect("read");
                    n += 1;
                }
            }
            println!("pd_read={n}");
            i32::from(!c.is_clean())
        }
        _ => {
            eprintln!("usage: pd_write create|store|check6 ...");
            2
        }
    };
    std::process::exit(code);
}

//! DSS6 writer harness for oracle comparisons (packet P22).
//!
//! ```text
//! dss6_write create <new.dss>
//!     Create an empty DSS6 file (refuses an existing path).
//! dss6_write ops <file.dss> <ops.tsv> [--fault <block>:<step>] [--no-sync] [--merge]
//!     Run the store lines of ops.tsv, the format of the Monolith helper
//!     (evidence/P22/scripts/P22Monolith.java), tab-separated:
//!       rts  path date(ddMMMyyyy) time(hhmm) units type values quality
//!       rtd  path date time units type values quality
//!       its  path jbase times(min) units type values quality inflag
//!       itd  path jbase times units type values quality inflag
//!     values: csv decimals or 0x<bits>; quality: csv or '-'. A fault applies to the
//!     first line only. Steps: data, eof, allocation, chain, counters, link, publish,
//!     retire, finish. Prints one line per block: outcome and pathname.
//! dss6_write check <file.dss>
//!     Structural check (check_file6) and a read of every live record.
//! ```
//!
//! Point it only at new files or copies (never at golden files).

use dss_core::dss6::interval::parse_d_part;
use dss_core::dss6::write::{check_file6, create_dss6, write_its6, write_rts6, ItsInput, ItsMode};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{FaultPoint, RtsInput, RtsValues, WriteOptions, WriteStep};
use std::process::ExitCode;

fn step(s: &str) -> Option<WriteStep> {
    Some(match s {
        "data" => WriteStep::Data,
        "eof" => WriteStep::EndOfFile,
        "allocation" => WriteStep::Allocation,
        "chain" => WriteStep::Chain,
        "counters" => WriteStep::Counters,
        "link" => WriteStep::Link,
        "publish" => WriteStep::Publish,
        "retire" => WriteStep::Retire,
        "finish" => WriteStep::Finish,
        _ => return None,
    })
}

fn floats(s: &str) -> Result<Vec<f32>, String> {
    s.split(',')
        .map(|t| {
            let t = t.trim();
            if let Some(h) = t.strip_prefix("0x") {
                u32::from_str_radix(h, 16)
                    .map(f32::from_bits)
                    .map_err(|e| e.to_string())
            } else {
                t.parse::<f32>().map_err(|e| e.to_string())
            }
        })
        .collect()
}

fn doubles(s: &str) -> Result<Vec<f64>, String> {
    s.split(',')
        .map(|t| {
            let t = t.trim();
            if let Some(h) = t.strip_prefix("0x") {
                u64::from_str_radix(h, 16)
                    .map(f64::from_bits)
                    .map_err(|e| e.to_string())
            } else {
                t.parse::<f64>().map_err(|e| e.to_string())
            }
        })
        .collect()
}

fn ints(s: &str) -> Result<Option<Vec<i32>>, String> {
    if s == "-" {
        return Ok(None);
    }
    s.split(',')
        .map(|t| {
            t.trim()
                .parse::<i64>()
                .map(|v| v as i32)
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn run() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("create") if a.len() == 3 => {
            create_dss6(a[2].as_ref()).map_err(|e| e.to_string())?;
            println!("created");
            Ok(())
        }
        Some("ops") if a.len() >= 4 => {
            let mut opts = WriteOptions::default();
            let mut i = 4;
            while i < a.len() {
                match a[i].as_str() {
                    "--fault" => {
                        let v = a.get(i + 1).ok_or("--fault needs <block>:<step>")?;
                        let (b, s) = v.split_once(':').ok_or("--fault needs <block>:<step>")?;
                        opts.fault = Some(FaultPoint {
                            block: b.parse().map_err(|_| "bad block")?,
                            step: step(s).ok_or("bad step")?,
                        });
                        i += 2;
                    }
                    "--no-sync" => {
                        opts.sync = false;
                        i += 1;
                    }
                    o => return Err(format!("unknown option {o}")),
                }
            }
            let spec = std::fs::read_to_string(&a[3]).map_err(|e| e.to_string())?;
            let path: &std::path::Path = a[2].as_ref();
            for (ln, line) in spec.lines().enumerate() {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let c: Vec<&str> = line.split('\t').collect();
                let res = match c[0] {
                    "rts" | "rtd" => {
                        let j = parse_d_part(c[2].as_bytes()).ok_or("bad date")?;
                        let t: i64 = c[3].parse().map_err(|_| "bad time")?;
                        let start = j * 86_400 + (t / 100) * 3_600 + (t % 100) * 60;
                        let q = ints(c[7])?;
                        let fv;
                        let dv;
                        let values = if c[0] == "rts" {
                            fv = floats(c[6])?;
                            RtsValues::F32(&fv)
                        } else {
                            dv = doubles(c[6])?;
                            RtsValues::F64(&dv)
                        };
                        let mut input = RtsInput::new(
                            c[1].as_bytes(),
                            start,
                            values,
                            c[4].as_bytes(),
                            c[5].as_bytes(),
                        );
                        input.quality = q.as_deref();
                        write_rts6(path, &input, &opts)
                    }
                    "its" | "itd" => {
                        let jb: i64 = c[2].parse().map_err(|_| "bad jbase")?;
                        let times: Vec<i64> = ints(c[3])?
                            .ok_or("no times")?
                            .iter()
                            .map(|&m| (jb * 1440 + i64::from(m)) * 60)
                            .collect();
                        let q = ints(c[7])?;
                        let fv;
                        let dv;
                        let values = if c[0] == "its" {
                            fv = floats(c[6])?;
                            RtsValues::F32(&fv)
                        } else {
                            dv = doubles(c[6])?;
                            RtsValues::F64(&dv)
                        };
                        let input = ItsInput {
                            pathname: c[1].as_bytes(),
                            times: &times,
                            values,
                            quality: q.as_deref(),
                            units: c[4].as_bytes(),
                            data_type: c[5].as_bytes(),
                            mode: if c.get(8) == Some(&"0") {
                                ItsMode::Merge
                            } else {
                                ItsMode::Replace
                            },
                        };
                        write_its6(path, &input, &opts)
                    }
                    o => return Err(format!("line {}: unknown op {o}", ln + 1)),
                };
                // The fault applies to the first store only.
                opts.fault = None;
                match res {
                    Ok(r) => {
                        if r.reconciled_interrupted_write {
                            println!("reconciled");
                        }
                        for b in r.blocks {
                            println!(
                                "{} {}",
                                b.outcome.as_str(),
                                String::from_utf8_lossy(&b.pathname)
                            );
                        }
                    }
                    Err(e) => println!("error line {}: {e}", ln + 1),
                }
            }
            Ok(())
        }
        Some("check") if a.len() == 3 => {
            let r = check_file6(a[2].as_ref()).map_err(|e| e.to_string())?;
            for n in &r.notes {
                println!("note: {n}");
            }
            for p in &r.problems {
                println!("problem: {p}");
            }
            let f = Dss6File::open(&a[2]).map_err(|e| e.to_string())?;
            let cat = f.catalog().map_err(|e| e.to_string())?;
            let mut bad = 0;
            for e in &cat.entries {
                if f.read_ts_record(&e.pathname).is_err() {
                    bad += 1;
                    println!("unreadable: {}", String::from_utf8_lossy(&e.pathname));
                }
            }
            println!(
                "live {} catalog {} anomalies {} unreadable {bad} {}",
                r.live_entries,
                cat.entries.len(),
                cat.anomalies.len(),
                if r.is_clean() && bad == 0 {
                    "CLEAN"
                } else {
                    "PROBLEMS"
                }
            );
            Ok(())
        }
        _ => Err("usage: dss6_write create|ops|check ...".into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

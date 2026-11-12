//! DSS7 writer harness for comparisons with HEC's C library and HEC Monolith.
//!
//! ```text
//! dss7_write create <new.dss>
//!     Create an empty DSS7 file (refuses an existing path).
//! dss7_write store <file.dss> <spec.txt> [--fault <block>:<step>] [--no-sync]
//!     Store every STORE line of the spec:
//!       STORE <float|double> <pathname> <ddMmmyyyy> <hhmm|hhmmss> <units|~> <type|~> <n> 0 <q:0|1>
//!       <n hex value bit patterns>
//!       [<n decimal quality ints> when q = 1]
//!     Irregular series:
//!       IRREG <float|double> <pathname> <units|~> <type|~> <n> <granularity|0> <flag> <q:0|1>
//!       <n decimal times, whole seconds since 1899-12-31 00:00>
//!       <n hex value bit patterns>
//!       [<n decimal quality ints> when q = 1]
//!     (granularity 0 = automatic; the flag is ignored: the writer implements flag 1).
//!     Steps: data, eof, allocation, chain, retire, publish, counters, link, activate,
//!     finish.
//!     Prints one line per block: outcome and pathname.
//! dss7_write check <file.dss>
//!     Catalog anomalies, record count and a read of every live record.
//! ```
//!
//! Point it only at new files or copies (never at golden files).

use dss_core::dss7::interval::parse_date;
use dss_core::dss7::write::{
    create_dss7, write_its, write_rts, FaultPoint, ItsInput, RtsInput, RtsValues, WriteError,
    WriteOptions, WriteStep,
};
use std::process::ExitCode;

fn step(s: &str) -> Option<WriteStep> {
    Some(match s {
        "data" => WriteStep::Data,
        "eof" => WriteStep::EndOfFile,
        "allocation" => WriteStep::Allocation,
        "link" => WriteStep::Link,
        "retire" => WriteStep::Retire,
        "publish" => WriteStep::Publish,
        "activate" => WriteStep::Activate,
        "counters" => WriteStep::Counters,
        "chain" => WriteStep::Chain,
        "finish" => WriteStep::Finish,
        _ => return None,
    })
}

fn hec_seconds(date: &str, time: &str) -> Option<i64> {
    let j = parse_date(date.as_bytes())?;
    let t: i64 = time.parse().ok()?;
    let (h, m, s) = if time.len() > 4 {
        (t / 10_000, (t / 100) % 100, t % 100)
    } else {
        (t / 100, t % 100, 0)
    };
    Some(j * 86_400 + h * 3_600 + m * 60 + s)
}

fn run() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("create") if a.len() == 3 => {
            create_dss7(a[2].as_ref()).map_err(|e| e.to_string())?;
            println!("created");
            Ok(())
        }
        Some("store") if a.len() >= 4 => {
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
                        opts.sync = dss_core::dss7::write::SyncMode::None;
                        i += 1;
                    }
                    o => return Err(format!("unknown option {o}")),
                }
            }
            let spec = std::fs::read_to_string(&a[3]).map_err(|e| e.to_string())?;
            let mut t = spec.split_whitespace();
            while let Some(kw) = t.next() {
                if kw == "IRREG" {
                    let mut next = || t.next().ok_or("truncated spec");
                    let kind = next()?.to_string();
                    let path = next()?.to_string();
                    let units = next()?.replace('~', "");
                    let dtype = next()?.replace('~', "");
                    let n: usize = next()?.parse().map_err(|_| "bad n")?;
                    let gran: i64 = next()?.parse().map_err(|_| "bad granularity")?;
                    let _flag = next()?;
                    let has_q = next()? == "1";
                    let mut times = Vec::with_capacity(n);
                    for _ in 0..n {
                        times.push(next()?.parse::<i64>().map_err(|_| "bad time")?);
                    }
                    let mut f32s = Vec::new();
                    let mut f64s = Vec::new();
                    for _ in 0..n {
                        let h = next()?;
                        if kind == "float" {
                            f32s.push(f32::from_bits(
                                u32::from_str_radix(h, 16).map_err(|_| "bad hex")?,
                            ));
                        } else {
                            f64s.push(f64::from_bits(
                                u64::from_str_radix(h, 16).map_err(|_| "bad hex")?,
                            ));
                        }
                    }
                    let mut q = Vec::new();
                    if has_q {
                        for _ in 0..n {
                            q.push(next()?.parse::<i32>().map_err(|_| "bad quality")?);
                        }
                    }
                    let values = if kind == "float" {
                        RtsValues::F32(&f32s)
                    } else {
                        RtsValues::F64(&f64s)
                    };
                    let mut input = ItsInput::new(
                        path.as_bytes(),
                        &times,
                        values,
                        units.as_bytes(),
                        dtype.as_bytes(),
                    );
                    if has_q {
                        input.quality = Some(&q);
                    }
                    if gran > 0 {
                        input.time_granularity = Some(gran);
                    }
                    match write_its(a[2].as_ref(), &input, &opts) {
                        Ok(r) => {
                            for b in r.blocks {
                                println!(
                                    "{} {}",
                                    b.outcome.as_str(),
                                    String::from_utf8_lossy(&b.pathname)
                                );
                            }
                        }
                        Err(WriteError::InjectedFault { block, step }) => {
                            println!("fault {block} {step:?}");
                            return Ok(());
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                    continue;
                }
                if kw != "STORE" {
                    return Err(format!("bad spec token {kw}"));
                }
                let mut next = || t.next().ok_or("truncated spec");
                let kind = next()?.to_string();
                let path = next()?.to_string();
                let date = next()?.to_string();
                let time = next()?.to_string();
                let units = next()?.replace('~', "");
                let dtype = next()?.replace('~', "");
                let n: usize = next()?.parse().map_err(|_| "bad n")?;
                let _flag = next()?;
                let has_q = next()? == "1";
                let mut f32s = Vec::new();
                let mut f64s = Vec::new();
                for _ in 0..n {
                    let h = next()?;
                    if kind == "float" {
                        f32s.push(f32::from_bits(u32::from_str_radix(h, 16).map_err(|_| "bad hex")?));
                    } else {
                        f64s.push(f64::from_bits(u64::from_str_radix(h, 16).map_err(|_| "bad hex")?));
                    }
                }
                let mut q = Vec::new();
                if has_q {
                    for _ in 0..n {
                        q.push(next()?.parse::<i32>().map_err(|_| "bad quality")?);
                    }
                }
                let start = hec_seconds(&date, &time).ok_or("bad date/time")?;
                let values = if kind == "float" {
                    RtsValues::F32(&f32s)
                } else {
                    RtsValues::F64(&f64s)
                };
                let mut input = RtsInput::new(path.as_bytes(), start, values, units.as_bytes(), dtype.as_bytes());
                if has_q {
                    input.quality = Some(&q);
                }
                match write_rts(a[2].as_ref(), &input, &opts) {
                    Ok(r) => {
                        for b in r.blocks {
                            println!("{} {}", b.outcome.as_str(), String::from_utf8_lossy(&b.pathname));
                        }
                    }
                    Err(WriteError::InjectedFault { block, step }) => {
                        println!("fault {block} {step:?}");
                        return Ok(());
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok(())
        }
        Some("check") if a.len() == 3 => {
            // Engine-side check: catalog anomalies (shared bins, orphan or unlinked bins,
            // count mismatch), knumberRecords vs live entries, every live record read.
            let mut f = dss_core::dss7::Dss7File::open(&a[2]).map_err(|e| e.to_string())?;
            let cat = f.catalog().map_err(|e| e.to_string())?;
            let live: Vec<Vec<u8>> = cat.catalog.live().map(|e| e.pathname.clone()).collect();
            let mut fail = 0;
            for p in &live {
                match f.lookup(p) {
                    Ok(Some(r)) => {
                        if f.read_ts_record(&r).is_err() {
                            fail += 1;
                        }
                    }
                    _ => fail += 1,
                }
            }
            println!(
                "anomalies {} live {} header {} readfail {}",
                cat.catalog.anomalies.len(),
                live.len(),
                f.header().number_records,
                fail
            );
            for an in &cat.catalog.anomalies {
                println!("  {:?}: {}", an.kind, an.detail);
            }
            Ok(())
        }
        _ => Err("usage: dss7_write create <new.dss> | store <file.dss> <spec> | check <file.dss> [--fault b:step] [--no-sync]".into()),
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

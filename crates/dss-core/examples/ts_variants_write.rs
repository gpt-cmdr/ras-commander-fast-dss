//! Time-series variant writer harness for comparisons with HEC software.
//!
//! ```text
//! ts_variants_write store <file.dss> <spec.txt> [--fault <block>:<step>] [--no-sync] [--six]
//! ```
//!
//! The spec holds `STORE`, `IRREG` and `PATTERN` lines (their fields are parsed below);
//! `--six` writes a DSS6 file (irregular entries and patterns only need the
//! fields DSS6 has). Prints one line per block. Point it only at new files or copies, never
//! at reference files.

use dss_core::dss6::write as w6;
use dss_core::dss7::interval::parse_date;
use dss_core::dss7::write::{
    write_its_variants, write_pattern, write_rts_variants, FaultPoint, ItsInput, PatternInput,
    RtsInput, RtsValues, TsNotes, TsVariants, WriteError, WriteOptions, WriteReport, WriteStep,
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

struct Tok<'a>(std::str::SplitWhitespace<'a>);

impl<'a> Tok<'a> {
    fn s(&mut self) -> Result<&'a str, String> {
        self.0.next().ok_or_else(|| "truncated spec".to_string())
    }
    fn n<T: std::str::FromStr>(&mut self) -> Result<T, String> {
        self.s()?.parse().map_err(|_| "bad number".to_string())
    }
    fn ints(&mut self, count: usize) -> Result<Vec<i32>, String> {
        (0..count).map(|_| self.n()).collect()
    }
    fn times(&mut self, count: usize) -> Result<Vec<i64>, String> {
        (0..count).map(|_| self.n()).collect()
    }
    fn notes(&mut self, count: usize) -> Result<Vec<Vec<u8>>, String> {
        (0..count)
            .map(|_| {
                let t = self.s()?;
                if t == "-" {
                    return Ok(Vec::new());
                }
                (0..t.len() / 2)
                    .map(|i| {
                        u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|_| "bad hex".into())
                    })
                    .collect()
            })
            .collect()
    }
    fn values(&mut self, kind: &str, n: usize) -> Result<(Vec<f32>, Vec<f64>), String> {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for _ in 0..n {
            let h = self.s()?;
            if kind == "float" {
                a.push(f32::from_bits(
                    u32::from_str_radix(h, 16).map_err(|_| "bad hex")?,
                ));
            } else {
                b.push(f64::from_bits(
                    u64::from_str_radix(h, 16).map_err(|_| "bad hex")?,
                ));
            }
        }
        Ok((a, b))
    }
}

fn vals<'a>(kind: &str, v: &'a (Vec<f32>, Vec<f64>)) -> RtsValues<'a> {
    if kind == "float" {
        RtsValues::F32(&v.0)
    } else {
        RtsValues::F64(&v.1)
    }
}

fn print(r: Result<WriteReport, WriteError>) -> Result<(), String> {
    let r = r.map_err(|e| e.to_string())?;
    if r.reconciled_interrupted_write {
        println!("reconciled");
    }
    for b in &r.blocks {
        println!(
            "{} {}",
            b.outcome.as_str(),
            String::from_utf8_lossy(&b.pathname)
        );
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 || a[1] != "store" {
        return Err("usage: ts_variants_write store <file> <spec> [options]".into());
    }
    let mut opts = WriteOptions::default();
    let mut six = false;
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
            "--six" => {
                six = true;
                i += 1;
            }
            o => return Err(format!("unknown option {o}")),
        }
    }
    let file: &std::path::Path = a[2].as_ref();
    let spec = std::fs::read_to_string(&a[3]).map_err(|e| e.to_string())?;
    let mut t = Tok(spec.split_whitespace());
    while let Some(kw) = t.0.next() {
        match kw {
            "STORE" => {
                let kind = t.s()?;
                let path = t.s()?;
                let (date, time) = (t.s()?, t.s()?);
                let units = t.s()?.replace('~', "");
                let dtype = t.s()?.replace('~', "");
                let n: usize = t.n()?;
                let (qes, nes, cn): (usize, usize, usize) = (t.n()?, t.n()?, t.n()?);
                let v = t.values(kind, n)?;
                let q = t.ints(n * qes)?;
                let inotes = t.ints(n * nes)?;
                let notes = if cn == 1 { t.notes(n)? } else { Vec::new() };
                let refs: Vec<&[u8]> = notes.iter().map(Vec::as_slice).collect();
                let start = hec_seconds(date, time).ok_or("bad date")?;
                let input = RtsInput::new(
                    path.as_bytes(),
                    start,
                    vals(kind, &v),
                    units.as_bytes(),
                    dtype.as_bytes(),
                );
                let var = TsVariants {
                    quality_words: (qes > 0).then_some((&q[..], qes)),
                    notes: if cn == 1 {
                        Some(TsNotes::Char(&refs))
                    } else if nes > 0 {
                        Some(TsNotes::Int {
                            words: &inotes,
                            element_size: nes,
                        })
                    } else {
                        None
                    },
                };
                print(write_rts_variants(file, &input, &var, &opts))?;
            }
            "IRREG" => {
                let kind = t.s()?;
                let path = t.s()?;
                let units = t.s()?.replace('~', "");
                let dtype = t.s()?.replace('~', "");
                let n: usize = t.n()?;
                let gran: i64 = t.n()?;
                let (qes, nes, cn): (usize, usize, usize) = (t.n()?, t.n()?, t.n()?);
                let times = t.times(n)?;
                let v = t.values(kind, n)?;
                let q = t.ints(n * qes)?;
                let inotes = t.ints(n * nes)?;
                let notes = if cn == 1 { t.notes(n)? } else { Vec::new() };
                let refs: Vec<&[u8]> = notes.iter().map(Vec::as_slice).collect();
                if six {
                    let input = w6::ItsInput {
                        pathname: path.as_bytes(),
                        times: &times,
                        values: vals(kind, &v),
                        quality: (qes == 1).then_some(&q[..]),
                        units: units.as_bytes(),
                        data_type: dtype.as_bytes(),
                        mode: w6::ItsMode::Replace,
                    };
                    print(w6::write_its6(file, &input, &opts))?;
                    continue;
                }
                let mut input = ItsInput::new(
                    path.as_bytes(),
                    &times,
                    vals(kind, &v),
                    units.as_bytes(),
                    dtype.as_bytes(),
                );
                input.time_granularity = (gran > 0).then_some(gran);
                let var = TsVariants {
                    quality_words: (qes > 0).then_some((&q[..], qes)),
                    notes: if cn == 1 {
                        Some(TsNotes::Char(&refs))
                    } else if nes > 0 {
                        Some(TsNotes::Int {
                            words: &inotes,
                            element_size: nes,
                        })
                    } else {
                        None
                    },
                };
                print(write_its_variants(file, &input, &var, &opts))?;
            }
            "PATTERN" => {
                let kind = t.s()?;
                let path = t.s()?;
                let units = t.s()?.replace('~', "");
                let dtype = t.s()?.replace('~', "");
                let n: usize = t.n()?;
                let gran: i64 = t.n()?;
                let times = if gran > 0 { t.times(n)? } else { Vec::new() };
                let v = t.values(kind, n)?;
                let mut input = PatternInput::new(
                    path.as_bytes(),
                    vals(kind, &v),
                    units.as_bytes(),
                    dtype.as_bytes(),
                );
                if gran > 0 {
                    input.times = Some(&times);
                    input.time_granularity = Some(gran);
                }
                if six {
                    // Replays of HEC-written cases: irregular patterns are opted in.
                    print(w6::write_pattern6_with(
                        file,
                        &input,
                        &opts,
                        w6::Dss6IrregularPattern::AllowUnconfirmed,
                    ))?;
                } else {
                    print(write_pattern(file, &input, &opts))?;
                }
            }
            o => return Err(format!("bad spec token {o}")),
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

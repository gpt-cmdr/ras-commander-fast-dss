//! Run DSS6 dataset reads from a request list (packet P08b test harness).
//!
//! ```text
//! cargo run --release -p dss-core --example dss6_dataset_dump -- <file.dss> <requests.tsv> <out-dir>
//! ```
//!
//! `requests.tsv`: `id \t pathname \t start \t end`, times as `DDMMMYYYY HHMM` (HHMM may
//! be 2400); empty start/end reads the whole dataset (trimmed, as HEC does). Writes
//! `results.jsonl` and per request `<id>.values.bin` (f64 LE, widened),
//! `<id>.times.bin` (i64 seconds) and `<id>.quality.bin`. Output holds pathnames: keep it
//! on local scratch storage.

use dss_core::dss6::{interval, DatasetIndex, DatasetOptions, Dss6File};
use std::io::Write;
use std::time::Instant;

fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (d, t) = s.split_once(' ')?;
    let jul = interval::parse_d_part(d.as_bytes())?;
    let t = t.trim();
    if t.len() != 4 {
        return None;
    }
    let hh: i64 = t[..2].parse().ok()?;
    let mm: i64 = t[2..].parse().ok()?;
    Some((jul * 1440 + hh * 60 + mm) * 60)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: dss6_dataset_dump <file.dss> <requests.tsv> <out-dir> [--catalog-blocks] [--trim]");
        std::process::exit(2);
    }
    let catalog_blocks = a.iter().any(|x| x == "--catalog-blocks");
    let trim = a.iter().any(|x| x == "--trim");
    let out = std::path::Path::new(&a[3]);
    std::fs::create_dir_all(out)?;
    let file = Dss6File::open(&a[1])?;
    let t0 = Instant::now();
    let index = DatasetIndex::build(&file)?;
    let index_s = t0.elapsed().as_secs_f64();
    let reqs = std::fs::read_to_string(&a[2])?;
    let mut res = std::fs::File::create(out.join("results.jsonl"))?;
    for line in reqs.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let (id, path) = (f[0], f[1]);
        let start = f.get(2).copied().unwrap_or("");
        let end = f.get(3).copied().unwrap_or("");
        let mut opts = if start.is_empty() {
            DatasetOptions::whole()
        } else {
            match (parse_time(start), parse_time(end)) {
                (Some(s), Some(e)) => DatasetOptions::window(s, e),
                _ => {
                    writeln!(res, "{{\"id\":\"{id}\",\"status\":\"bad_request\"}}")?;
                    continue;
                }
            }
        };
        opts.catalog_blocks = catalog_blocks;
        if trim {
            opts.trim_missing = true;
        }
        let t = Instant::now();
        let r = file.read_ts_dataset_indexed(&index, path.as_bytes(), &opts);
        let secs = t.elapsed().as_secs_f64();
        match r {
            Ok(ds) => {
                let s = &ds.series;
                let v = s.values.to_f64();
                let vb: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                std::fs::write(out.join(format!("{id}.values.bin")), vb)?;
                let tb: Vec<u8> = s.times.iter().flat_map(|x| x.to_le_bytes()).collect();
                std::fs::write(out.join(format!("{id}.times.bin")), tb)?;
                if let Some(q) = &s.quality {
                    let qb: Vec<u8> = q.iter().flat_map(|x| x.to_le_bytes()).collect();
                    std::fs::write(out.join(format!("{id}.quality.bin")), qb)?;
                }
                writeln!(
                    res,
                    "{{\"id\":\"{id}\",\"status\":\"ok\",\"n\":{},\"units\":\"{}\",\"type\":\"{}\",\"found\":{},\"missing\":{},\"filled\":{},\"trimmed\":{},\"warnings\":{},\"quality\":{},\"seconds\":{secs:.6}}}",
                    s.len(),
                    s.units.escape_default(),
                    s.data_type.escape_default(),
                    ds.report.blocks_found,
                    ds.report.blocks_missing,
                    ds.report.filled_no_data,
                    ds.report.trimmed,
                    ds.report.warnings.len(),
                    s.quality.is_some(),
                )?;
            }
            Err(e) => {
                writeln!(
                    res,
                    "{{\"id\":\"{id}\",\"status\":\"error\",\"error\":\"{}\",\"seconds\":{secs:.6}}}",
                    e.to_string().escape_default()
                )?;
            }
        }
    }
    println!("{{\"index_s\":{index_s:.3},\"datasets\":{}}}", index.len());
    Ok(())
}

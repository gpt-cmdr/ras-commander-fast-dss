//! Dump every time-series record of a DSS6 file (test harness).
//!
//! ```text
//! cargo run --release -p dss-core --example dss6_ts_dump -- <file.dss> <out-dir> [--max N]
//! ```
//!
//! Writes `<out-dir>/index.jsonl` (one line per live TS record: pathname as hex, type,
//! status, metadata, offsets into the binary files) and the raw little-endian arrays
//! `values.bin` (stored width), `times.bin` (i64 seconds) and `quality.bin` (i32). The
//! output contains pathnames, so keep it on local scratch storage. `--max N` samples about
//! `N` records evenly in catalog order. The file is opened read-only.

use dss_core::dss6::{CatalogOptions, Dss6File, InfoCheck, TsError, TsReadOptions};
use dss_core::TsValues;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

fn js(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                o.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn err_kind(e: &TsError) -> &'static str {
    match e {
        TsError::Dss6(_) => "dss6",
        TsError::NotFound { .. } => "not_found",
        TsError::UnsupportedRecordType { .. } => "unsupported_type",
        TsError::UnsupportedCompression { .. } => "unsupported_compression",
        TsError::PasswordProtected => "password",
        TsError::BadPathname { .. } => "bad_pathname",
        TsError::Corrupt { .. } => "corrupt",
        TsError::TooLarge { .. } => "too_large",
        TsError::NoData { .. } => "no_data",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: dss6_ts_dump <file.dss> <out-dir> [--max N] [--no-arrays]");
        std::process::exit(2);
    }
    let mut max: Option<usize> = None;
    let mut arrays = true;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--max" => {
                max = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--no-arrays" => arrays = false,
            _ => {}
        }
        i += 1;
    }
    let out = std::path::Path::new(&args[2]);
    std::fs::create_dir_all(out)?;
    let t0 = Instant::now();
    let file = Dss6File::open(&args[1])?;
    let opts = CatalogOptions {
        stop_at_knrecs: false,
        verify_hashes: false,
        verify_table_chains: false,
        check_duplicates: false,
        verify_info: InfoCheck::None,
    };
    let walk = file.walk_bins(&opts)?;
    let t_cat = t0.elapsed().as_secs_f64();
    let live: Vec<_> = walk
        .live()
        .filter(|e| (100..120).contains(&e.record_type))
        .collect();
    let step = match max {
        Some(m) if m > 0 && live.len() > m => live.len().div_ceil(m),
        _ => 1,
    };
    let mut idx = BufWriter::new(File::create(out.join("index.jsonl"))?);
    let mut vals = BufWriter::new(File::create(out.join("values.bin"))?);
    let mut tims = BufWriter::new(File::create(out.join("times.bin"))?);
    let mut qual = BufWriter::new(File::create(out.join("quality.bin"))?);
    let (mut voff, mut toff, mut qoff) = (0u64, 0u64, 0u64);
    let (mut n_ok, mut n_err, mut n_vals) = (0usize, 0usize, 0u64);
    let ropts = TsReadOptions::default();
    let t1 = Instant::now();
    for e in live.iter().step_by(step) {
        let res = file
            .read_info(e.info_address.into(), e.pathname.len() as i32)
            .map_err(TsError::from)
            .and_then(|info| {
                let addrs = (
                    info.address,
                    info.data_address,
                    info.quality,
                    info.compression,
                );
                file.decode_ts_info(&info, &ropts).map(|r| (r, addrs))
            });
        match res {
            Ok((r, addrs)) => {
                n_ok += 1;
                n_vals += r.values.len() as u64;
                let (dtype, vbytes): (&str, Vec<u8>) = match &r.values {
                    TsValues::F32(v) => ("f4", v.iter().flat_map(|x| x.to_le_bytes()).collect()),
                    TsValues::F64(v) => ("f8", v.iter().flat_map(|x| x.to_le_bytes()).collect()),
                };
                let n = r.values.len();
                let mut line = format!(
                    "{{\"p\":\"{}\",\"type\":{},\"status\":\"ok\",\"n\":{},\"dtype\":\"{}\",\"units\":{},\"dtype_str\":{},\"offset\":{},\"layout\":\"{:?}\",\"comp\":{},\"tz\":{},\"tzname\":{},\"interval\":{},\"block_jul\":{},\"klnda\":{},\"kindat\":{},\"nonstd\":{},\"qual\":{},\"capacity\":{},\"pseudo\":{},\"nuhead\":{},\"nihead\":{}",
                    hex(&r.pathname),
                    r.record_type,
                    n,
                    dtype,
                    js(&r.units),
                    js(&r.data_type),
                    r.time_offset_minutes,
                    r.layout,
                    r.compression,
                    r.time_zone_offset.map_or("null".into(), |v| v.to_string()),
                    r.time_zone_name.as_deref().map_or("null".into(), js),
                    r.interval_minutes.map_or("null".into(), |v| v.to_string()),
                    r.block_start_julian.map_or("null".into(), |v| v.to_string()),
                    r.logical_values,
                    r.data_words,
                    r.nonstandard_block,
                    r.quality.is_some(),
                    r.irregular_capacity.map_or("null".into(), |v| v.to_string()),
                    r.pseudo_interval_minutes.map_or("null".into(), |v| v.to_string()),
                    r.user_header.len(),
                    r.internal_header.len(),
                );
                line.push_str(&format!(
                    ",\"iaddr\":{},\"daddr\":{},\"kiqual\":{}",
                    addrs.0, addrs.1, addrs.2
                ));
                let _ = addrs.3;
                if arrays {
                    vals.write_all(&vbytes)?;
                    for t in &r.times {
                        tims.write_all(&t.to_le_bytes())?;
                    }
                    line.push_str(&format!(",\"voff\":{voff},\"toff\":{toff}"));
                    voff += vbytes.len() as u64;
                    toff += 8 * r.times.len() as u64;
                    if let Some(q) = &r.quality {
                        for x in q {
                            qual.write_all(&x.to_le_bytes())?;
                        }
                        line.push_str(&format!(",\"qoff\":{qoff}"));
                        qoff += 4 * q.len() as u64;
                    }
                }
                line.push_str("}\n");
                idx.write_all(line.as_bytes())?;
            }
            Err(err) => {
                n_err += 1;
                writeln!(
                    idx,
                    "{{\"p\":\"{}\",\"type\":{},\"status\":\"error\",\"kind\":\"{}\",\"error\":{}}}",
                    hex(&e.pathname),
                    e.record_type,
                    err_kind(&err),
                    js(&err.to_string())
                )?;
            }
        }
    }
    idx.flush()?;
    vals.flush()?;
    tims.flush()?;
    qual.flush()?;
    let t_read = t1.elapsed().as_secs_f64();
    let summary = format!(
        "{{\"live_ts\":{},\"read\":{},\"ok\":{n_ok},\"error\":{n_err},\"values\":{n_vals},\"knrecs\":{},\"live_all\":{},\"catalog_s\":{t_cat:.3},\"read_s\":{t_read:.3},\"step\":{step}}}\n",
        live.len(),
        n_ok + n_err,
        file.header().number_records,
        walk.live().count(),
    );
    std::fs::write(out.join("summary.json"), &summary)?;
    print!("{summary}");
    Ok(())
}

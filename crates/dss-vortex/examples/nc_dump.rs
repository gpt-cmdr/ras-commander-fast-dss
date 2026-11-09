//! Test harness: writes what this crate reads from a NetCDF or HDF5 file, for comparison with
//! HEC-Vortex, one file per item. `tests/support/nc_job.py` runs it under a memory limit.
//! Usage: nc_dump <input> <out-dir> [--max-steps N | --steps a,b,c] [--stored]
//! `--stored` also writes each step's stored bytes, so the decode can be checked separately.
use dss_vortex::source::netcdf::NetcdfFile;
use dss_vortex::DataType;
use std::fmt::Write as _;
use std::path::Path;

fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn text(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("\"{}\"", escape(s)),
        None => "null".to_string(),
    }
}

fn bits(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

fn steps_to_dump(count: usize, limit: usize) -> Vec<usize> {
    if limit == 0 || count <= limit {
        return (0..count).collect();
    }
    let mut steps: Vec<usize> = (0..limit - 2).collect();
    steps.extend([count / 2, count - 1]);
    steps.sort_unstable();
    steps.dedup();
    steps
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    let mut limit = 0;
    let mut chosen: Vec<usize> = Vec::new();
    let mut stored = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--max-steps" => {
                limit = args[i + 1].parse().unwrap();
                i += 1;
            }
            "--steps" => {
                chosen = args[i + 1].split(',').map(|s| s.parse().unwrap()).collect();
                i += 1;
            }
            "--stored" => stored = true,
            other => panic!("{other}"),
        }
        i += 1;
    }
    std::fs::create_dir_all(out.join("dtos")).unwrap();
    if stored {
        std::fs::create_dir_all(out.join("stored")).unwrap();
    }
    let file = match NetcdfFile::open(input) {
        Ok(f) => f,
        Err(e) => {
            let json = format!(
                "{{\"file_error\":\"{}\",\"variables\":[]}}",
                escape(&e.to_string())
            );
            std::fs::write(out.join("dtos.json"), json).unwrap();
            println!("OK file refused: {e}");
            return;
        }
    };
    let mut json = String::from("{\"variables\":[");
    let mut n = 0usize;
    for (k, name) in file.variable_names().iter().enumerate() {
        if k > 0 {
            json.push(',');
        }
        let _ = write!(json, "{{\"variable\":\"{}\"", escape(name));
        match file.variable(name) {
            Err(e) => {
                let _ = write!(json, ",\"error\":\"{}\"}}", escape(&e.to_string()));
                continue;
            }
            Ok(v) => {
                let time_dim = match v.time_dimension() {
                    Some(d) => d.to_string(),
                    None => "null".to_string(),
                };
                let _ = write!(
                    json,
                    ",\"time_dim\":{time_dim},\"dto_count\":{},\"dtos\":[",
                    v.step_count()
                );
                let mut failure = None;
                let steps = if chosen.is_empty() {
                    steps_to_dump(v.step_count(), limit)
                } else {
                    chosen.clone()
                };
                for (j, t) in steps.into_iter().enumerate() {
                    let s = match v.step(t) {
                        Ok(s) => s,
                        Err(e) => {
                            failure = Some(e.to_string());
                            break;
                        }
                    };
                    let cells_name = format!("{n:05}.f32");
                    let mut bytes = Vec::with_capacity(s.cells.len() * 4);
                    for c in &s.cells {
                        bytes.extend_from_slice(&c.to_bits().to_le_bytes());
                    }
                    std::fs::write(out.join("dtos").join(&cells_name), bytes).unwrap();
                    let frame = v.frame(t).err().map(|e| e.to_string());
                    if stored {
                        let bytes = file.stored_step(name, t).unwrap();
                        std::fs::write(out.join("stored").join(&cells_name), bytes).unwrap();
                    }
                    if j > 0 {
                        json.push(',');
                    }
                    let data_type = match s.data_type {
                        DataType::PerCum => "ACCUMULATION",
                        DataType::PerAver => "AVERAGE",
                        DataType::InstVal => "INSTANTANEOUS",
                        DataType::InstCum => "INSTANT_CUMULATIVE",
                    };
                    let time = |v: Option<i64>| match v {
                        Some(m) => m.to_string(),
                        None => "null".to_string(),
                    };
                    let _ = write!(
                        json,
                        "{{\"step\":{t},\"file\":\"{cells_name}\",\"cells\":{},\"nx\":{},\"ny\":{},\
                         \"dx\":\"{}\",\"dy\":\"{}\",\"originX\":\"{}\",\"originY\":\"{}\",\
                         \"noDataValue\":\"{}\",\"wkt\":\"{}\",\"units\":{},\"shortName\":\"{}\",\
                         \"fullName\":\"{}\",\"description\":{},\"standard_name\":{},\"cell_methods\":{},\"start_ms\":{},\"end_ms\":{},\
                         \"dataType\":\"{data_type}\",\"frame_error\":{}}}",
                        s.cells.len(),
                        s.nx,
                        s.ny,
                        bits(s.dx),
                        bits(s.dy),
                        bits(s.origin_x),
                        bits(s.origin_y),
                        bits(s.nodata),
                        escape(&s.wkt),
                        text(&s.units),
                        escape(&s.short_name),
                        escape(&s.full_name),
                        text(&s.description),
                        text(&s.standard_name),
                        text(&s.cell_methods),
                        time(s.start),
                        time(s.end),
                        text(&frame),
                    );
                    n += 1;
                }
                json.push(']');
                if let Some(e) = failure {
                    let _ = write!(json, ",\"error\":\"{}\"", escape(&e));
                }
                json.push('}');
            }
        }
    }
    json.push_str("]}");
    std::fs::write(out.join("dtos.json"), json).unwrap();
    println!(
        "OK cells={n} metadata_estimate={}",
        file.metadata_estimate()
    );
}

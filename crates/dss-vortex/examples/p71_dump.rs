//! Open one input through `import::open_source`, as the importer does, and write every
//! variable's steps, so the reading can be compared with Vortex's.
//!
//! Usage: p71_dump <input> <out-dir> [--snodas-times header] [--snodas-georef header]
//!
//! Writes `frames.json` (a list, or a refusal) and `NNN.f32` (little-endian cell bits) per frame.
use dss_vortex::import::{open_source, OpenOptions};
use dss_vortex::source::snodas::{SnodasGeoreference, SnodasTimes};
use dss_vortex::{Crs, FrameTime, VortexError};
use serde_json::{json, Value};
use std::path::Path;

fn crs_text(crs: &Crs) -> String {
    match crs {
        Crs::Wkt(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

fn time(t: &FrameTime) -> Value {
    match t {
        FrameTime::Invariant => json!({"kind": "invariant"}),
        FrameTime::Instant((d, s)) => json!({"kind": "instant", "day": d, "second": s}),
        FrameTime::Period { start, end } => json!({
            "kind": "period", "start_day": start.0, "start_second": start.1,
            "end_day": end.0, "end_second": end.1
        }),
    }
}

fn refusal(e: &VortexError) -> Value {
    let kind = match e {
        VortexError::Refused { .. } => "refused",
        VortexError::Unsupported { .. } => "unsupported",
        VortexError::Decode { .. } => "decode",
        VortexError::Frame(_) => "frame",
        VortexError::Io(_) => "io",
        _ => "other",
    };
    json!({"refused": true, "kind": kind, "error": e.to_string()})
}

/// The options after `<input> <out-dir>`, in pairs. An option without its value, or one this
/// harness does not know, is an error: it must not run with the default instead.
fn parse_options(rest: &[String]) -> Result<OpenOptions, String> {
    let mut options = OpenOptions::default();
    let mut i = 0;
    while i < rest.len() {
        let Some(value) = rest.get(i + 1) else {
            return Err(format!("{} has no value", rest[i]));
        };
        match (rest[i].as_str(), value.as_str()) {
            ("--snodas-times", "header") => options.snodas.times = SnodasTimes::Header,
            ("--snodas-times", "vortex") => options.snodas.times = SnodasTimes::Vortex,
            ("--snodas-times", "header_as_stated") => {
                options.snodas.times = SnodasTimes::HeaderAsStated
            }
            ("--snodas-georef", "header") => {
                options.snodas.georeference = SnodasGeoreference::Header
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 2;
    }
    Ok(options)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: p71_dump <input> <out-dir> [--snodas-times T] [--snodas-georef header]");
        std::process::exit(2);
    }
    let input = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    let options = match parse_options(&args[3..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("p71_dump: {e}");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(out).unwrap();
    let source = match open_source(input, &options) {
        Ok(s) => s,
        Err(e) => {
            std::fs::write(out.join("frames.json"), refusal(&e).to_string()).unwrap();
            return;
        }
    };
    let mut frames = Vec::new();
    let mut n = 0;
    for variable in source.variables() {
        let steps = source.step_count(&variable).unwrap();
        for step in 0..steps {
            let short = source.short_name(&variable).unwrap();
            let frame = match source.frame(&variable, step) {
                Ok(f) => f,
                Err(e) => {
                    frames.push(json!({"variable": variable, "step": step, "error": refusal(&e)}));
                    continue;
                }
            };
            let p = frame.parts();
            let mut bytes = Vec::with_capacity(p.cells.len() * 4);
            for c in &p.cells {
                bytes.extend_from_slice(&c.to_bits().to_le_bytes());
            }
            let cells_file = format!("{n:03}.f32");
            std::fs::write(out.join(&cells_file), bytes).unwrap();
            n += 1;
            let t = p.transform;
            let six = [
                t.origin_x,
                t.dx,
                t.row_rotation,
                t.origin_y,
                t.column_rotation,
                t.dy,
            ];
            let six_bits: Vec<String> = six
                .iter()
                .map(|v| format!("{:016x}", v.to_bits()))
                .collect();
            let def = source.grid(&variable, step).unwrap();
            frames.push(json!({
                "variable": variable,
                "short_name": short,
                "step": step,
                "cells": cells_file,
                "nx": p.nx,
                "ny": p.ny,
                "transform": six_bits,
                "transform_repr": six,
                "crs": crs_text(&p.crs),
                "nodata": p.nodata.map(|v| format!("{:08x}", v.to_bits())),
                "import_nodata": def.nodata.map(|v| format!("{:08x}", v.to_bits())),
                "time": time(&p.time),
                "data_type": format!("{:?}", p.data_type),
                "units": p.units,
                "description": p.description,
            }));
        }
    }
    let doc = json!({"refused": false, "file_name": source.file_name(), "frames": frames});
    std::fs::write(
        out.join("frames.json"),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::parse_options;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_trailing_option_without_its_value_is_an_error() {
        assert!(parse_options(&args(&["--snodas-times"])).is_err());
        assert!(parse_options(&args(&["--snodas-times", "vortex", "--snodas-georef"])).is_err());
        assert!(parse_options(&args(&["--snodas-times", "sometimes"])).is_err());
        assert!(parse_options(&args(&["--other", "x"])).is_err());
    }

    #[test]
    fn known_options_are_read() {
        assert!(parse_options(&args(&[])).is_ok());
        let o = parse_options(&args(&[
            "--snodas-times",
            "vortex",
            "--snodas-georef",
            "header",
        ]))
        .unwrap();
        assert_eq!(
            o.snodas.times,
            dss_vortex::source::snodas::SnodasTimes::Vortex
        );
        assert_eq!(
            o.snodas.georeference,
            dss_vortex::source::snodas::SnodasGeoreference::Header
        );
    }
}

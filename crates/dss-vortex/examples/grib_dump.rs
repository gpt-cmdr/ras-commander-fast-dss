//! Test harness: writes what this crate reads from a GRIB2 file, for comparison with HEC-Vortex.
//! Usage: grib_dump <input> <out-dir>
use dss_vortex::products::WindowRule;
use dss_vortex::source::grib::{Grib2File, Limits};
use dss_vortex::{Crs, FrameTime};
use std::fmt::Write as _;
use std::path::Path;

fn write_cells(path: &Path, cells: &[f32]) {
    let mut bytes = Vec::with_capacity(cells.len() * 4);
    for c in cells {
        bytes.extend_from_slice(&c.to_bits().to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn bits(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    std::fs::create_dir_all(out.join("records")).unwrap();
    std::fs::create_dir_all(out.join("frames")).unwrap();
    let file = Grib2File::open_with_limits(input, Limits::default()).unwrap();
    let mut json = String::from("{\"records\":[");
    for i in 0..file.record_count() {
        if i > 0 {
            json.push(',');
        }
        let cells = file.decode_record(i);
        let georef = file.georeference(i);
        let _ = write!(json, "{{\"index\":{i}");
        match cells {
            Ok(c) => {
                write_cells(&out.join("records").join(format!("{i:05}.f32")), &c);
                let _ = write!(json, ",\"cells\":{}", c.len());
            }
            Err(e) => {
                let _ = write!(json, ",\"error\":\"{}\"", escape(&e.to_string()));
            }
        }
        match georef {
            Ok(g) => {
                let _ = write!(
                    json,
                    ",\"nx\":{},\"ny\":{},\"originX\":\"{}\",\"originY\":\"{}\",\"dx\":\"{}\",\"dy\":\"{}\",\
                     \"startx\":\"{}\",\"starty\":\"{}\",\"radius\":\"{}\",\"wkt\":\"{}\"",
                    g.nx,
                    g.ny,
                    bits(g.origin_x),
                    bits(g.origin_y),
                    bits(g.dx),
                    bits(g.dy),
                    bits(g.start_x),
                    bits(g.start_y),
                    bits(g.wkt_radius),
                    escape(&g.wkt)
                );
            }
            Err(e) => {
                let _ = write!(json, ",\"georef_error\":\"{}\"", escape(&e.to_string()));
            }
        }
        json.push('}');
    }
    json.push_str("],\"variables\":[");
    let mut n = 0;
    let mut first_var = true;
    for variable in file.variables() {
        if !first_var {
            json.push(',');
        }
        first_var = false;
        let _ = write!(
            json,
            "{{\"name\":\"{}\",\"records\":{:?},\"frames\":[",
            escape(&variable.name),
            variable.steps.iter().map(|s| s.record).collect::<Vec<_>>()
        );
        for step in 0..variable.steps.len() {
            if step > 0 {
                json.push(',');
            }
            // Parity with Vortex is what the gate compares.
            match file.frame_with(&variable.name, step, WindowRule::Vortex) {
                Ok(frame) => {
                    let name = format!("{n:05}.f32");
                    n += 1;
                    write_cells(&out.join("frames").join(&name), frame.cells());
                    let p = frame.parts();
                    let wkt = match &p.crs {
                        Crs::Wkt(w) => w.clone(),
                        _ => String::new(),
                    };
                    let time = match p.time {
                        FrameTime::Period { start, end } => {
                            format!("[{},{},{},{}]", start.0, start.1, end.0, end.1)
                        }
                        FrameTime::Instant(t) => format!("[{},{},{},{}]", t.0, t.1, t.0, t.1),
                        FrameTime::Invariant => "[]".to_string(),
                    };
                    let t = p.transform;
                    let _ = write!(
                        json,
                        "{{\"file\":\"{name}\",\"nx\":{},\"ny\":{},\"originX\":\"{}\",\"originY\":\"{}\",\
                         \"dx\":\"{}\",\"dy\":\"{}\",\"wkt\":\"{}\",\"time\":{time},\"dataType\":\"{:?}\",\
                         \"units\":\"{}\",\"shortName\":\"{}\",\"description\":\"{}\",\"noData\":\"{:08x}\"}}",
                        p.nx,
                        p.ny,
                        bits(t.origin_x),
                        bits(t.origin_y),
                        bits(t.dx),
                        bits(t.dy),
                        escape(&wkt),
                        p.data_type,
                        escape(&p.units),
                        escape(&p.variable),
                        escape(p.description.as_deref().unwrap_or("")),
                        p.nodata.unwrap_or(f32::NAN).to_bits()
                    );
                }
                Err(e) => {
                    let _ = write!(json, "{{\"error\":\"{}\"}}", escape(&e.to_string()));
                }
            }
        }
        json.push_str("]}");
    }
    json.push_str("],\"unlisted\":[");
    for (i, (index, error)) in file.unlisted().iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        let _ = write!(
            json,
            "{{\"index\":{index},\"error\":\"{}\"}}",
            escape(&error.to_string())
        );
    }
    json.push_str("]}");
    std::fs::write(out.join("native.json"), json).unwrap();
}

//! Test harness: writes what this crate reads from a Zarr v2 store, for comparison with xarray
//! and zarr-python, and for the memory-limited probes of `tests/support/zarr_job.py`.
//!
//! Usage: zarr_dump <store> <out-dir> [--variable NAME] [--steps A:B] [--max-steps N]
//!                  [--bbox W,S,E,N | --window R0:R1,C0:C1] [--stored]
//!
//! Writes `<out-dir>/dtos.json`, and per variable `cells_<name>.f32` (little-endian `f32`, step
//! after step) and, with `--stored`, `stored_<name>.bin`. Every failure the reader reports is
//! written into the JSON and the exit code stays 0: only a crash ends it otherwise.
use dss_vortex::source::zarr::{Bbox, Window, ZarrStore};
use dss_vortex::{Crs, FrameTime};
use std::fmt::Write as _;
use std::ops::Range;
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

fn quoted(s: &str) -> String {
    format!("\"{}\"", escape(s))
}

fn range(text: &str) -> Range<usize> {
    let (a, b) = text.split_once(':').expect("A:B");
    a.parse().unwrap()..b.parse().unwrap()
}

fn time_text(time: &FrameTime) -> String {
    let stamp = |(julian, seconds): (i64, i64)| {
        let (y, m, d) = dss_core::time::julian_to_ymd(julian);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    };
    match time {
        FrameTime::Invariant => "invariant".to_string(),
        FrameTime::Instant(t) => format!("instant {}", stamp(*t)),
        FrameTime::Period { start, end } => format!("period {} {}", stamp(*start), stamp(*end)),
    }
}

struct Options {
    variable: Option<String>,
    steps: Option<Range<usize>>,
    max_steps: usize,
    bbox: Option<Bbox>,
    window: Option<Window>,
    stored: bool,
}

fn dump_variable(
    store: &ZarrStore,
    name: &str,
    out: &Path,
    options: &Options,
) -> Result<String, String> {
    let e = |error: dss_vortex::VortexError| error.to_string();
    let v = store.variable(name).map_err(e)?;
    let window = match (&options.window, &options.bbox) {
        (Some(window), _) => window.clone(),
        (None, Some(bbox)) => v.window_for_bbox(bbox).map_err(e)?,
        (None, None) => v.full_window(),
    };
    let (nx, ny, t) = v.grid(&window).map_err(e)?;
    let count = v.step_count();
    let steps = match &options.steps {
        Some(steps) => steps.clone(),
        None if options.max_steps > 0 => 0..count.min(options.max_steps),
        None => 0..count,
    };
    let mut times = Vec::new();
    for step in steps.clone() {
        let instant = v.instant(step).map_err(e)?;
        let time = v.time(step).map_err(e)?;
        times.push(format!(
            "{{\"step\":{step},\"instant\":{},\"time\":{}}}",
            instant.map_or("null".to_string(), |i| i.to_string()),
            quoted(&time_text(&time))
        ));
    }
    let cells = v.read(steps.clone(), &window).map_err(e)?;
    // Written through a small buffer: a second copy of the cells would be this tool's memory, not
    // the reader's, under a memory limit.
    let cells_file = format!("cells_{name}.f32");
    {
        use std::io::Write as _;
        let file = std::fs::File::create(out.join(&cells_file)).map_err(|e| e.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        for cell in &cells {
            writer
                .write_all(&cell.to_le_bytes())
                .map_err(|e| e.to_string())?;
        }
        writer.flush().map_err(|e| e.to_string())?;
    }
    drop(cells);
    let mut stored_file = "null".to_string();
    if options.stored {
        let stored = v.read_stored(steps.clone(), &window).map_err(e)?;
        let file = format!("stored_{name}.bin");
        std::fs::write(out.join(&file), stored).map_err(|e| e.to_string())?;
        stored_file = quoted(&file);
    }
    let crs = match v.crs() {
        Crs::Wkt(text) => quoted(text),
        Crs::Undefined => "null".to_string(),
        other => quoted(&format!("{other:?}")),
    };
    let transform = [
        t.origin_x,
        t.dx,
        t.row_rotation,
        t.origin_y,
        t.column_rotation,
        t.dy,
    ];
    let transform: Vec<String> = transform
        .iter()
        .map(|v| format!("\"{:016x}\"", v.to_bits()))
        .collect();
    Ok(format!(
        "{{\"name\":{},\"steps\":{count},\"read_steps\":[{},{}],\"rows\":[{},{}],\"cols\":[{},{}],\
         \"nx\":{nx},\"ny\":{ny},\"transform_bits\":[{}],\"units\":{},\"description\":{},\"crs\":{crs},\
         \"nodata_bits\":{},\"accumulation\":{},\"times\":[{}],\"cells\":{},\"stored\":{stored_file}}}",
        quoted(name),
        steps.start,
        steps.end,
        window.rows.start,
        window.rows.end,
        window.cols.start,
        window.cols.end,
        transform.join(","),
        quoted(v.units()),
        v.description().map_or("null".to_string(), quoted),
        v.nodata()
            .map_or("null".to_string(), |n| format!("\"{:08x}\"", n.to_bits())),
        v.stated_accumulation()
            .map_or("null".to_string(), |s| s.to_string()),
        times.join(","),
        quoted(&cells_file),
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    let mut options = Options {
        variable: None,
        steps: None,
        max_steps: 0,
        bbox: None,
        window: None,
        stored: false,
    };
    let mut i = 3;
    while i < args.len() {
        let value = || args[i + 1].as_str();
        match args[i].as_str() {
            "--variable" => options.variable = Some(value().to_string()),
            "--steps" => options.steps = Some(range(value())),
            "--max-steps" => options.max_steps = value().parse().unwrap(),
            "--bbox" => {
                let b: Vec<f64> = value().split(',').map(|v| v.parse().unwrap()).collect();
                options.bbox = Some(Bbox {
                    west: b[0],
                    south: b[1],
                    east: b[2],
                    north: b[3],
                });
            }
            "--window" => {
                let (rows, cols) = value().split_once(',').expect("R0:R1,C0:C1");
                options.window = Some(Window {
                    rows: range(rows),
                    cols: range(cols),
                });
            }
            "--stored" => {
                options.stored = true;
                i -= 1;
            }
            other => panic!("{other}"),
        }
        i += 2;
    }
    std::fs::create_dir_all(out).unwrap();
    let json = match ZarrStore::open(input) {
        Err(error) => format!("{{\"file_error\":{}}}", quoted(&error.to_string())),
        Ok(store) => {
            let names = match &options.variable {
                Some(name) => vec![name.clone()],
                None => store.variable_names(),
            };
            let variables: Vec<String> = names
                .iter()
                .map(|name| {
                    dump_variable(&store, name, out, &options).unwrap_or_else(|error| {
                        format!("{{\"name\":{},\"error\":{}}}", quoted(name), quoted(&error))
                    })
                })
                .collect();
            let left_out: Vec<String> = store.left_out().iter().map(|l| quoted(l)).collect();
            format!(
                "{{\"store\":{},\"consolidated\":{},\"left_out\":[{}],\"variables\":[{}]}}",
                quoted(store.file_name()),
                store.is_consolidated(),
                left_out.join(","),
                variables.join(",")
            )
        }
    };
    std::fs::write(out.join("dtos.json"), json).unwrap();
}

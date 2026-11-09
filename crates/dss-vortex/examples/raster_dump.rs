//! Test harness: reads or writes a raster with this crate, for comparison with rasterio and Vortex.
//! Usage: raster_dump read <input> <out-dir>
//!        raster_dump write <input> <output.tif|.asc> [vortex] [classic] [plain]
use dss_vortex::sink_raster::{write_asc, write_geotiff, AscOptions, TiffOptions};
use dss_vortex::source::raster::{CrsStatus, RasterFile};
use dss_vortex::{Crs, FrameTime, VortexError};
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
            c => out.push(c),
        }
    }
    out
}

fn bits(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

fn refusal_json(e: &VortexError) -> String {
    let kind = match e {
        VortexError::Refused { .. } => "refused",
        VortexError::Unsupported { .. } => "unsupported",
        VortexError::Decode { .. } => "decode",
        VortexError::Frame(_) => "frame",
        VortexError::Io(_) => "io",
        _ => "other",
    };
    format!(
        "{{\"refused\":true,\"kind\":\"{kind}\",\"error\":\"{}\"}}",
        escape(&e.to_string())
    )
}

fn read(input: &Path, out: &Path) {
    std::fs::create_dir_all(out).unwrap();
    let file = match RasterFile::open(input) {
        Ok(f) => f,
        Err(e) => {
            std::fs::write(out.join("meta.json"), refusal_json(&e)).unwrap();
            return;
        }
    };
    let parts = file.frame().parts();
    let mut bytes = Vec::with_capacity(parts.cells.len() * 4);
    for c in &parts.cells {
        bytes.extend_from_slice(&c.to_bits().to_le_bytes());
    }
    std::fs::write(out.join("cells.f32"), bytes).unwrap();
    let t = parts.transform;
    let mut json = format!(
        "{{\"refused\":false,\"nx\":{},\"ny\":{},\"format\":\"{:?}\",\"row_order\":\"{:?}\",\"transform\":[\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\"]",
        parts.nx,
        parts.ny,
        file.format(),
        parts.row_order,
        bits(t.origin_x),
        bits(t.dx),
        bits(t.row_rotation),
        bits(t.origin_y),
        bits(t.column_rotation),
        bits(t.dy),
    );
    match parts.nodata {
        Some(n) => {
            let _ = write!(json, ",\"nodata\":\"{:08x}\"", n.to_bits());
        }
        None => json.push_str(",\"nodata\":null"),
    }
    let (kind, wkt) = match &parts.crs {
        Crs::Undefined => ("undefined", String::new()),
        Crs::Wkt(t) => ("wkt", t.clone()),
        Crs::Proj(t) => ("proj", t.clone()),
        Crs::Named(_) => (
            "named",
            dss_vortex::crs::CrsDef::from_frame(&parts.crs)
                .unwrap()
                .to_wkt(),
        ),
    };
    let _ = write!(
        json,
        ",\"crs_kind\":\"{kind}\",\"wkt\":\"{}\",\"crs_status\":\"{}\"",
        escape(&wkt),
        match file.crs_status() {
            CrsStatus::Stated => "stated",
            CrsStatus::NotInFile => "not_in_file",
            CrsStatus::Unresolved { .. } => "unresolved",
        }
    );
    let time = match parts.time {
        FrameTime::Invariant => "null".to_string(),
        FrameTime::Instant(t) => format!("[{},{}]", t.0, t.1),
        FrameTime::Period { start, end } => {
            format!("[{},{},{},{}]", start.0, start.1, end.0, end.1)
        }
    };
    let _ = write!(
        json,
        ",\"time\":{time},\"data_type\":\"{:?}\",\"units\":\"{}\",\"variable\":\"{}\",\"description\":\"{}\"}}",
        parts.data_type,
        escape(&parts.units),
        escape(&parts.variable),
        escape(parts.description.as_deref().unwrap_or("")),
    );
    std::fs::write(out.join("meta.json"), json).unwrap();
}

fn write(input: &Path, output: &Path, flags: &[String]) -> Result<(), VortexError> {
    let frame = RasterFile::open(input)?.into_frame();
    let vortex = flags.iter().any(|f| f == "vortex");
    if output.extension().is_some_and(|e| e == "asc") {
        let options = if vortex {
            AscOptions::vortex()
        } else {
            AscOptions::default()
        };
        write_asc(&frame, output, &options)
    } else {
        let mut options = if vortex {
            TiffOptions::vortex()
        } else {
            TiffOptions::default()
        };
        if flags.iter().any(|f| f == "classic") {
            options.bigtiff = false;
        }
        if flags.iter().any(|f| f == "plain") {
            options.deflate = false;
        }
        write_geotiff(&frame, output, &options)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "read" => read(Path::new(&args[2]), Path::new(&args[3])),
        "write" => {
            if let Err(e) = write(Path::new(&args[2]), Path::new(&args[3]), &args[4..]) {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        other => panic!("unknown mode {other}"),
    }
}

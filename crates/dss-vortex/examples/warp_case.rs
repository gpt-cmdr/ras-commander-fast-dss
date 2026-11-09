//! Comparison tool: run the production warp on one case file and write its cells.
//!
//! Usage: `warp_case <case.txt> <out.f32>`. The case holds `input`, `nx`, `ny`, `ox`, `dx`, `oy`,
//! `dy`, `nodata`, `method`, `wkt`, `start`, `end`, `units`, `variable` and `description`, and
//! optionally `envelope` (min_x,max_x,min_y,max_y in SHG, as Vortex's options state it), `tr` or
//! `cell`, `threads` and `align=vortex`.
//!
//! The input's length must be exactly `nx * ny * 4` bytes. A warp goes through `warp_frame`, and
//! what is printed comes from the frame it returns, one `key=value` to a line: `nx`, `ny`, `chunks`,
//! `gt` (all six coefficients), `nodata_bits`, `start`, `end`, `interval_seconds`, `units`,
//! `variable`, `description` and `wkt`. They are compared with HEC-Vortex's, so the tool
//! declares its own frame rather than being classified under Vortex's.
use dss_vortex::crs::{CrsDef, Envelope};
use dss_vortex::frame::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder,
};
use dss_vortex::warp::{Align, Method, SourceGrid, WarpEnvelope, WarpPlan, WarpSpec};
use std::collections::HashMap;

type AnyError = Box<dyn std::error::Error>;

// Howard Hinnant's days_from_civil, so that julian days here are days since 1970-01-01. Only
// differences and the round trip to text matter to this tool.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `java.time`'s ISO text in UTC, as Vortex's `startTime` prints: `2024-06-14T23:00Z`.
fn parse_time(text: &str) -> Result<(i64, i64), AnyError> {
    let body = text.strip_suffix('Z').ok_or(format!("time {text}"))?;
    let (date, time) = body.split_once('T').ok_or(format!("time {text}"))?;
    let d: Vec<i64> = date.split('-').map(str::parse).collect::<Result<_, _>>()?;
    let t: Vec<i64> = time.split(':').map(str::parse).collect::<Result<_, _>>()?;
    if d.len() != 3 || !(t.len() == 2 || t.len() == 3) {
        return Err(format!("time {text}").into());
    }
    let seconds = t[0] * 3600 + t[1] * 60 + t.get(2).copied().unwrap_or(0);
    Ok((days_from_civil(d[0], d[1], d[2]), seconds))
}

fn format_time(t: (i64, i64)) -> String {
    let (y, m, d) = civil_from_days(t.0);
    let (hh, mm, ss) = (t.1 / 3600, t.1 % 3600 / 60, t.1 % 60);
    if ss == 0 {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }
}

fn main() -> Result<(), AnyError> {
    let args: Vec<String> = std::env::args().collect();
    let text = std::fs::read_to_string(&args[1])?;
    let kv: HashMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |k: &str| -> Result<&str, AnyError> {
        kv.get(k).copied().ok_or(format!("missing {k}").into())
    };
    let f = |k: &str| -> Result<f64, AnyError> { Ok(get(k)?.parse::<f64>()?) };
    let (nx, ny): (usize, usize) = (get("nx")?.parse()?, get("ny")?.parse()?);
    let raw = std::fs::read(get("input")?)?;
    if nx.checked_mul(ny).and_then(|n| n.checked_mul(4)) != Some(raw.len()) {
        return Err(format!("input is {} bytes, not {nx} x {ny} x 4", raw.len()).into());
    }
    let data: Vec<f32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let transform = GeoTransform::axis_aligned(f("ox")?, f("dx")?, f("oy")?, f("dy")?);
    let wkt = get("wkt")?;
    let source = SourceGrid {
        nx,
        ny,
        transform,
        crs: CrsDef::parse_wkt(wkt)?,
    };
    let method = match get("method")? {
        "near" => Method::Near,
        "bilinear" => Method::Bilinear,
        "average" => Method::Average,
        other => return Err(format!("method {other}").into()),
    };
    let mut spec = WarpSpec::new(method);
    spec.target = Some(CrsDef::shg());
    spec.cell_size = kv.get("cell").map(|v| v.parse()).transpose()?;
    if let Some(e) = kv.get("envelope") {
        let v: Vec<f64> = e
            .split(',')
            .map(|p| p.parse::<f64>())
            .collect::<Result<_, _>>()?;
        spec.envelope = Some(WarpEnvelope {
            envelope: Envelope {
                min_x: v[0],
                max_x: v[1],
                min_y: v[2],
                max_y: v[3],
            },
            crs: Some(CrsDef::shg()),
        });
    }
    if kv.get("align") == Some(&"vortex") {
        spec.align = Align::Vortex;
    }
    spec.threads = kv.get("threads").map_or(Ok(1), |v| v.parse())?;
    let plan = WarpPlan::new(&source, &spec)?;
    let nodata = get("nodata")?.parse::<f32>()?;
    let (start, end) = (parse_time(get("start")?)?, parse_time(get("end")?)?);
    let (time, data_type) = if start == end {
        (FrameTime::Instant(start), DataType::InstVal)
    } else {
        (FrameTime::Period { start, end }, DataType::PerCum)
    };
    let row_order = if transform.dy > 0.0 {
        RowOrder::SouthToNorth
    } else {
        RowOrder::NorthToSouth
    };
    let frame = GridFrame::new(GridFrameParts {
        cells: data,
        nx,
        ny,
        row_order,
        transform,
        crs: Crs::Wkt(wkt.to_string()),
        nodata: Some(nodata),
        time,
        data_type,
        units: get("units")?.to_string(),
        variable: get("variable")?.to_string(),
        description: Some(get("description")?.to_string()),
    })?;
    let chunks = plan.chunk_count(nodata);
    let out = plan.warp_frame(&frame)?;
    let p = out.parts();
    let mut bytes = Vec::with_capacity(p.cells.len() * 4);
    for v in &p.cells {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&args[2], bytes)?;
    let t = p.transform;
    let (start, end) = match p.time {
        FrameTime::Instant(t) => (t, t),
        FrameTime::Period { start, end } => (start, end),
        FrameTime::Invariant => return Err("the warp lost the frame's time".into()),
    };
    println!("nx={}", p.nx);
    println!("ny={}", p.ny);
    println!("chunks={chunks}");
    println!(
        "gt={:?},{:?},{:?},{:?},{:?},{:?}",
        t.origin_x, t.dx, t.row_rotation, t.origin_y, t.column_rotation, t.dy
    );
    let marker = p.nodata.ok_or("the warp lost the frame's nodata marker")?;
    println!("nodata_bits={:#010x}", marker.to_bits());
    println!("start={}", format_time(start));
    println!("end={}", format_time(end));
    println!(
        "interval_seconds={}",
        (end.0 - start.0) * 86_400 + end.1 - start.1
    );
    println!("units={}", p.units);
    println!("variable={}", p.variable);
    println!("description={}", p.description.as_deref().unwrap_or(""));
    let crs = CrsDef::from_frame(&p.crs)?.to_wkt();
    println!(
        "wkt={}",
        crs.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    Ok(())
}

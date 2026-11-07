//! Read-only grid audit/dump, no HEC runtime and no extra dependencies.
//! Usage: grid_dump <6|7> <input.dss> <output-prefix> [maximum-records]
//! Writes metadata JSONL and little-endian f32 data/range/i32 count arrays to .bin.
//! These outputs may contain client data: keep them outside the repository.
use dss_core::{dss6::Dss6File, dss7::Dss7File, Grid, GridError};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn number(v: Option<f32>) -> String {
    v.map(|v| {
        if v.is_nan() {
            "NaN".into()
        } else if v == f32::INFINITY {
            "Infinity".into()
        } else if v == f32::NEG_INFINITY {
            "-Infinity".into()
        } else {
            format!("{:?}", f64::from(v))
        }
    })
    .unwrap_or_else(|| "null".into())
}
fn emit(
    g: &Grid,
    meta: &mut impl Write,
    bin: &mut impl Write,
    offset: &mut u64,
) -> std::io::Result<()> {
    let mut fields = vec![format!(
        "\"pathname\":{}",
        json_string(&String::from_utf8_lossy(&g.pathname))
    )];
    macro_rules! n {
        ($key:expr,$value:expr) => {
            fields.push(format!("\"{}\":{}", $key, $value));
        };
    }
    macro_rules! s {
        ($key:expr,$value:expr) => {
            fields.push(format!("\"{}\":{}", $key, json_string($value)));
        };
    }
    n!("record_type", g.record_type);
    n!("grid_type", g.record_type);
    n!("struct_version", g.struct_version);
    n!("version", g.version);
    n!("data_type", g.data_type);
    s!("data_units", &g.data_units);
    s!("data_source", &g.data_source);
    n!("lower_left_cell_x", g.lower_left_cell_x);
    n!("lower_left_cell_y", g.lower_left_cell_y);
    n!("number_of_cells_x", g.nx);
    n!("number_of_cells_y", g.ny);
    n!("cell_size", number(Some(g.cell_size)));
    n!("compression_method", g.compression_method);
    n!("sizeof_compressed_elements", g.sizeof_compressed_elements);
    n!(
        "compression_scale_factor",
        number(Some(g.compression_scale_factor))
    );
    n!("compression_base", number(Some(g.compression_base)));
    n!("storage_data_type", g.storage_data_type);
    n!("number_of_ranges", g.range_limit_table.len());
    n!("min_data_value", number(g.min_data_value));
    n!("max_data_value", number(g.max_data_value));
    n!("mean_data_value", number(g.mean_data_value));
    s!("srs_name", &g.srs_name);
    n!("srs_definition_type", g.srs_definition_type);
    s!("srs_definition", &g.srs_definition);
    n!(
        "x_coord_of_grid_cell_zero",
        number(Some(g.x_coord_of_grid_cell_zero))
    );
    n!(
        "y_coord_of_grid_cell_zero",
        number(Some(g.y_coord_of_grid_cell_zero))
    );
    n!("null_value", number(Some(g.null_value)));
    s!("time_zone_id", &g.time_zone_id);
    n!("time_zone_raw_offset", g.time_zone_raw_offset);
    n!("is_interval", g.is_interval);
    n!("is_time_stamped", g.is_time_stamped);
    n!(
        "start_seconds",
        g.start_time
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into())
    );
    n!(
        "end_seconds",
        g.end_time
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into())
    );
    if let Some(p) = &g.albers {
        n!("projection_datum", p.datum);
        s!("projection_units", &p.units);
        n!(
            "first_standard_parallel",
            number(Some(p.first_standard_parallel))
        );
        n!(
            "second_standard_parallel",
            number(Some(p.second_standard_parallel))
        );
        n!("central_meridian", number(Some(p.central_meridian)));
        n!(
            "latitude_of_projection_origin",
            number(Some(p.latitude_of_projection_origin))
        );
        n!("false_easting", number(Some(p.false_easting)));
        n!("false_northing", number(Some(p.false_northing)));
    }
    n!("offset", *offset);
    for v in &g.data {
        bin.write_all(&v.to_le_bytes())?;
    }
    for v in &g.range_limit_table {
        bin.write_all(&v.to_le_bytes())?;
    }
    for v in &g.number_equal_or_exceeding_range_limit {
        bin.write_all(&v.to_le_bytes())?;
    }
    *offset += (g.data.len() + 2 * g.range_limit_table.len()) as u64 * 4;
    writeln!(meta, "{{{}}}", fields.join(","))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        return Err("grid_dump <6|7> <input.dss> <output-prefix> [maximum-records]".into());
    }
    let limit: usize = args
        .get(4)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(usize::MAX);
    let before = std::fs::metadata(&args[2])?;
    let start = Instant::now();
    let mut meta = BufWriter::new(File::create(format!("{}.jsonl", args[3]))?);
    let mut bin: Box<dyn Write> = if args.iter().any(|s| s == "--audit") {
        Box::new(std::io::sink())
    } else {
        Box::new(BufWriter::new(File::create(format!("{}.bin", args[3]))?))
    };
    let mut offset = 0;
    let mut ok = 0;
    let mut errors = 0;
    let mut read =
        |path: &[u8], result: Result<Grid, GridError>| -> Result<(), Box<dyn std::error::Error>> {
            match result {
                Ok(g) => {
                    emit(&g, &mut meta, &mut bin, &mut offset)?;
                    ok += 1;
                }
                Err(e) => {
                    writeln!(
                        meta,
                        "{{\"pathname\":{},\"error\":{}}}",
                        json_string(&String::from_utf8_lossy(path)),
                        json_string(&e.to_string())
                    )?;
                    errors += 1;
                }
            }
            Ok(())
        };
    if args[1] == "6" {
        let f = Dss6File::open(&args[2])?;
        let cat = f.catalog()?;
        let entries: Vec<_> = cat
            .live()
            .filter(|e| (400..450).contains(&e.record_type))
            .collect();
        let stride = entries.len().div_ceil(limit.max(1)).max(1);
        for e in entries.into_iter().step_by(stride).take(limit) {
            let result = f
                .read_info(
                    dss_core::dss6::source::byte_offset_word(e.info_address)
                        .ok_or("bad info offset")?,
                    e.pathname.len() as i32,
                )
                .map_err(GridError::from)
                .and_then(|i| f.decode_grid_info(&i));
            read(&e.pathname, result)?;
        }
    } else {
        let mut f = Dss7File::open(&args[2])?;
        let cat = f.catalog()?;
        let indices: Vec<_> = cat
            .catalog
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.status,
                    dss_core::RecordStatus::Live | dss_core::RecordStatus::Alias
                ) && (400..450).contains(&e.record_type)
            })
            .map(|(i, _)| i)
            .collect();
        let stride = indices.len().div_ceil(limit.max(1)).max(1);
        for i in indices.into_iter().step_by(stride).take(limit) {
            let result = f
                .record_at(&cat, i)
                .map_err(GridError::from)
                .and_then(|r| f.read_grid_record(&r));
            read(&cat.catalog.entries[i].pathname, result)?;
        }
    }
    meta.flush()?;
    bin.flush()?;
    let after = std::fs::metadata(&args[2])?;
    println!(
        "{{\"ok\":{ok},\"errors\":{errors},\"elapsed_seconds\":{},\"unchanged\":{}}}",
        start.elapsed().as_secs_f64(),
        before.len() == after.len() && before.modified()? == after.modified()?
    );
    if errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}

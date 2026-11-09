//! Dump every paired-data record of a DSS6 or DSS7 file (test harness).
//!
//! ```text
//! cargo run --release -p dss-core --example pd_dump -- <file.dss> <out-dir> [--max N] [--no-arrays]
//! ```
//!
//! Writes `<out-dir>/index.jsonl` (one line per live PD/PDD record: pathname as hex,
//! sizes, units, labels, extras, offsets into `values.bin`) and `values.bin` (ordinates
//! then curve values, little-endian, stored width), plus `summary.json`. The output
//! contains pathnames: keep it on local scratch storage. `--max N` samples about `N`
//! records evenly in catalog order. The file is opened read-only.

use dss_core::dss6::{CatalogOptions, Dss6File, InfoCheck};
use dss_core::dss7::Dss7File;
use dss_core::{DssVersion, FileSignature, PairedData, PdValues};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
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

fn bytes_of(v: &PdValues) -> (&'static str, Vec<u8>) {
    match v {
        PdValues::F32(x) => ("f4", x.iter().flat_map(|f| f.to_le_bytes()).collect()),
        PdValues::F64(x) => ("f8", x.iter().flat_map(|f| f.to_le_bytes()).collect()),
    }
}

struct Out {
    idx: BufWriter<File>,
    vals: BufWriter<File>,
    off: u64,
    arrays: bool,
    ok: usize,
    err: usize,
    multi: usize,
    labeled: usize,
    pdd: usize,
    values: u64,
}

impl Out {
    fn ok(&mut self, pd: &PairedData, extra: &str) -> std::io::Result<()> {
        self.ok += 1;
        if pd.number_curves > 1 {
            self.multi += 1;
        }
        if !pd.labels.is_empty() {
            self.labeled += 1;
        }
        if pd.record_type == 205 {
            self.pdd += 1;
        }
        self.values += (pd.ordinates.len() + pd.values.len()) as u64;
        let labels: Vec<String> = pd.labels.iter().map(|l| js(l)).collect();
        let (dtype, ob) = bytes_of(&pd.ordinates);
        let (_, vb) = bytes_of(&pd.values);
        let mut line = format!(
            "{{\"p\":\"{}\",\"type\":{},\"status\":\"ok\",\"no\":{},\"nc\":{},\"dtype\":\"{dtype}\",\"ui\":{},\"ti\":{},\"ud\":{},\"td\":{},\"labels\":[{}],\"tz\":{},\"xaxis\":{},\"xp\":{},\"yp\":{},\"nuh\":{}{extra}",
            hex(&pd.pathname),
            pd.record_type,
            pd.number_ordinates,
            pd.number_curves,
            js(&pd.units_independent),
            js(&pd.type_independent),
            js(&pd.units_dependent),
            js(&pd.type_dependent),
            labels.join(","),
            pd.time_zone.as_deref().map_or("null".into(), js),
            pd.independent_is_x_axis,
            pd.x_precision,
            pd.y_precision,
            pd.user_header.len(),
        );
        if self.arrays {
            self.vals.write_all(&ob)?;
            self.vals.write_all(&vb)?;
            line.push_str(&format!(",\"off\":{}", self.off));
            self.off += (ob.len() + vb.len()) as u64;
        }
        line.push_str("}\n");
        self.idx.write_all(line.as_bytes())
    }

    fn err(&mut self, path: &[u8], rtype: i32, kind: &str, msg: &str) -> std::io::Result<()> {
        self.err += 1;
        writeln!(
            self.idx,
            "{{\"p\":\"{}\",\"type\":{rtype},\"status\":\"error\",\"kind\":\"{kind}\",\"error\":{}}}",
            hex(path),
            js(msg)
        )
    }
}

fn step_for(n: usize, max: Option<usize>) -> usize {
    match max {
        Some(m) if m > 0 && n > m => n.div_ceil(m),
        _ => 1,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: pd_dump <file.dss> <out-dir> [--max N] [--no-arrays]");
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
    let out_dir = std::path::Path::new(&args[2]);
    std::fs::create_dir_all(out_dir)?;
    let mut head = vec![0u8; 512];
    let n = File::open(&args[1])?.read(&mut head)?;
    let sig = FileSignature::from_bytes(&head[..n])?;
    let mut o = Out {
        idx: BufWriter::new(File::create(out_dir.join("index.jsonl"))?),
        vals: BufWriter::new(File::create(out_dir.join("values.bin"))?),
        off: 0,
        arrays,
        ok: 0,
        err: 0,
        multi: 0,
        labeled: 0,
        pdd: 0,
        values: 0,
    };
    let t0 = Instant::now();
    let (n_pd, step, t_cat);
    match sig.version {
        DssVersion::V7 => {
            let mut f = Dss7File::open(&args[1])?;
            let cat = f.catalog()?;
            t_cat = t0.elapsed().as_secs_f64();
            let mut seen = HashSet::new();
            let paths: Vec<(Vec<u8>, i32)> = cat
                .catalog
                .entries
                .iter()
                .filter(|e| e.is_live() && (e.record_type == 200 || e.record_type == 205))
                .filter(|e| seen.insert(e.pathname.clone()))
                .map(|e| (e.pathname.clone(), e.record_type))
                .collect();
            n_pd = paths.len();
            step = step_for(n_pd, max);
            for (p, t) in paths.iter().step_by(step) {
                match f.read_pd(p) {
                    Ok(r) => {
                        let m = &r.meta;
                        let extra = format!(
                            ",\"labels_raw\":\"{}\",\"ll\":{},\"padded\":{},\"mismatch\":{},\"nih\":{},\"nh2\":{}",
                            hex(&m.labels_raw),
                            m.labels_length_stored,
                            m.units_padded,
                            m.value_count_mismatch,
                            m.internal_header_ints,
                            m.header2_ints
                        );
                        o.ok(&r.pd, &extra)?;
                    }
                    Err(e) => {
                        let kind = match &e {
                            dss_core::dss7::PdError::Dss7(_) => "dss7",
                            dss_core::dss7::PdError::NotFound { .. } => "not_found",
                            dss_core::dss7::PdError::UnsupportedRecordType { .. } => {
                                "unsupported_type"
                            }
                            dss_core::dss7::PdError::Corrupt { .. } => "corrupt",
                        };
                        o.err(p, *t, kind, &e.to_string())?;
                    }
                }
            }
        }
        DssVersion::V6 => {
            let f = Dss6File::open(&args[1])?;
            let opts = CatalogOptions {
                stop_at_knrecs: false,
                verify_hashes: false,
                verify_table_chains: false,
                check_duplicates: false,
                verify_info: InfoCheck::None,
            };
            let walk = f.walk_bins(&opts)?;
            t_cat = t0.elapsed().as_secs_f64();
            let live: Vec<_> = walk
                .live()
                .filter(|e| e.record_type == 200 || e.record_type == 205)
                .collect();
            n_pd = live.len();
            step = step_for(n_pd, max);
            for e in live.iter().step_by(step) {
                let res = f
                    .read_info(e.info_address.into(), e.pathname.len() as i32)
                    .map_err(dss_core::dss6::PdError::from)
                    .and_then(|info| f.decode_pd_info(&info));
                match res {
                    Ok(r) => {
                        let m = &r.meta;
                        let extra = format!(
                            ",\"style\":\"{:?}\",\"kinihe\":{},\"kindat\":{},\"klnda\":{},\"stale\":{},\"extra_words\":{},\"kiprec\":{}",
                            m.label_style,
                            m.internal_header_words,
                            m.data_words,
                            m.logical_values,
                            m.logical_values_stale,
                            m.extra_data_words,
                            m.precision_raw
                        );
                        o.ok(&r.pd, &extra)?;
                    }
                    Err(err) => {
                        use dss_core::dss6::PdError as E;
                        let kind = match &err {
                            E::Dss6(_) => "dss6",
                            E::NotFound { .. } => "not_found",
                            E::UnsupportedRecordType { .. } => "unsupported_type",
                            E::PasswordProtected => "password",
                            E::Corrupt { .. } => "corrupt",
                        };
                        o.err(&e.pathname, e.record_type, kind, &err.to_string())?;
                    }
                }
            }
        }
    }
    o.idx.flush()?;
    o.vals.flush()?;
    let total = t0.elapsed().as_secs_f64();
    let summary = format!(
        "{{\"version\":\"{}\",\"live_pd\":{n_pd},\"read\":{},\"ok\":{},\"error\":{},\"multi_curve\":{},\"labeled\":{},\"pdd\":{},\"values\":{},\"catalog_s\":{t_cat:.3},\"total_s\":{total:.3},\"step\":{step}}}\n",
        sig.version_string,
        o.ok + o.err,
        o.ok,
        o.err,
        o.multi,
        o.labeled,
        o.pdd,
        o.values,
    );
    std::fs::write(out_dir.join("summary.json"), &summary)?;
    print!("{summary}");
    Ok(())
}

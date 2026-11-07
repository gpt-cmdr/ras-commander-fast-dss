//! DSS7 time-series harness for oracle comparison and corpus runs (packet P08a).
//!
//! ```text
//! dss7_ts_dump records <file.dss> <outdir>
//!     Decode every live time-series record the way hecdss reads one pathname with no
//!     time window. Writes <outdir>/index.jsonl (pathnames: keep off the repo) and
//!     <outdir>/data.bin (little-endian values, i64 times, i32 quality).
//! dss7_ts_dump datasets <file.dss> <requests.tsv> <outdir>
//!     requests.tsv: "<id>\t<pathname>\t<start|->\t<end|->\t<trim 0|1>" with HEC epoch
//!     seconds. Same output format, keyed by id.
//! dss7_ts_dump corpus <list.tsv> <out.jsonl> [--max N] [--threads T]
//!     list.tsv: "<id>\t<path>". Per file: decode every (or an even sample of N) live TS
//!     record; counts per type and error kind, consistency with kinfoNumberData /
//!     kinfoLogicalNumber, size/mtime before and after. No pathnames in the output.
//! dss7_ts_dump time <file.dss>
//!     Wall time to decode every TS record.
//! ```
//!
//! Files are opened read-only through `Dss7File::open`; nothing is written to them.

use dss_core::dss7::{Dss7Error, Dss7File, Dss7Ts, TsError, TsReadOptions, TsWindow};
use dss_core::TsValues;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

fn json_str(b: &[u8]) -> String {
    let mut o = String::from("\"");
    for &c in b {
        let c = c as char;
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
        TsError::Dss7(Dss7Error::BadInfoBlock { .. }) => "bad_info",
        TsError::Dss7(_) => "dss7",
        TsError::NotFound { .. } => "not_found",
        TsError::UnsupportedRecordType { .. } => "unsupported",
        TsError::BadPathname { .. } => "bad_pathname",
        TsError::BadWindow => "bad_window",
        TsError::Corrupt { .. } => "corrupt",
        TsError::TooManyValues { .. } => "too_many",
    }
}

struct Sink {
    index: BufWriter<std::fs::File>,
    data: BufWriter<std::fs::File>,
    off: u64,
}

impl Sink {
    fn new(dir: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Sink {
            index: BufWriter::new(std::fs::File::create(format!("{dir}/index.jsonl"))?),
            data: BufWriter::new(std::fs::File::create(format!("{dir}/data.bin"))?),
            off: 0,
        })
    }

    fn write_bytes(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.data.write_all(b)?;
        self.off += b.len() as u64;
        Ok(())
    }

    fn ok(&mut self, key: &str, r: &Dss7Ts, extra: &str) -> std::io::Result<()> {
        let off = self.off;
        let (elem, n_vals) = match &r.ts.values {
            TsValues::F32(v) => {
                for x in v {
                    self.data.write_all(&x.to_le_bytes())?;
                }
                self.off += v.len() as u64 * 4;
                (1, v.len())
            }
            TsValues::F64(v) => {
                for x in v {
                    self.data.write_all(&x.to_le_bytes())?;
                }
                self.off += v.len() as u64 * 8;
                (2, v.len())
            }
        };
        let tb: Vec<u8> = r.ts.times.iter().flat_map(|t| t.to_le_bytes()).collect();
        self.write_bytes(&tb)?;
        if let Some(q) = &r.ts.quality {
            let qb: Vec<u8> = q.iter().flat_map(|t| t.to_le_bytes()).collect();
            self.write_bytes(&qb)?;
        }
        let m = &r.meta;
        writeln!(
            self.index,
            "{{{key},\"ok\":true,\"type\":{},\"n\":{},\"nvals\":{},\"elem\":{},\"units\":{},\"data_type\":{},\"tz\":{},\"gran\":{},\"base\":{},\"qes\":{},\"off\":{},\"pattern\":{},\"blocks\":{},\"found\":{},\"offset_s\":{},\"precision\":{}{extra}}}",
            r.ts.record_type,
            r.ts.times.len(),
            n_vals,
            elem,
            json_str(r.ts.units.as_bytes()),
            json_str(r.ts.data_type.as_bytes()),
            json_str(r.ts.time_zone.as_deref().unwrap_or("").as_bytes()),
            m.time_granularity_seconds,
            m.julian_base_date,
            if r.ts.quality.is_some() { m.quality_element_size } else { 0 },
            off,
            m.pattern,
            m.blocks_in_window,
            m.blocks_found,
            m.time_offset_seconds,
            m.precision,
        )
    }

    fn err(&mut self, key: &str, rtype: i32, e: &TsError) -> std::io::Result<()> {
        writeln!(
            self.index,
            "{{{key},\"ok\":false,\"type\":{rtype},\"err\":\"{}\",\"msg\":{}}}",
            err_kind(e),
            json_str(e.to_string().as_bytes())
        )
    }
}

/// Live TS pathnames in catalog order (first occurrence of each pathname).
fn ts_paths(f: &mut Dss7File<std::fs::File>) -> Result<Vec<(Vec<u8>, i32)>, Dss7Error> {
    let cat = f.catalog()?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in &cat.catalog.entries {
        if e.is_live() && (100..200).contains(&e.record_type) && seen.insert(e.pathname.clone()) {
            out.push((e.pathname.clone(), e.record_type));
        }
    }
    Ok(out)
}

/// Read one record by pathname, as hecdss does (lookup, then the record's block).
fn read_by_path(
    f: &mut Dss7File<std::fs::File>,
    path: &[u8],
    rtype: i32,
) -> Result<Dss7Ts, TsError> {
    let rec = f.lookup(path)?.ok_or_else(|| TsError::NotFound {
        pathname: path.to_vec(),
    })?;
    if rtype == 102 || rtype == 107 {
        let p = f.read_ts_profile_record(&rec)?;
        return Ok(Dss7Ts {
            ts: dss_core::TimeSeries {
                pathname: p.pathname,
                record_type: p.record_type,
                kind: dss_core::TsKind::Regular {
                    interval_seconds: p.meta.interval.code,
                },
                times: p.times,
                values: p.values,
                quality: None,
                units: p.value_units,
                data_type: p.data_type,
                time_zone: p.time_zone,
            },
            meta: p.meta,
        });
    }
    f.read_ts_record(&rec)
}

/// `DSS7_TS_VIA_CATALOG=1`: open each record through its catalog entry
/// (`Dss7File::record_at`) instead of a hash lookup, which also reaches entries in orphan
/// bins that HEC cannot read.
fn records_via_catalog(file: &str, out: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = Dss7File::open(file)?;
    let cat = f.catalog()?;
    let mut sink = Sink::new(out)?;
    let mut seen = HashSet::new();
    for (i, e) in cat.catalog.entries.iter().enumerate() {
        if !(e.is_live() && (100..200).contains(&e.record_type) && seen.insert(e.pathname.clone()))
        {
            continue;
        }
        let key = format!("\"i\":{i},\"path\":{}", json_str(&e.pathname));
        let r = f
            .record_at(&cat, i)
            .map_err(TsError::from)
            .and_then(|rec| f.read_ts_record(&rec));
        match r {
            Ok(r) => sink.ok(&key, &r, "")?,
            Err(err) => sink.err(&key, e.record_type, &err)?,
        }
    }
    sink.index.flush()?;
    sink.data.flush()?;
    Ok(())
}

fn records(file: &str, out: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("DSS7_TS_VIA_CATALOG").is_ok_and(|v| v == "1") {
        return records_via_catalog(file, out);
    }
    let mut f = Dss7File::open(file)?;
    let paths = ts_paths(&mut f)?;
    let mut sink = Sink::new(out)?;
    let t0 = Instant::now();
    for (i, (p, t)) in paths.iter().enumerate() {
        let key = format!("\"i\":{i},\"path\":{}", json_str(p));
        match read_by_path(&mut f, p, *t) {
            Ok(r) => sink.ok(&key, &r, "")?,
            Err(e) => sink.err(&key, *t, &e)?,
        }
    }
    sink.index.flush()?;
    sink.data.flush()?;
    eprintln!(
        "{} records in {:.2} s",
        paths.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn datasets(file: &str, req: &str, out: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = Dss7File::open(file)?;
    let cat = f.catalog()?;
    let mut sink = Sink::new(out)?;
    let text = std::fs::read(req)?;
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&[u8]> = line.split(|&b| b == b'\t').collect();
        if cols.len() < 5 {
            continue;
        }
        let num = |c: &[u8]| -> Option<i64> { std::str::from_utf8(c).ok()?.trim().parse().ok() };
        let window = match (num(cols[2]), num(cols[3])) {
            (Some(start), Some(end)) => Some(TsWindow { start, end }),
            _ => None,
        };
        let opts = TsReadOptions {
            window,
            trim_missing: cols[4] == b"1",
        };
        let key = format!(
            "\"id\":{},\"path\":{}",
            json_str(cols[0]),
            json_str(cols[1])
        );
        let t0 = Instant::now();
        let r = f.read_ts_with_catalog(&cat, cols[1], &opts);
        let extra = format!(",\"elapsed_s\":{:.6}", t0.elapsed().as_secs_f64());
        match r {
            Ok(r) => sink.ok(&key, &r, &extra)?,
            Err(e) => sink.err(&key, 0, &e)?,
        }
    }
    sink.index.flush()?;
    sink.data.flush()?;
    Ok(())
}

fn stat_of(path: &str) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

fn corpus_one(id: &str, path: &str, max: usize) -> String {
    let t0 = Instant::now();
    let before = stat_of(path);
    let mut f = match Dss7File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return format!(
                "{{\"id\":{},\"open_error\":{}}}",
                json_str(id.as_bytes()),
                json_str(e.to_string().as_bytes())
            )
        }
    };
    let paths = match ts_paths(&mut f) {
        Ok(p) => p,
        Err(e) => {
            return format!(
                "{{\"id\":{},\"catalog_error\":{}}}",
                json_str(id.as_bytes()),
                json_str(e.to_string().as_bytes())
            )
        }
    };
    let total = paths.len();
    let step = if max > 0 && total > max {
        total as f64 / max as f64
    } else {
        1.0
    };
    let mut picks: Vec<usize> = Vec::new();
    let mut x = 0.0f64;
    while (x as usize) < total && (max == 0 || picks.len() < max.max(1)) {
        picks.push(x as usize);
        x += step;
    }
    let mut ok: BTreeMap<i32, u64> = BTreeMap::new();
    let mut errs: BTreeMap<String, u64> = BTreeMap::new();
    let (
        mut values,
        mut reg_logical_mismatch,
        mut irr_count_mismatch,
        mut reg_checked,
        mut irr_checked,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut err_examples: Vec<String> = Vec::new();
    for &i in &picks {
        let (p, t) = &paths[i];
        let rec = match f.lookup(p) {
            Ok(Some(r)) => r,
            Ok(None) => {
                *errs.entry(format!("{t}:not_found")).or_default() += 1;
                continue;
            }
            Err(e) => {
                *errs
                    .entry(format!("{t}:{}", err_kind(&TsError::Dss7(e))))
                    .or_default() += 1;
                continue;
            }
        };
        let r = if *t == 102 || *t == 107 {
            f.read_ts_profile_record(&rec)
                .map(|p| (p.times.len(), p.meta.pattern, p.meta.interval.code))
        } else {
            f.read_ts_record(&rec)
                .map(|r| (r.ts.times.len(), r.meta.pattern, r.meta.interval.code))
        };
        match r {
            Ok((n, pattern, code)) => {
                *ok.entry(*t).or_default() += 1;
                values += n as u64;
                if !pattern {
                    if code > 0 {
                        reg_checked += 1;
                        if rec.info.logical_number > 0 && n as i64 != rec.info.logical_number {
                            reg_logical_mismatch += 1;
                        }
                    } else {
                        irr_checked += 1;
                        if n as i64 != rec.info.number_data {
                            irr_count_mismatch += 1;
                        }
                    }
                }
            }
            Err(e) => {
                *errs.entry(format!("{t}:{}", err_kind(&e))).or_default() += 1;
                if err_examples.len() < 3 {
                    err_examples.push(format!("{t}:{}", err_kind(&e)));
                }
            }
        }
    }
    let after = stat_of(path);
    let unchanged = before.is_some() && before == after;
    let okj: Vec<String> = ok.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    let errj: Vec<String> = errs
        .iter()
        .map(|(k, v)| format!("{}:{v}", json_str(k.as_bytes())))
        .collect();
    format!(
        "{{\"id\":{},\"fver\":{},\"ts_records\":{total},\"read\":{},\"ok\":{{{}}},\"errors\":{{{}}},\"values\":{values},\"reg_checked\":{reg_checked},\"reg_logical_mismatch\":{reg_logical_mismatch},\"irr_checked\":{irr_checked},\"irr_count_mismatch\":{irr_count_mismatch},\"unchanged\":{unchanged},\"size\":{},\"elapsed_s\":{:.3}}}",
        json_str(id.as_bytes()),
        f.header().fver,
        picks.len(),
        okj.join(","),
        errj.join(","),
        before.map(|b| b.0).unwrap_or(0),
        t0.elapsed().as_secs_f64()
    )
}

fn corpus(
    list: &str,
    out: &str,
    max: usize,
    threads: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(list)?;
    let jobs: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, '\t');
            Some((it.next()?.to_string(), it.next()?.trim_end().to_string()))
        })
        .collect();
    let next = AtomicUsize::new(0);
    let outf = Mutex::new(BufWriter::new(std::fs::File::create(out)?));
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::SeqCst);
                let Some((id, path)) = jobs.get(k) else { break };
                let line = corpus_one(id, path, max);
                let mut o = outf.lock().unwrap();
                let _ = writeln!(o, "{line}");
                let _ = o.flush();
            });
        }
    });
    Ok(())
}

fn timing(file: &str) -> Result<(), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let mut f = Dss7File::open(file)?;
    let paths = ts_paths(&mut f)?;
    let t_cat = t0.elapsed().as_secs_f64();
    let (mut n, mut vals, mut errs) = (0usize, 0usize, 0usize);
    for (p, t) in &paths {
        match read_by_path(&mut f, p, *t) {
            Ok(r) => {
                n += 1;
                vals += r.ts.times.len();
            }
            Err(_) => errs += 1,
        }
    }
    println!(
        "{{\"records\":{n},\"errors\":{errs},\"values\":{vals},\"catalog_s\":{t_cat:.3},\"total_s\":{:.3}}}",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Check the info block's first/last valid-value stamps (`kinfoFirstDate`,
/// `kinfoLastDate`) against the decoded values (P06 open item).
fn dates(file: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = Dss7File::open(file)?;
    let paths = ts_paths(&mut f)?;
    let (mut n, mut first_ok, mut last_ok, mut no_valid) = (0, 0, 0, 0);
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    for (p, t) in &paths {
        let Ok(Some(rec)) = f.lookup(p) else { continue };
        let Ok(r) = (if *t == 102 || *t == 107 {
            continue;
        } else {
            f.read_ts_record(&rec)
        }) else {
            continue;
        };
        if r.meta.pattern {
            continue;
        }
        let v = r.ts.values.to_f64();
        // P30: the writer finds first/last valid values with `zisMissing*`
        // (`Internal/ztsRegStoreBlock.c` line 313 -> `ztsFirstLastValidValues.c`
        // lines 136-345), so NaN is valid there.
        let valid: Vec<usize> = (0..v.len())
            .filter(|&i| !dss_core::is_hec_missing(v[i]))
            .collect();
        n += 1;
        let stamp = |d: (i32, i32)| i64::from(d.0) * 86_400 + i64::from(d.1);
        let (Some(&a), Some(&b)) = (valid.first(), valid.last()) else {
            no_valid += 1;
            let ih = &r.meta.internal_header;
            let sp = usize::try_from(ih.block_start_position).ok();
            let k = match sp.and_then(|i| r.ts.times.get(i)) {
                Some(&t) if t == stamp(rec.info.first_date) => "no_valid:first=first_stored",
                _ => "no_valid:first=other",
            };
            *kinds.entry(k.into()).or_default() += 1;
            continue;
        };
        let fs = stamp(rec.info.first_date);
        let ls = stamp(rec.info.last_date);
        if fs == r.ts.times[a] {
            first_ok += 1;
        } else {
            let sp = usize::try_from(r.meta.internal_header.block_start_position).ok();
            let k = if r.meta.interval.code > 0 && sp.and_then(|i| r.ts.times.get(i)) == Some(&fs) {
                "first=first_stored(missing)"
            } else if fs == r.ts.times[0] {
                "first=first_slot"
            } else if r.ts.times.contains(&fs) {
                "first=other_slot"
            } else {
                "first=not_a_slot"
            };
            *kinds.entry(k.into()).or_default() += 1;
        }
        if ls == r.ts.times[b] {
            last_ok += 1;
        } else {
            let ep = usize::try_from(r.meta.internal_header.block_end_position).ok();
            let k = if r.meta.interval.code > 0 && ep.and_then(|i| r.ts.times.get(i)) == Some(&ls) {
                "last=last_stored(missing)"
            } else if ls == *r.ts.times.last().unwrap() {
                "last=last_slot"
            } else if r.ts.times.contains(&ls) {
                "last=other_slot"
            } else {
                "last=not_a_slot"
            };
            *kinds.entry(k.into()).or_default() += 1;
        }
    }
    let kj: Vec<String> = kinds
        .iter()
        .map(|(k, v)| format!("{}:{v}", json_str(k.as_bytes())))
        .collect();
    println!("{{\"fver\":{},\"checked\":{n},\"first_ok\":{first_ok},\"last_ok\":{last_ok},\"no_valid\":{no_valid},\"other\":{{{}}}}}", f.header().fver, kj.join(","));
    Ok(())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let flag = |name: &str, d: usize| -> usize {
        a.iter()
            .position(|x| x == name)
            .and_then(|i| a.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let r = match a.get(1).map(String::as_str) {
        Some("records") if a.len() >= 4 => records(&a[2], &a[3]),
        Some("datasets") if a.len() >= 5 => datasets(&a[2], &a[3], &a[4]),
        Some("corpus") if a.len() >= 4 => {
            corpus(&a[2], &a[3], flag("--max", 0), flag("--threads", 4))
        }
        Some("time") if a.len() >= 3 => timing(&a[2]),
        Some("dates") if a.len() >= 3 => dates(&a[2]),
        _ => {
            eprintln!("usage: dss7_ts_dump records|datasets|corpus|time ... (see source)");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

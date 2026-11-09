//! DSS6 time-series decode over real corpus files (opt-in; the files are not part of this repository).
//!
//! Set `DSS6_TS_CORPUS_TSV` (lines `task_id<TAB>path`) and `DSS6_TS_CORPUS_OUT` (JSON
//! lines, counts only, no pathnames). Optional: `DSS6_TS_CORPUS_MAX` records per file
//! (default 5000, evenly sampled in catalog order), `DSS6_TS_CORPUS_THREADS` (default 4),
//! `DSS6_TS_CORPUS_DETAILS` (JSON lines with pathnames of errors and non-standard blocks;
//! keep it on local scratch storage). Without the variables the test is a no-op.
//!
//! ```text
//! cargo test --release -p dss-core --test dss6_ts_corpus -- --nocapture
//! ```
//!
//! Files are opened read-only; size and modification time are checked before and after.

use dss_core::dss6::{CatalogOptions, Dss6File, InfoCheck, TsError, TsLayout, TsReadOptions};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

fn stat(path: &str) -> Option<(u64, u128)> {
    let m = std::fs::metadata(path).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((m.len(), t))
}

fn kind(e: &TsError) -> &'static str {
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

fn run_file(task: &str, path: &str, max: usize, details: &Mutex<Option<std::fs::File>>) -> String {
    let t0 = Instant::now();
    let before = stat(path);
    let mut c: BTreeMap<String, u64> = BTreeMap::new();
    let mut add = |k: String, n: u64| *c.entry(k).or_insert(0) += n;
    let mut det = Vec::new();
    let (mut seen_offset, mut seen_tz) = (false, false);
    let res = (|| -> Result<(), String> {
        let file = Dss6File::open(path).map_err(|e| format!("open: {e:?}"))?;
        let opts = CatalogOptions {
            stop_at_knrecs: false,
            verify_hashes: false,
            verify_table_chains: false,
            check_duplicates: false,
            verify_info: InfoCheck::None,
        };
        let walk = file.walk_bins(&opts).map_err(|e| format!("walk: {e}"))?;
        let ts: Vec<_> = walk
            .live()
            .filter(|e| (100..120).contains(&e.record_type))
            .collect();
        add("live_ts".into(), ts.len() as u64);
        let step = if ts.len() > max {
            ts.len().div_ceil(max)
        } else {
            1
        };
        let ro = TsReadOptions::default();
        for e in ts.iter().step_by(step) {
            add("read".into(), 1);
            let r = file
                .read_info(e.info_address.into(), e.pathname.len() as i32)
                .map_err(TsError::from)
                .and_then(|info| file.decode_ts_info(&info, &ro).map(|r| (r, info)));
            match r {
                Ok((r, info)) => {
                    add(format!("ok:{}", r.record_type), 1);
                    add("values".into(), r.values.len() as u64);
                    let layout = match r.layout {
                        TsLayout::Compressed(s) => format!("compressed{s}"),
                        l => format!("{l:?}"),
                    };
                    add(format!("layout:{}:{layout}", r.record_type), 1);
                    // Count consistency with the info block / internal header.
                    let consistent = match r.layout {
                        TsLayout::RegularPattern => r.values.len() as i32 == info.data_words,
                        TsLayout::Regular | TsLayout::Compressed(_) => {
                            r.values.len() as i32 == info.logical_values
                        }
                        _ => r.internal_header.get(1).copied() == Some(r.values.len() as i32),
                    };
                    if !consistent {
                        add("count_inconsistent".into(), 1);
                    }
                    if r.time_offset_minutes != 0 {
                        if !seen_offset {
                            seen_offset = true;
                            det.push(format!(
                                "{{\"task\":\"{task}\",\"what\":\"offset\",\"p\":\"{}\",\"offset\":{}}}",
                                hex(&r.pathname),
                                r.time_offset_minutes
                            ));
                        }
                        add("offset_nonzero".into(), 1);
                    }
                    if matches!(r.time_zone_offset, Some(v) if v != 0 && v != -1) {
                        if !seen_tz {
                            seen_tz = true;
                            det.push(format!(
                                "{{\"task\":\"{task}\",\"what\":\"tz\",\"p\":\"{}\",\"tz\":{},\"name\":\"{}\"}}",
                                hex(&r.pathname),
                                r.time_zone_offset.unwrap_or(0),
                                r.time_zone_name.clone().unwrap_or_default().escape_default()
                            ));
                        }
                        add("tz_offset_set".into(), 1);
                    }
                    if r.time_zone_name.is_some() {
                        add("tz_name_set".into(), 1);
                    }
                    if r.quality.is_some() {
                        add(format!("quality:{}", r.record_type), 1);
                    }
                    if r.pseudo_interval_minutes.is_some() {
                        add("pseudo".into(), 1);
                    }
                    if !r.user_header.is_empty() {
                        add("user_header".into(), 1);
                    }
                    if r.coordinates.is_some() {
                        add("coordinates".into(), 1);
                    }
                    if let Some(cap) = r.irregular_capacity {
                        if info.data_words > (r.values.len() as i32) * 2 {
                            add("its_slack".into(), 1);
                        }
                        let _ = cap;
                    }
                    if r.nonstandard_block {
                        add(
                            format!("nonstandard_block:{}", r.interval_minutes.unwrap_or(0)),
                            1,
                        );
                        det.push(format!(
                            "{{\"task\":\"{task}\",\"what\":\"nonstandard\",\"p\":\"{}\",\"n\":{}}}",
                            hex(&r.pathname),
                            r.values.len()
                        ));
                    }
                    if r.values.is_empty() {
                        add("empty".into(), 1);
                    }
                }
                Err(err) => {
                    add(format!("error:{}:{}", e.record_type, kind(&err)), 1);
                    det.push(format!(
                        "{{\"task\":\"{task}\",\"what\":\"error\",\"p\":\"{}\",\"kind\":\"{}\",\"msg\":\"{}\"}}",
                        hex(&e.pathname),
                        kind(&err),
                        err.to_string().escape_default()
                    ));
                }
            }
        }
        Ok(())
    })();
    let after = stat(path);
    let unchanged = before.is_some() && before == after;
    if !det.is_empty() {
        if let Some(f) = details.lock().unwrap().as_mut() {
            for d in det {
                let _ = writeln!(f, "{d}");
            }
        }
    }
    let counts: Vec<String> = c.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!(
        "{{\"task\":\"{task}\",\"status\":\"{}\",\"unchanged\":{unchanged},\"bytes\":{},\"seconds\":{:.3},\"counts\":{{{}}}}}",
        match &res {
            Ok(()) => "ok".to_string(),
            Err(e) => e.escape_default().to_string(),
        },
        before.map_or(0, |b| b.0),
        t0.elapsed().as_secs_f64(),
        counts.join(",")
    )
}

#[test]
fn dss6_ts_corpus() {
    let (Ok(tsv), Ok(out)) = (
        std::env::var("DSS6_TS_CORPUS_TSV"),
        std::env::var("DSS6_TS_CORPUS_OUT"),
    ) else {
        eprintln!("DSS6_TS_CORPUS_TSV / DSS6_TS_CORPUS_OUT not set; skipping");
        return;
    };
    let max: usize = std::env::var("DSS6_TS_CORPUS_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let threads: usize = std::env::var("DSS6_TS_CORPUS_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .clamp(1, 4);
    let details = Mutex::new(
        std::env::var("DSS6_TS_CORPUS_DETAILS")
            .ok()
            .and_then(|p| std::fs::File::create(p).ok()),
    );
    let list: Vec<(String, String)> = std::fs::read_to_string(&tsv)
        .expect("read list")
        .lines()
        .filter_map(|l| {
            l.split_once('\t')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .collect();
    let out = Mutex::new(std::fs::File::create(&out).expect("create out"));
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((task, path)) = list.get(i) else {
                    break;
                };
                let line = run_file(task, path, max, &details);
                writeln!(out.lock().unwrap(), "{line}").unwrap();
            });
        }
    });
}

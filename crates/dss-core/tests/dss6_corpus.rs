//! DSS6 catalog run over real corpus files (opt-in; the files are not part of this repository).
//!
//! Set `DSS6_CORPUS_TSV` to a file of `task_id<TAB>path` lines and `DSS6_CORPUS_OUT` to
//! an output JSON-lines path. Without them the test is a no-op. Run in release:
//!
//! ```text
//! cargo test --release -p dss-core --test dss6_corpus -- --nocapture
//! ```
//!
//! Files are opened read-only. Size and modification time are recorded before and after
//! each file to prove the reader did not change it. At most `DSS6_CORPUS_THREADS`
//! (default 4) files are open at once. The output holds counts, anomaly kinds and word
//! addresses only, never record pathnames.

use dss_core::dss6::{CatalogOptions, Dss6Error, Dss6File, InfoCheck};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

fn error_kind(e: &Dss6Error) -> &'static str {
    match e {
        Dss6Error::Header(_) => "Header",
        Dss6Error::NotVersion6 { .. } => "NotVersion6",
        Dss6Error::ByteSwapped { .. } => "ByteSwapped",
        Dss6Error::BadTableType(_) => "BadTableType",
        Dss6Error::BinSizeOutOfRange(_) => "BinSizeOutOfRange",
        Dss6Error::BadHeader { .. } => "BadHeader",
        Dss6Error::Io(_) => "Io",
        Dss6Error::OutOfBounds { .. } => "OutOfBounds",
        Dss6Error::LockRecord { .. } => "LockRecord",
        Dss6Error::Corrupt { .. } => "Corrupt",
    }
}

fn run_one(task: &str, path: &str) -> String {
    let t0 = Instant::now();
    let before = stat(path);
    let mut j = String::new();
    let _ = write!(j, "{{\"task_id\":{task}");
    let result = Dss6File::open(path).and_then(|f| {
        let opts = CatalogOptions {
            verify_info: InfoCheck::Sample(3000),
            ..Default::default()
        };
        let walk = f.walk_bins(&opts)?;
        Ok((f, walk))
    });
    match result {
        Err(e) => {
            // Header errors carry no client data; I/O errors may name paths, so keep the kind.
            let msg = match &e {
                Dss6Error::Io(_) => "io error".to_string(),
                other => other.to_string().replace('"', "'"),
            };
            let _ = write!(
                j,
                ",\"ok\":false,\"error_kind\":\"{}\",\"error\":\"{msg}\"",
                error_kind(&e)
            );
        }
        Ok((f, walk)) => {
            let h = f.header();
            let mut st: BTreeMap<i32, usize> = BTreeMap::new();
            for e in &walk.entries {
                *st.entry(e.status).or_default() += 1;
            }
            let c = |codes: &[i32]| {
                codes
                    .iter()
                    .map(|s| st.get(s).copied().unwrap_or(0))
                    .sum::<usize>()
            };
            let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
            for a in &walk.anomalies {
                *kinds.entry(format!("{:?}", a.kind)).or_default() += 1;
            }
            let nonascii = walk
                .live()
                .filter(|e| e.pathname.iter().any(|&b| b >= 0x80))
                .count();
            let pseudo = walk.live().filter(|e| e.pseudo_interval > 0).count();
            let max_len = walk.live().map(|e| e.pathname.len()).max().unwrap_or(0);
            let _ = write!(
                j,
                ",\"ok\":true,\"version\":\"{}\",\"knrecs\":{},\"khash\":{},\"ktable\":{},\"kbnsiz\":{},\"size\":{},\"live\":{},\"status1\":{},\"status11\":{},\"deleted\":{},\"renamed\":{},\"rewritten_old\":{},\"entries\":{},\"other_status\":{},\"blocks\":{},\"bins\":{},\"overflow_bins\":{},\"info_checked\":{},\"nonascii_live\":{nonascii},\"pseudo_live\":{pseudo},\"max_path_len\":{max_len},\"eof_word\":{},\"warnings\":{}",
                h.version_string,
                h.number_records,
                h.max_hash,
                h.table_type,
                h.bin_size,
                f.size_bytes(),
                c(&[1, 11]),
                c(&[1]),
                c(&[11]),
                c(&[2, 12]),
                c(&[3, 13]),
                c(&[4, 14]),
                walk.entries.len(),
                walk.entries.len() - c(&[1, 11, 2, 12, 3, 13, 4, 14]),
                walk.blocks,
                walk.bins,
                walk.overflow.len(),
                walk.info_checked,
                f.eof_word().map_or("null".to_string(), |w| w.to_string()),
                f.warnings().len(),
            );
            j.push_str(",\"anomalies\":{");
            let parts: Vec<String> = kinds.iter().map(|(k, n)| format!("\"{k}\":{n}")).collect();
            j.push_str(&parts.join(","));
            j.push('}');
            // Anomaly kinds with word-free byte addresses only (details can hold pathnames).
            let addrs: Vec<String> = walk
                .anomalies
                .iter()
                .take(20)
                .map(|a| {
                    format!(
                        "[\"{:?}\",{}]",
                        a.kind,
                        a.address.map_or("null".into(), |x| x.to_string())
                    )
                })
                .collect();
            let _ = write!(j, ",\"anomaly_addrs\":[{}]", addrs.join(","));
            // Header-level warning texts carry no pathnames.
            let w: Vec<String> = f
                .warnings()
                .iter()
                .map(|w| format!("\"{}\"", w.detail.replace('"', "'")))
                .collect();
            let _ = write!(j, ",\"warning_text\":[{}]", w.join(","));
        }
    }
    let after = stat(path);
    let fmt_stat =
        |s: Option<(u64, u128)>| s.map_or("null".to_string(), |(a, b)| format!("[{a},{b}]"));
    let _ = write!(
        j,
        ",\"stat_before\":{},\"stat_after\":{},\"unchanged\":{},\"elapsed_s\":{:.3}}}",
        fmt_stat(before),
        fmt_stat(after),
        before.is_some() && before == after,
        t0.elapsed().as_secs_f64()
    );
    j
}

#[test]
fn dss6_corpus_catalog_run() {
    let (Ok(list), Ok(out)) = (
        std::env::var("DSS6_CORPUS_TSV"),
        std::env::var("DSS6_CORPUS_OUT"),
    ) else {
        eprintln!("DSS6_CORPUS_TSV / DSS6_CORPUS_OUT not set; skipping corpus run");
        return;
    };
    let threads: usize = std::env::var("DSS6_CORPUS_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .clamp(1, 4);
    let text = std::fs::read_to_string(list).expect("read corpus list");
    let tasks: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .collect();
    let next = AtomicUsize::new(0);
    let sink = Mutex::new(std::fs::File::create(&out).expect("create output"));
    let done = AtomicUsize::new(0);
    let started = SystemTime::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((task, path)) = tasks.get(i) else {
                    break;
                };
                let line = run_one(task, path);
                let mut f = sink.lock().unwrap();
                writeln!(f, "{line}").unwrap();
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                if n % 100 == 0 {
                    eprintln!(
                        "{n}/{} files, {:?}",
                        tasks.len(),
                        started.elapsed().unwrap_or_default()
                    );
                }
            });
        }
    });
    eprintln!(
        "catalogued {} files in {:?}",
        tasks.len(),
        started.elapsed().unwrap_or_default()
    );
}

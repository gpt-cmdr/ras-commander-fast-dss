//! DSS7 catalog harness for golden-fixture and corpus runs (packet P06).
//!
//! ```text
//! dss7_corpus stats <list.tsv> <out.jsonl> [--info N] [--threads T]
//!     list.tsv: "<id>\t<path>" per line. Writes one JSON object per file with header
//!     facts, walk counters and anomaly counts (no pathnames).
//! dss7_corpus dump <file.dss> <out.tsv> [--info]
//!     One TSV row per catalog entry (includes pathnames; keep output off the repo).
//! ```
//!
//! Files are opened read-only through `Dss7File::open`; nothing is written to them.

use dss_core::dss7::{CatalogOptions, ChainState, Dss7File, InfoCheck};
use dss_core::AnomalyKind;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn kind_name(k: AnomalyKind) -> &'static str {
    match k {
        AnomalyKind::HashMismatch => "hash_mismatch",
        AnomalyKind::OrphanBin => "orphan_bin",
        AnomalyKind::BadInfoBlock => "bad_info_block",
        AnomalyKind::DuplicatePathname => "duplicate_pathname",
        AnomalyKind::RecordCountMismatch => "record_count_mismatch",
        AnomalyKind::OutOfBounds => "out_of_bounds",
        AnomalyKind::CycleDetected => "cycle_detected",
        AnomalyKind::Other => "other",
    }
}

fn stats_one(id: &str, path: &str, info: Option<usize>) -> String {
    let t0 = Instant::now();
    let meta_before = std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    let mut f = match Dss7File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return format!(
                "{{\"id\":{},\"ok\":false,\"error\":{},\"elapsed_s\":{:.3}}}",
                json_str(id),
                json_str(&e.to_string()),
                t0.elapsed().as_secs_f64()
            )
        }
    };
    let opts = CatalogOptions {
        check_info: match info {
            None => InfoCheck::None,
            Some(0) => InfoCheck::Live,
            Some(n) => InfoCheck::LiveUpTo(n),
        },
        // DSS7_RECOVER=1 lists entries in bins outside the block chain.
        recover_unlinked: std::env::var("DSS7_RECOVER").is_ok_and(|v| v == "1"),
        ..CatalogOptions::default()
    };
    let h = f.header().clone();
    let res = f.catalog_with(&opts);
    let meta_after = std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    let unchanged = meta_before.is_some() && meta_before == meta_after;
    let cat = match res {
        Ok(c) => c,
        Err(e) => {
            return format!(
                "{{\"id\":{},\"ok\":false,\"error\":{},\"elapsed_s\":{:.3}}}",
                json_str(id),
                json_str(&e.to_string()),
                t0.elapsed().as_secs_f64()
            )
        }
    };
    let mut kinds: BTreeMap<&str, u64> = BTreeMap::new();
    for a in &cat.catalog.anomalies {
        *kinds.entry(kind_name(a.kind)).or_default() += 1;
    }
    let mut types: BTreeMap<i32, u64> = BTreeMap::new();
    for e in cat.catalog.live() {
        *types.entry(e.record_type).or_default() += 1;
    }
    let s = &cat.stats;
    let kinds_json: Vec<String> = kinds.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    let types_json: Vec<String> = types.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!(
        concat!(
            "{{\"id\":{},\"ok\":true,\"version\":{},\"fver\":{},\"header_size_zero\":{},\"knumberRecords\":{},",
            "\"kfileSize\":{},\"kmaxHash\":{},\"kbinSize\":{},\"hash_table\":{},\"first_bin\":{},\"old_locking\":{},",
            "\"blocks\":{},\"bins\":{},\"entries\":{},\"live\":{},\"undeleted\":{},\"aliases\":{},\"deleted\":{},",
            "\"renamed\":{},\"other_status\":{},\"hash_mismatch_live\":{},\"hash_mismatch_nonlive\":{},",
            "\"orphan_entries\":{},\"orphan_bins\":{},\"table_chain_miss\":{},\"duplicate_entries\":{},",
            "\"duplicate_pathnames\":{},\"info_checked\":{},\"info_bad\":{},\"info_zeroed\":{},\"dangling_links\":{},\"dangling_zeroed\":{},\"unlisted_bins\":{},\"unlisted_live\":{},\"recovered_entries\":{},\"recovered_live\":{},",
            "\"anomalies\":{{{}}},\"live_types\":{{{}}},\"unchanged\":{},\"elapsed_s\":{:.3}}}"
        ),
        json_str(id),
        json_str(&h.version_string),
        h.fver,
        h.header_size_was_zero,
        h.number_records,
        h.file_size_words,
        h.max_hash,
        h.bin_size,
        h.hash_table_address,
        h.first_bin_address,
        h.old_locking,
        s.blocks,
        s.bins,
        s.entries,
        s.live,
        s.undeleted,
        s.aliases,
        s.deleted,
        s.renamed,
        s.other_status,
        s.hash_mismatch_live,
        s.hash_mismatch_nonlive,
        s.orphan_entries,
        s.orphan_bins,
        s.table_chain_miss,
        s.duplicate_entries,
        s.duplicate_pathnames,
        s.info_checked,
        s.info_bad,
        s.info_zeroed,
        s.dangling_links,
        s.dangling_zeroed_bins,
        s.unlisted_bins,
        s.unlisted_live_entries,
        s.recovered_entries,
        s.recovered_live,
        kinds_json.join(","),
        types_json.join(","),
        unchanged,
        t0.elapsed().as_secs_f64()
    )
}

fn dump(path: &str, out: &str, info: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = Dss7File::open(path)?;
    let opts = CatalogOptions {
        check_info: if info {
            InfoCheck::Live
        } else {
            InfoCheck::None
        },
        recover_unlinked: std::env::var("DSS7_RECOVER").is_ok_and(|v| v == "1"),
        ..CatalogOptions::default()
    };
    let cat = f.catalog_with(&opts)?;
    for a in &cat.catalog.anomalies {
        if !matches!(
            a.kind,
            AnomalyKind::OrphanBin | AnomalyKind::DuplicatePathname | AnomalyKind::BadInfoBlock
        ) {
            eprintln!("anomaly {:?} at {:?}: {}", a.kind, a.address, a.detail);
        }
    }
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    writeln!(
        w,
        "idx\traw_status\ttype\tlast_write_ms\tjulian_first\tjulian_last\tchain\tdup_of\tinfo_addr_words\tinfo_problem\tinfo_type\tinfo_last_write_ms\tinfo_first\tinfo_last\trecovered\tpathname"
    )?;
    for (i, (e, m)) in cat.catalog.entries.iter().zip(&cat.meta).enumerate() {
        let chain = match m.chain {
            ChainState::NotChecked => "-".to_string(),
            ChainState::OwnChain(h) => format!("own{h}"),
            ChainState::OtherSlot => "other".into(),
            ChainState::Orphan => "orphan".into(),
        };
        let (ip, it, ilw, ifd, ild) = match m.info {
            None => (
                "-".to_string(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
            Some(s) => (
                s.problem
                    .map(|p| format!("{p:?}"))
                    .unwrap_or_else(|| "ok".into()),
                s.record_type.to_string(),
                s.last_write_ms.to_string(),
                format!("{},{}", s.first_date.0, s.first_date.1),
                format!("{},{}", s.last_date.0, s.last_date.1),
            ),
        };
        writeln!(
            w,
            "{i}\t{}\t{}\t{}\t{}\t{}\t{chain}\t{}\t{}\t{ip}\t{it}\t{ilw}\t{ifd}\t{ild}\t{}\t{}",
            m.raw_status,
            e.record_type,
            e.last_write_ms.unwrap_or(0),
            m.julian_first.map(|v| v.to_string()).unwrap_or_default(),
            m.julian_last.map(|v| v.to_string()).unwrap_or_default(),
            m.duplicate_of.map(|v| v.to_string()).unwrap_or_default(),
            m.info_address,
            u8::from(m.recovered),
            e.pathname_lossy().replace(['\t', '\n'], " ")
        )?;
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match args.get(1).map(String::as_str) {
        Some("stats") if args.len() >= 4 => {
            let list = std::fs::read_to_string(&args[2]).expect("read list");
            let items: Vec<(String, String)> = list
                .lines()
                .filter_map(|l| {
                    l.split_once('\t')
                        .map(|(a, b)| (a.to_string(), b.trim().to_string()))
                })
                .collect();
            let info = flag("--info").map(|v| v.parse::<usize>().expect("--info N"));
            let threads = flag("--threads")
                .map(|v| v.parse::<usize>().expect("--threads T"))
                .unwrap_or(1)
                .clamp(1, 4);
            let out = Mutex::new(std::io::BufWriter::new(
                std::fs::File::create(&args[3]).expect("create out"),
            ));
            let next = AtomicUsize::new(0);
            std::thread::scope(|sc| {
                for _ in 0..threads {
                    sc.spawn(|| loop {
                        let k = next.fetch_add(1, Ordering::SeqCst);
                        let Some((id, path)) = items.get(k) else {
                            break;
                        };
                        let line = stats_one(id, path, info);
                        let mut o = out.lock().unwrap();
                        writeln!(o, "{line}").unwrap();
                        o.flush().unwrap();
                        if k % 100 == 0 {
                            eprintln!("{k}/{}", items.len());
                        }
                    });
                }
            });
        }
        Some("dump") if args.len() >= 4 => {
            if let Err(e) = dump(&args[2], &args[3], args.iter().any(|a| a == "--info")) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: dss7_corpus stats <list.tsv> <out.jsonl> [--info N] [--threads T]");
            eprintln!("       dss7_corpus dump <file.dss> <out.tsv> [--info]");
            std::process::exit(2);
        }
    }
}

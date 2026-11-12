//! P66: DSS6 regular merge census over HEC-written DSS6 files you supply (to confirm
//! that the refusals that stay are rare, and that merges into real HEC-written blocks work).
//!
//! Set `P66_CENSUS_TSV` (lines `id<TAB>path`), `P66_CENSUS_WORK` (a scratch folder on a
//! local disk; every file is copied there and only the copy is written) and
//! `P66_CENSUS_OUT` (JSON lines, counts only, no pathnames). Optional: `P66_CENSUS_EQUAL`
//! (records per file probed with an equal rewrite, default 2000) and `P66_CENSUS_CHANGED`
//! (records per file merged for real, default 200), both evenly sampled in catalog order.
//! Without the variables the test is a no-op.
//!
//! Per file:
//! - stored features of every live regular time-series block (types 100/105, not
//!   patterns): compression scheme, quality, a time-zone offset word or name, a user
//!   header, an `ELEV` C part with a user header, a non-standard block length;
//! - a new-record probe (file-level refusals: layouts before 6-M, record-tag schemes);
//! - an equal rewrite of the first values of each sampled block (every refusal check of
//!   the merge runs; the outcome must be `Unchanged` or a typed refusal);
//! - a real merge into each of fewer sampled blocks (the first value changed), then a
//!   read-back: the written position holds the new value and every other position, the
//!   times and the quality are bit-identical to before;
//! - `check_file6` problem counts before and after.
//!
//! ```text
//! cargo test --release -p dss-core --test p66_merge_census -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use dss_core::dss6::write::{check_file6, write_rts6};
use dss_core::dss6::Dss6File;
use dss_core::dss7::write::{
    BlockOutcome, RtsInput, RtsValues, SyncMode, WriteError, WriteOptions,
};
use dss_core::TsValues;

fn category(e: &WriteError) -> String {
    let s = e.to_string();
    let has = |k: &str| s.to_ascii_lowercase().contains(&k.to_ascii_lowercase());
    match e {
        WriteError::Unsupported(_) => {
            if has("6-M") || has("older") || has("before 6-") {
                "refused_old_layout".into()
            } else if has("tag") {
                "refused_tag_scheme".into()
            } else if has("elevation") || has("ELEV") {
                "refused_elev_user_header".into()
            } else if has("standard block") {
                "refused_nonstandard_block".into()
            } else if has("511") {
                "refused_quality_width_511".into()
            } else if has("type") {
                "refused_record_type".into()
            } else if has("table") || has("STABLE") {
                "refused_table_type".into()
            } else {
                format!("refused_other: {}", s.chars().take(60).collect::<String>())
            }
        }
        WriteError::Corrupt(_) => "corrupt".into(),
        WriteError::Locked(_) | WriteError::Lock(_) => "locked".into(),
        WriteError::BadInput(_) => format!("bad_input: {}", s.chars().take(60).collect::<String>()),
        WriteError::BadPathname(_) if has("printable") => "refused_pathname_bytes".into(),
        _ => format!("error: {}", s.chars().take(60).collect::<String>()),
    }
}

fn remove_copy(copy: &Path) {
    if let Ok(m) = std::fs::metadata(copy) {
        let mut perm = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(copy, perm);
        let _ = std::fs::remove_file(copy);
    }
}

/// An error message without anything that could name a file.
fn kind_of(s: &str) -> String {
    s.chars()
        .take(50)
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .collect()
}

fn opts() -> WriteOptions {
    WriteOptions {
        sync: SyncMode::None,
        ..WriteOptions::default()
    }
}

fn bits(v: &TsValues) -> Vec<u64> {
    match v {
        TsValues::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
        TsValues::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
    }
}

fn sample(n: usize, k: usize) -> Vec<usize> {
    if n <= k {
        return (0..n).collect();
    }
    (0..k).map(|i| i * n / k).collect()
}

#[derive(Default)]
struct Tally(BTreeMap<String, u64>);

impl Tally {
    fn add(&mut self, k: impl Into<String>) {
        *self.0.entry(k.into()).or_default() += 1;
    }
    fn json(&self) -> String {
        let items: Vec<String> = self
            .0
            .iter()
            .map(|(k, v)| format!("\"{}\":{v}", k.replace('"', "'")))
            .collect();
        format!("{{{}}}", items.join(","))
    }
}

fn probe_file(id: &str, src: &Path, work: &Path, n_equal: usize, n_changed: usize) -> String {
    let copy = work.join(format!("{id}.dss"));
    let before_len = std::fs::metadata(src).unwrap().len();
    remove_copy(&copy);
    std::fs::copy(src, &copy).unwrap();
    // The source files may be read-only; the copy must be writable.
    let mut perm = std::fs::metadata(&copy).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(&copy, perm).unwrap();
    let mut feat = Tally::default();
    let mut equal = Tally::default();
    let mut changed = Tally::default();
    let f = match Dss6File::open(&copy) {
        Ok(f) => f,
        Err(e) => {
            remove_copy(&copy);
            return format!(
                "{{\"id\":\"{id}\",\"open_error\":\"{}\"}}",
                kind_of(&e.to_string())
            );
        }
    };
    let version = f.header().version_string.clone();
    let cat = match f.catalog() {
        Ok(c) => c,
        Err(e) => {
            drop(f);
            remove_copy(&copy);
            return format!(
                "{{\"id\":\"{id}\",\"catalog_error\":\"{}\"}}",
                kind_of(&e.to_string())
            );
        }
    };
    let mut regular = Vec::new();
    for e in cat.live() {
        if !matches!(e.record_type, 100 | 105) {
            continue;
        }
        let parts: Vec<&[u8]> = e.pathname.split(|&b| b == b'/').collect();
        if parts.len() == 8 && parts[4].to_ascii_uppercase().starts_with(b"TS-") {
            feat.add("pattern_block");
            continue;
        }
        let r = match f.read_ts_record(&e.pathname) {
            Ok(r) => r,
            Err(_) => {
                feat.add("read_error");
                continue;
            }
        };
        feat.add("regular_block");
        if r.compression > 0 {
            feat.add(format!("compression_{}", r.compression));
        }
        if r.quality.is_some() {
            feat.add("quality");
        }
        if r.record_type == 105 {
            feat.add("double");
        }
        if matches!(r.time_zone_offset, Some(x) if x != -1 && x != 0) {
            feat.add("tz_offset_word_set");
        }
        if r.time_zone_name.is_some() {
            feat.add("tz_name");
        }
        if !r.user_header.is_empty() {
            feat.add("user_header");
            if parts.len() == 8 && parts[3].eq_ignore_ascii_case(b"ELEV") {
                feat.add("elev_user_header");
            }
        }
        if r.nonstandard_block {
            feat.add("nonstandard_block");
        }
        regular.push(e.pathname.clone());
    }
    drop(f);
    let problems_before = check_file6(&copy)
        .map(|c| c.problems.len())
        .unwrap_or(usize::MAX);
    // File-level probe: a new record.
    let pv = [1.0f32];
    let probe = RtsInput::new(
        b"/P66CENSUS/PROBE/FLOW//1HOUR/NEW/",
        (43_831 * 1440 + 60) * 60,
        RtsValues::F32(&pv),
        b"CFS",
        b"INST-VAL",
    );
    let new_record = match write_rts6(&copy, &probe, &opts()) {
        Ok(_) => "written".to_string(),
        Err(e) => category(&e),
    };
    // Equal rewrites.
    for i in sample(regular.len(), n_equal) {
        let p = &regular[i];
        let f = Dss6File::open(&copy).unwrap();
        let r = f.read_ts_record(p).unwrap();
        drop(f);
        let k = r.times.len().min(3);
        if k == 0 {
            equal.add("empty");
            continue;
        }
        let mut input = match &r.values {
            TsValues::F32(v) => RtsInput::new(p, r.times[0], RtsValues::F32(&v[..k]), b"", b""),
            TsValues::F64(v) => RtsInput::new(p, r.times[0], RtsValues::F64(&v[..k]), b"", b""),
        };
        let units = r.units.clone().into_bytes();
        let dtype = r.data_type.clone().into_bytes();
        input.units = &units;
        input.data_type = &dtype;
        let q = r.quality.as_ref().map(|q| q[..k].to_vec());
        input.quality = q.as_deref();
        match write_rts6(&copy, &input, &opts()) {
            Ok(rep) => {
                let o = rep.blocks.iter().map(|b| b.outcome).collect::<Vec<_>>();
                if o == [BlockOutcome::Unchanged] {
                    equal.add("unchanged");
                } else if r.compression > 0 {
                    // A compressed block is rewritten uncompressed;
                    // every decoded value, the times and the quality must be unchanged.
                    let f = Dss6File::open(&copy).unwrap();
                    let a = f.read_ts_record(p).unwrap();
                    let ok = bits(&a.values) == bits(&r.values)
                        && a.times == r.times
                        && a.quality == r.quality
                        && a.compression == 0;
                    equal.add(if ok {
                        "rewritten_uncompressed_verified"
                    } else {
                        "rewritten_uncompressed_mismatch"
                    });
                } else {
                    // What the rewrite changed (field names only).
                    let f = Dss6File::open(&copy).unwrap();
                    let a = f.read_ts_record(p).unwrap();
                    let mut d = Vec::new();
                    if bits(&a.values) != bits(&r.values) {
                        d.push("values".to_string());
                    }
                    for (i, (x, y)) in a.internal_header.iter().zip(&r.internal_header).enumerate()
                    {
                        if x != y {
                            d.push(format!("ihead{}", i + 1));
                        }
                    }
                    if a.internal_header.len() != r.internal_header.len() {
                        d.push("ihead_len".into());
                    }
                    for (k, same) in [
                        ("precision", a.precision == r.precision),
                        ("quality", a.quality == r.quality),
                        ("user_header", a.user_header == r.user_header),
                        ("type", a.record_type == r.record_type),
                    ] {
                        if !same {
                            d.push(k.into());
                        }
                    }
                    if d.iter().any(|x| x == "ihead2") {
                        // Units words as stored and as rewritten (bytes only).
                        let w = |h: &[i32]| {
                            h.get(1..5)
                                .unwrap_or(&[])
                                .iter()
                                .flat_map(|x| x.to_le_bytes())
                                .map(|c| format!("{c:02x}"))
                                .collect::<String>()
                        };
                        equal.add(format!(
                            "units_words {} -> {}",
                            w(&r.internal_header),
                            w(&a.internal_header)
                        ));
                    }
                    equal.add(format!("replaced: {}", d.join("+")));
                }
            }
            Err(e) => equal.add(category(&e)),
        }
    }
    // Real merges, verified by reading back, on a fresh copy of the source: the equal
    // rewrites above have already stored compressed blocks uncompressed, and a real merge
    // must start from the block as HEC stored it.
    remove_copy(&copy);
    std::fs::copy(src, &copy).unwrap();
    let mut perm = std::fs::metadata(&copy).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(&copy, perm).unwrap();
    for i in sample(regular.len(), n_changed) {
        let p = &regular[i];
        let f = Dss6File::open(&copy).unwrap();
        let before = f.read_ts_record(p).unwrap();
        drop(f);
        if before.times.is_empty() {
            continue;
        }
        let (vals32, vals64);
        let new_bits;
        let mut input = match &before.values {
            TsValues::F32(v) => {
                let x = if v[0] == -901.0 { 1.0 } else { v[0] + 1.0 };
                vals32 = [x];
                new_bits = u64::from(x.to_bits());
                RtsInput::new(p, before.times[0], RtsValues::F32(&vals32), b"", b"")
            }
            TsValues::F64(v) => {
                let x = if v[0] == -901.0 { 1.0 } else { v[0] + 1.0 };
                vals64 = [x];
                new_bits = x.to_bits();
                RtsInput::new(p, before.times[0], RtsValues::F64(&vals64), b"", b"")
            }
        };
        let units = before.units.clone().into_bytes();
        let dtype = before.data_type.clone().into_bytes();
        input.units = &units;
        input.data_type = &dtype;
        let q = before.quality.as_ref().map(|q| vec![q[0]]);
        input.quality = q.as_deref();
        match write_rts6(&copy, &input, &opts()) {
            Ok(_) => {
                let f = Dss6File::open(&copy).unwrap();
                let after = f.read_ts_record(p).unwrap();
                let (b, a) = (bits(&before.values), bits(&after.values));
                let ok = after.times == before.times
                    && a.len() == b.len()
                    && a[0] == new_bits
                    && a[1..] == b[1..]
                    && after.quality == before.quality
                    && after.record_type == before.record_type
                    && after.units == before.units
                    && after.data_type == before.data_type
                    && after.time_offset_minutes == before.time_offset_minutes
                    && after.compression == 0;
                changed.add(if ok {
                    "merged_verified"
                } else {
                    "merged_mismatch"
                });
                if before.compression > 0 {
                    changed.add(if ok {
                        "merged_verified_from_compressed"
                    } else {
                        "merged_mismatch_from_compressed"
                    });
                }
            }
            Err(e) => changed.add(category(&e)),
        }
    }
    let problems_after = check_file6(&copy)
        .map(|c| c.problems.len())
        .unwrap_or(usize::MAX);
    assert_eq!(
        std::fs::metadata(src).unwrap().len(),
        before_len,
        "{id}: source changed"
    );
    remove_copy(&copy);
    format!(
        "{{\"id\":\"{id}\",\"version\":\"{version}\",\"records\":{},\"features\":{},\"new_record\":\"{new_record}\",\"equal\":{},\"changed\":{},\"check_problems_before\":{problems_before},\"check_problems_after\":{problems_after}}}",
        cat.live().count(),
        feat.json(),
        equal.json(),
        changed.json()
    )
}

#[test]
fn p66_merge_census() {
    let (Ok(tsv), Ok(work), Ok(out)) = (
        std::env::var("P66_CENSUS_TSV"),
        std::env::var("P66_CENSUS_WORK"),
        std::env::var("P66_CENSUS_OUT"),
    ) else {
        return;
    };
    let n_equal = std::env::var("P66_CENSUS_EQUAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let n_changed = std::env::var("P66_CENSUS_CHANGED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let work = PathBuf::from(work);
    std::fs::create_dir_all(&work).unwrap();
    let mut w = std::fs::File::create(out).unwrap();
    for line in std::fs::read_to_string(tsv).unwrap().lines() {
        let Some((id, path)) = line.split_once('\t') else {
            continue;
        };
        let t = std::time::Instant::now();
        let j = probe_file(id, Path::new(path), &work, n_equal, n_changed);
        writeln!(w, "{j}").unwrap();
        eprintln!("{id}: {:.1} s", t.elapsed().as_secs_f64());
    }
}

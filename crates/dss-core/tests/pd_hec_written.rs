//! P23: HEC-written paired-data fixtures (`fixtures/public/p23_hec_pd7.dss`, heclib
//! `zpdStore`; `p23_hec_pd6.dss`, Monolith 3.3.27 `zspdi6`/`zpdStore6`) read natively, and
//! the native writer replaying the same stores (`p23_hec_pd{7,6}_spec.tsv`) from an empty
//! native file: every record's areas word-identical to HEC's, every info field equal except
//! the documented differences (equal rewrites skipped, type change by
//! relocation instead of delete-and-add, HEC's stale `KILNDA` after an in-place rewrite).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dss_core::dss6::write::{create_dss6, write_pd6};
use dss_core::dss6::{word_byte_offset, Dss6File};
use dss_core::dss7::write::{create_dss7, write_pd, PdArrays, PdInput, WriteOptions};
use dss_core::dss7::Dss7File;
use dss_core::{PairedData, TsValues};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

/// The last spec line per pathname (upper case) and all lines in order.
fn spec(version: u8) -> (BTreeMap<String, Vec<String>>, Vec<Vec<String>>) {
    let text = std::fs::read_to_string(fixture(&format!("p23_hec_pd{version}_spec.tsv"))).unwrap();
    let rows: Vec<Vec<String>> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    let mut last = BTreeMap::new();
    for r in &rows {
        assert_eq!(r.len(), 14);
        last.insert(r[1].to_ascii_uppercase(), r.clone());
    }
    (last, rows)
}

fn hex(s: &str) -> Vec<u64> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|t| u64::from_str_radix(t, 16).unwrap())
        .collect()
}

fn bits(v: &TsValues) -> Vec<u64> {
    match v {
        TsValues::F32(x) => x.iter().map(|y| u64::from(y.to_bits())).collect(),
        TsValues::F64(x) => x.iter().map(|y| y.to_bits()).collect(),
    }
}

fn s(r: &[String], i: usize) -> &str {
    if r[i] == "~" {
        ""
    } else {
        &r[i]
    }
}

fn check_against_spec(pd: &PairedData, r: &[String], version: u8) {
    let what = String::from_utf8_lossy(&pd.pathname).into_owned();
    assert_eq!(pd.record_type == 205, r[0] == "double", "{what}");
    assert_eq!(bits(&pd.ordinates), hex(&r[12]), "{what}");
    assert_eq!(bits(&pd.values), hex(&r[13]), "{what}");
    assert_eq!(
        [
            pd.units_independent.as_str(),
            pd.type_independent.as_str(),
            pd.units_dependent.as_str(),
            pd.type_dependent.as_str()
        ],
        [s(r, 2), s(r, 3), s(r, 4), s(r, 5)],
        "{what}"
    );
    let want: Vec<String> = if r[6] == "~" {
        Vec::new()
    } else {
        r[6].split('|').map(str::to_string).collect()
    };
    assert_eq!(pd.labels, want, "{what}");
    assert_eq!(
        (pd.x_precision, pd.y_precision),
        (r[7].parse().unwrap(), r[8].parse().unwrap()),
        "{what}"
    );
    assert_eq!(pd.independent_is_x_axis, r[9].parse::<i32>().unwrap());
    if version == 7 {
        assert_eq!(
            pd.time_zone.as_deref(),
            (r[10] != "~").then_some(r[10].as_str())
        );
    }
}

fn replay(path: &Path, rows: &[Vec<String>], version: u8) {
    let opts = WriteOptions {
        sync: false,
        ..WriteOptions::default()
    };
    for r in rows {
        let (o, v) = (hex(&r[12]), hex(&r[13]));
        let fo: Vec<f32> = o.iter().map(|&b| f32::from_bits(b as u32)).collect();
        let fv: Vec<f32> = v.iter().map(|&b| f32::from_bits(b as u32)).collect();
        let dor: Vec<f64> = o.iter().map(|&b| f64::from_bits(b)).collect();
        let dv: Vec<f64> = v.iter().map(|&b| f64::from_bits(b)).collect();
        let data = if r[0] == "double" {
            PdArrays::F64 {
                ordinates: &dor,
                values: &dv,
            }
        } else {
            PdArrays::F32 {
                ordinates: &fo,
                values: &fv,
            }
        };
        let labels: Option<Vec<Vec<u8>>> =
            (r[6] != "~").then(|| r[6].split('|').map(|x| x.as_bytes().to_vec()).collect());
        let mut input = PdInput::new(
            r[1].as_bytes(),
            data,
            r[11].parse().unwrap(),
            s(r, 2).as_bytes(),
            s(r, 3).as_bytes(),
            s(r, 4).as_bytes(),
            s(r, 5).as_bytes(),
        );
        input.labels = labels.as_deref();
        input.x_precision = r[7].parse().unwrap();
        input.y_precision = r[8].parse().unwrap();
        input.independent_is_x_axis = r[9].parse().unwrap();
        input.time_zone = (r[10] != "~").then(|| r[10].as_bytes());
        if version == 6 {
            write_pd6(path, &input, &opts).unwrap();
        } else {
            write_pd(path, &input, &opts).unwrap();
        }
    }
}

fn ints7(bytes: &[u8], a: &dss_core::dss7::info::Area) -> Vec<i32> {
    let start = a.address as usize * 8;
    (0..a.number_ints as usize)
        .map(|i| i32::from_le_bytes(bytes[start + 4 * i..start + 4 * i + 4].try_into().unwrap()))
        .collect()
}

#[test]
fn dss7_fixture_reads_and_native_replay_matches() {
    let (last, rows) = spec(7);
    let fx = fixture("p23_hec_pd7.dss");
    let mut f = Dss7File::open(&fx).unwrap();
    let live: Vec<_> = f.catalog().unwrap().catalog.live().cloned().collect();
    assert_eq!(live.len(), last.len());
    for e in &live {
        let pd = f.read_pd(&e.pathname).unwrap().pd;
        check_against_spec(
            &pd,
            &last[String::from_utf8_lossy(&e.pathname)
                .to_ascii_uppercase()
                .as_str()],
            7,
        );
    }
    // Native replay from an empty native file.
    let d = std::env::temp_dir().join(format!("pd_hec7_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let nat = d.join("nat.dss");
    create_dss7(&nat).unwrap();
    replay(&nat, &rows, 7);
    let (hb, nb) = (std::fs::read(&fx).unwrap(), std::fs::read(&nat).unwrap());
    let mut g = Dss7File::open(&nat).unwrap();
    let mut identical = 0;
    for e in &live {
        let h = f.lookup(&e.pathname).unwrap().unwrap();
        let n = g.lookup(&e.pathname).unwrap().unwrap();
        let (hi, ni) = (&h.info, &n.info);
        for (a, b) in [
            (&hi.internal_header, &ni.internal_header),
            (&hi.header2, &ni.header2),
            (&hi.user_header, &ni.user_header),
            (&hi.values1, &ni.values1),
            (&hi.values2, &ni.values2),
            (&hi.values3, &ni.values3),
        ] {
            assert_eq!(ints7(&hb, a), ints7(&nb, b), "{:?}", e.pathname);
        }
        assert_eq!(
            (hi.number_data, hi.logical_number, hi.allocated_size),
            (ni.number_data, ni.logical_number, ni.allocated_size)
        );
        let p = String::from_utf8_lossy(&e.pathname).to_ascii_uppercase();
        // HEC deletes and re-adds on a PD -> PDD change (version 1, no
        // expansion); the writer relocates (version 2, one expansion). HEC rewrites an
        // equal record (version 2); the writer skips it.
        if p.contains("/FREQ/") {
            assert_eq!((hi.version, hi.number_expansions), (1, 0));
            assert_eq!((ni.version, ni.number_expansions), (2, 1));
            assert_eq!(hi.record_type, ni.record_type);
        } else if p.contains("/LONG/") {
            assert_eq!((hi.version, ni.version), (2, 1));
        } else {
            assert_eq!(
                (
                    hi.record_type,
                    hi.version,
                    hi.number_expansions,
                    &hi.first_date
                ),
                (
                    ni.record_type,
                    ni.version,
                    ni.number_expansions,
                    &ni.first_date
                ),
                "{p}"
            );
            identical += 1;
        }
    }
    assert_eq!(identical, live.len() - 2);
    // The file-header area statistics after the same stores equal heclib's
    // (`zwriteInternal()` lines 265-320, `zwriteNew()`, `zwriteExisting()`):
    // kmaxInternalHeader..kmaxValues3Size (58-63), knumberInternalHeader..knumberDataArea2
    // (64-69), kmaxRecordSize (70), kmaxPdSize (75), kmaxPddSize (76, never set by heclib).
    let hw = |b: &[u8], i: usize| i64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
    for i in (58..=70).chain([75, 76]) {
        assert_eq!(hw(&hb, i), hw(&nb, i), "header word {i}");
    }
    assert!(hw(&nb, 62) > 0 && hw(&nb, 69) > 0 && hw(&nb, 75) > 0);
    let _ = std::fs::remove_dir_all(&d);
}

fn ints6(bytes: &[u8], addr: i32, n: i32) -> Vec<i32> {
    (0..n as i64)
        .map(|k| {
            let o = word_byte_offset(i64::from(addr) + k).unwrap() as usize;
            i32::from_le_bytes(bytes[o..o + 4].try_into().unwrap())
        })
        .collect()
}

#[test]
fn dss6_fixture_reads_and_native_replay_matches() {
    let (last, rows) = spec(6);
    let fx = fixture("p23_hec_pd6.dss");
    let f = Dss6File::open(&fx).unwrap();
    let entries = f.catalog().unwrap().entries;
    assert_eq!(entries.len(), last.len());
    for e in &entries {
        let pd = f.read_pd(&e.pathname).unwrap().pd;
        check_against_spec(
            &pd,
            &last[String::from_utf8_lossy(&e.pathname)
                .to_ascii_uppercase()
                .as_str()],
            6,
        );
    }
    let d = std::env::temp_dir().join(format!("pd_hec6_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let nat = d.join("nat.dss");
    create_dss6(&nat).unwrap();
    replay(&nat, &rows, 6);
    let g = Dss6File::open(&nat).unwrap();
    let (hb, nb) = (std::fs::read(&fx).unwrap(), std::fs::read(&nat).unwrap());
    for e in &entries {
        let h = f.lookup(&e.pathname).unwrap().unwrap().info;
        let n = g.lookup(&e.pathname).unwrap().unwrap().info;
        let p = String::from_utf8_lossy(&e.pathname).into_owned();
        assert_eq!(
            ints6(&hb, h.internal_header.0, h.internal_header.1),
            ints6(&nb, n.internal_header.0, n.internal_header.1),
            "{p}"
        );
        assert_eq!(
            ints6(&hb, h.data_address, h.data_words),
            ints6(&nb, n.data_address, n.data_words),
            "{p}"
        );
        assert_eq!(
            (
                h.record_type,
                h.compression,
                h.quality,
                h.precision,
                h.user_header.1
            ),
            (
                n.record_type,
                n.compression,
                n.quality,
                n.precision,
                n.user_header.1
            ),
            "{p}"
        );
        if p.contains("/THREE/") {
            // HEC's in-place rewrite leaves KILNDA at the old length (zowrit6.f lines
            // 155-160); the writer stores the current one.
            assert_eq!((h.logical_values, n.logical_values), (20, 12));
        } else {
            assert_eq!(h.logical_values, n.logical_values, "{p}");
        }
        if p.contains("/LONG/") {
            assert_eq!((h.record_version, n.record_version), (2, 1));
        } else {
            assert_eq!(h.record_version, n.record_version, "{p}");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

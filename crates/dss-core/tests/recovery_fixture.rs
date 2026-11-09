//! Synthetic DSS7 recovery fixture. No HEC software or external
//! data is used. It reproduces, in miniature, the interrupted-write damage found in
//! private HEC-written test files:
//!
//! - an ordinary record (reachable by pathname lookup);
//! - an orphan entry: its bin is in the bin-block chain but no hash-table slot reaches
//!   it (as in a private file with orphan entries; `lookup` returns `None`, `record_at` decodes it);
//! - a zeroed info block (as in a private file with zeroed info blocks; every route must refuse it);
//! - a duplicated pathname whose hash-chain copy has a zeroed info block and whose
//!   orphan copy is intact;
//! - a paired-data record (unsupported by the time-series route);
//! - an entry in a bin that the hash table reaches but the bin-block chain does not
//!   (unlinked bin, listed only in recovery mode).
//!
//! To regenerate the committed fixture, set `DSS_PY_FIXTURE_OUT` to a scratch
//! directory, run this test, then copy `synthetic_recovery7.dss` into
//! `fixtures/public/` and update its SHA-256 in `fixtures/public/README.md`.

mod dss7_common;

use dss7_common::{build, set_word, Rec, Spec};
use dss_core::dss7::{
    info::key, CatalogOptions, ChainState, Dss7Error, Dss7File, InfoProblem, TsError,
};
use dss_core::TsValues;

const REACHABLE: &str = "/RECOVERY/REACHABLE/FLOW/TS-PATTERN/1HOUR/SYN/";
const ORPHAN: &str = "/RECOVERY/ORPHAN/FLOW/TS-PATTERN/1HOUR/SYN/";
const ZEROED: &str = "/RECOVERY/ZEROED/FLOW/TS-PATTERN/1HOUR/SYN/";
const DUP: &str = "/RECOVERY/DUP/FLOW/TS-PATTERN/1HOUR/SYN/";
const CURVE: &str = "/RECOVERY/CURVE/STAGE-FLOW///SYN/";
const UNLINKED: &str = "/RECOVERY/UNLINKED/FLOW/TS-PATTERN/1HOUR/SYN/";

/// Spec order (index = record number in `Built::info_addr`): REACHABLE, ORPHAN, ZEROED,
/// DUP (hash-chain copy), dup (orphan copy), CURVE, UNLINKED.
const R_REACHABLE: usize = 0;
const R_ORPHAN: usize = 1;
const R_DUP_ORPHAN: usize = 4;
const R_UNLINKED: usize = 6;

/// Values stored for each decodable record (distinct so a test can tell copies apart).
fn values_for(rec: usize) -> Option<[f32; 3]> {
    match rec {
        R_REACHABLE => Some([1.0, 2.0, 3.0]),
        R_ORPHAN => Some([10.0, -901.0, 30.0]),
        R_DUP_ORPHAN => Some([40.0, 41.0, 42.0]),
        R_UNLINKED => Some([60.0, 61.0, 62.0]),
        _ => None,
    }
}

fn spec() -> Spec {
    let dup_orphan = DUP.to_ascii_lowercase();
    Spec {
        // One bin per block, so cutting one block link hides exactly the last bin.
        bins_per_block: 1,
        recs: vec![
            Rec::live(REACHABLE, 101),
            Rec::live(ORPHAN, 101).orphan(),
            Rec::live(ZEROED, 101).zero_info(),
            Rec::live(DUP, 101).zero_info(),
            Rec::live(&dup_orphan, 101).orphan(),
            Rec::live(CURVE, 200),
            Rec::live(UNLINKED, 101),
        ],
        ..Spec::default()
    }
}

fn synthetic_recovery7() -> Vec<u8> {
    let spec = spec();
    let built = build(&spec);
    let mut bytes = built.bytes;
    // Payloads, as in python_regression_fixtures.rs: heclib_c/src/Internal/
    // ztsRetrievePattern.c:ztsRetrievePattern reads raw values1 floats sized by
    // numberData (no compression, no quality); info addresses count int64 words and
    // area lengths count int32 words (Internal/zwriteNew.c:zwriteNew).
    let mut data_addr = bytes.len() / 8 - 1; // overwrite the old EOF flag
    bytes.truncate(data_addr * 8);
    for (rec, info) in built.info_addr.iter().enumerate() {
        let Some(vals) = values_for(rec) else {
            continue;
        };
        let info = *info as usize;
        for v in vals {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        set_word(&mut bytes, info + key::VALUES1_ADDRESS, data_addr as i64);
        set_word(&mut bytes, info + key::VALUES1_NUMBER, vals.len() as i64);
        set_word(&mut bytes, info + key::NUMBER_DATA, vals.len() as i64);
        set_word(&mut bytes, info + key::ALLOCATED_SIZE, 2);
        data_addr = bytes.len() / 8;
    }
    let eof = bytes.len() / 8;
    bytes.extend_from_slice(&(-97532i64).to_le_bytes());
    set_word(&mut bytes, 5, eof as i64);

    // Unlink the last bin block (dss7.md §3.5; zcatalogInternal() follows the block
    // links, zcheck() follows the hash table). The unlinked record must sit alone in
    // that last bin for the cut to hide exactly it.
    let h = Dss7File::from_bytes(&bytes).unwrap().header().clone();
    let unlinked_bin = built.entry_addr[R_UNLINKED];
    let prev_block = unlinked_bin - (h.bin_size * h.bins_per_block + 1);
    assert_eq!(
        built
            .entry_addr
            .iter()
            .filter(|&&e| e >= unlinked_bin && e < unlinked_bin + h.bin_size)
            .count(),
        1,
        "the unlinked record must be alone in the last bin"
    );
    set_word(
        &mut bytes,
        (prev_block + h.bin_size * h.bins_per_block) as usize,
        0,
    );
    bytes
}

fn index_of(cat: &dss_core::dss7::Dss7Catalog, path: &str) -> Vec<usize> {
    (0..cat.catalog.entries.len())
        .filter(|&i| cat.catalog.entries[i].pathname == path.as_bytes())
        .collect()
}

#[test]
fn synthetic_recovery_fixture() {
    let bytes = synthetic_recovery7();
    let mut f = Dss7File::from_bytes(&bytes).unwrap();

    // Default (HEC-parity) catalog: the unlinked record is not listed.
    let cat = f.catalog().unwrap();
    assert!(index_of(&cat, UNLINKED).is_empty());
    assert_eq!(cat.catalog.entries.len(), 6);

    // Ordinary record: lookup and record_at agree and decode.
    let i = index_of(&cat, REACHABLE)[0];
    assert!(matches!(cat.meta[i].chain, ChainState::OwnChain(_)));
    let by_path = f.lookup(REACHABLE.as_bytes()).unwrap().unwrap();
    let by_index = f.record_at(&cat, i).unwrap();
    assert_eq!(by_path.bin_entry_address, by_index.bin_entry_address);
    let ts = f.read_ts_record(&by_index).unwrap();
    assert_eq!(ts.ts.values, TsValues::F32(vec![1.0, 2.0, 3.0]));

    // Orphan: lookup cannot reach it; record_at decodes it (orphan form).
    let i = index_of(&cat, ORPHAN)[0];
    assert_eq!(cat.meta[i].chain, ChainState::Orphan);
    assert!(f.lookup(ORPHAN.as_bytes()).unwrap().is_none());
    let rec = f.record_at(&cat, i).unwrap();
    let ts = f.read_ts_record(&rec).unwrap();
    assert_eq!(ts.ts.values, TsValues::F32(vec![10.0, -901.0, 30.0]));

    // Zeroed info: refused by lookup and by record_at (zeroed-info form).
    let i = index_of(&cat, ZEROED)[0];
    for r in [f.lookup(ZEROED.as_bytes()), f.record_at(&cat, i).map(Some)] {
        assert!(matches!(
            r,
            Err(Dss7Error::BadInfoBlock {
                problem: InfoProblem::Zeroed,
                ..
            })
        ));
    }

    // Duplicate: HEC's choice (the chain copy) is zeroed; the orphan copy decodes.
    let chain = index_of(&cat, DUP)[0];
    let lower = index_of(&cat, &DUP.to_ascii_lowercase())[0];
    assert_eq!(cat.find(DUP.as_bytes()), Some(chain));
    assert_eq!(cat.meta[lower].duplicate_of, Some(chain));
    assert!(f.record_at(&cat, chain).is_err());
    let rec = f.record_at(&cat, lower).unwrap();
    let ts = f.read_ts_record(&rec).unwrap();
    assert_eq!(ts.ts.values, TsValues::F32(vec![40.0, 41.0, 42.0]));

    // Paired data through the time-series decoder: typed unsupported.
    let i = index_of(&cat, CURVE)[0];
    let rec = f.record_at(&cat, i).unwrap();
    assert!(matches!(
        f.read_ts_record(&rec),
        Err(TsError::UnsupportedRecordType {
            record_type: 200,
            ..
        })
    ));

    // Unlinked bin: lookup still finds it; recovery mode lists and opens it, and
    // picks the intact copy of the duplicate.
    assert!(f.lookup(UNLINKED.as_bytes()).unwrap().is_some());
    let rcat = f
        .catalog_with(&CatalogOptions {
            recover_unlinked: true,
            ..CatalogOptions::default()
        })
        .unwrap();
    let i = index_of(&rcat, UNLINKED)[0];
    assert!(rcat.meta[i].recovered);
    let rec = f.record_at(&rcat, i).unwrap();
    let ts = f.read_ts_record(&rec).unwrap();
    assert_eq!(ts.ts.values, TsValues::F32(vec![60.0, 61.0, 62.0]));
    assert_eq!(
        rcat.find(DUP.as_bytes()),
        Some(index_of(&rcat, &DUP.to_ascii_lowercase())[0])
    );

    // The committed copy must be exactly what this builder writes.
    let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_recovery7.dss");
    let c = std::fs::read(&committed).expect("fixtures/public/synthetic_recovery7.dss is missing");
    assert_eq!(c, bytes, "fixtures/public/synthetic_recovery7.dss is stale");
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_recovery7.dss"), bytes).unwrap();
    }
}

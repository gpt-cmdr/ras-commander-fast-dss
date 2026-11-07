//! P40: a digest of the profile, location and catalog-index routes, shared by the host
//! test (`route_fixtures.rs`) and the Node test (`node_routes.rs`). The P35 digest and its
//! table are left alone, so they keep proving the P35 reads did not change.
#![allow(dead_code)]

use super::{dataset_key, hash_ts, hash_values, Fnv};
use dss_wasm::reader::{DssBuffer, EntryProvenance, LocationData, ProfileData, ReadError};

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../../../../fixtures/public/", $name)) as &[u8],
        )
    };
}

/// Public fixtures with profiles (float and double), location records (DSS7, Latin-1
/// text, DSS6 headers with user-header text), interrupted-write cases and ordinary
/// time series on both versions.
pub const ROUTE_FIXTURES: &[(&str, &[u8])] = &[
    fixture!("synthetic_profile7.dss"),
    fixture!("synthetic_profile_f32_7.dss"),
    fixture!("synthetic_location7.dss"),
    fixture!("synthetic_location_latin1_7.dss"),
    fixture!("synthetic_location_uh6.dss"),
    fixture!("synthetic_recovery7.dss"),
    fixture!("synthetic_python6.dss"),
    fixture!("synthetic_python7.dss"),
    fixture!("castro.dss"),
];

/// Counts and a hash of the new routes on one fixture in one mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteDigest {
    pub entries: usize,
    pub at_records: usize,
    pub profiles: usize,
    pub locations: usize,
    pub associated: usize,
    pub errors: usize,
    pub values: usize,
    pub hash: u64,
}

pub fn hash_err(h: &mut Fnv, e: &ReadError) {
    h.str(e.kind.as_str());
    h.str(e.kind.error_name());
    h.str(&e.message);
    if let Some(u) = &e.unavailable {
        h.str(u.problem);
        h.i64(u.index as i64);
        h.i64(u.info_address as i64);
    }
}

pub fn hash_provenance(h: &mut Fnv, p: &EntryProvenance) {
    h.i64(p.index as i64);
    h.i64(i64::from(p.version));
    h.str(&p.pathname);
    h.i64(i64::from(p.record_type));
    h.str(p.status);
    h.i64(p.raw_status);
    for b in [
        p.recover_mode,
        p.recovered,
        p.duplicate,
        p.reachable_by_lookup,
        p.selected_by_pathname,
        p.ts_supported,
        p.password_protected,
    ] {
        h.i64(i64::from(b));
    }
    h.i64(p.duplicate_of.map_or(-1, |i| i as i64));
    h.str(p.hash_chain);
    h.i64(p.bin_entry_address as i64);
    h.i64(p.info_address as i64);
    h.str(p.info_problem.unwrap_or("<none>"));
    h.str(p.info_detail.as_deref().unwrap_or("<none>"));
    h.str(p.outcome());
}

pub fn hash_profile(h: &mut Fnv, d: &ProfileData) -> usize {
    let p = &d.profile;
    h.bytes(&p.pathname);
    h.i64(i64::from(p.record_type));
    for &t in &p.times {
        h.i64(t);
    }
    let mut n = hash_values(h, &p.depths);
    n += hash_values(h, &p.values);
    h.str(&p.depth_units);
    h.str(&p.value_units);
    h.str(&p.data_type);
    h.str(p.time_zone.as_deref().unwrap_or("<none>"));
    let m = &p.meta;
    h.i64(m.interval.code);
    h.i64(i64::from(m.precision));
    h.i64(m.time_offset_seconds);
    h.i64(m.time_granularity_seconds);
    h.i64(m.julian_base_date);
    h.i64(m.quality_element_size as i64);
    for q in m.quality_words.iter().flatten() {
        h.i64(i64::from(*q));
    }
    for w in m.user_header.iter().chain(&m.internal_header.words) {
        h.i64(i64::from(*w));
    }
    h.i64(m.blocks_found as i64);
    h.i64(m.blocks_in_window as i64);
    for b in &m.block_pathnames {
        h.bytes(b);
    }
    n
}

pub fn hash_location(h: &mut Fnv, d: &LocationData) {
    let l = &d.location;
    h.i64(i64::from(d.version));
    h.i64(i64::from(d.record_type));
    h.bytes(&l.pathname);
    for v in [l.x, l.y, l.z] {
        h.bytes(&v.to_bits().to_le_bytes());
    }
    h.i64(i64::from(l.descriptors_stored));
    for c in [
        l.coordinate_system_code,
        l.coordinate_id,
        l.horizontal_units_code,
        l.horizontal_datum_code,
        l.vertical_units_code,
        l.vertical_datum_code,
    ] {
        h.i64(i64::from(c));
    }
    h.str(l.time_zone_name.as_deref().unwrap_or("<none>"));
    h.bytes(l.supplemental.as_deref().unwrap_or(b"<none>"));
    h.str(l.supplemental_text().as_deref().unwrap_or("<none>"));
    h.str(l.supplemental_encoding().unwrap_or("<none>"));
    for v in d.values1.iter().flatten() {
        h.i64(i64::from(*v));
    }
    h.str(d.program.as_deref().unwrap_or("<none>"));
    h.i64(d.last_write_ms.unwrap_or(i64::MIN));
    h.i64(d.block_julian.unwrap_or(i64::MIN));
}

pub fn open_mode(bytes: &[u8], recover: bool) -> DssBuffer {
    if recover {
        DssBuffer::new_recovery(bytes.to_vec()).expect("open")
    } else {
        DssBuffer::new(bytes.to_vec()).expect("open")
    }
}

/// Default limits, as the JavaScript defaults.
pub const MAX_BLOCKS: u64 = dss_wasm::reader::DEFAULT_MAX_WINDOW_BLOCKS;
pub const MAX_CELLS: u64 = dss_wasm::reader::DEFAULT_MAX_PROFILE_CELLS;

/// Every row through the catalog-index route, every profile both ways, every location
/// record, and the associated location of every live pathname.
pub fn route_digest(bytes: &[u8], recover: bool) -> RouteDigest {
    let mut h = Fnv::new();
    let mut d = RouteDigest {
        entries: 0,
        at_records: 0,
        profiles: 0,
        locations: 0,
        associated: 0,
        errors: 0,
        values: 0,
        hash: 0,
    };
    let mut f = open_mode(bytes, recover);
    let cat = f.catalog().expect("catalog");
    d.entries = cat.len();
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..cat.len() {
        match f.entry_provenance(i) {
            Ok(p) => hash_provenance(&mut h, &p),
            Err(e) => hash_err(&mut h, &e),
        }
        match f.read_record_at(i) {
            Ok((p, ts)) => {
                d.at_records += 1;
                hash_provenance(&mut h, &p);
                d.values += hash_ts(&mut h, &ts);
            }
            Err(e) => {
                d.errors += 1;
                hash_err(&mut h, &e);
            }
        }
        if !matches!(cat.status[i], "live" | "alias") || cat.duplicate[i] {
            continue;
        }
        let p = cat.stored_pathname[i].clone();
        let t = cat.record_type[i];
        if (100..200).contains(&t) {
            match f.read_profile_record(&p, MAX_CELLS) {
                Ok(r) => {
                    d.profiles += 1;
                    d.values += hash_profile(&mut h, &r);
                }
                Err(e) => {
                    d.errors += 1;
                    hash_err(&mut h, &e);
                }
            }
            if dataset_key(&p).is_some_and(|k| seen.insert(k)) {
                match f.read_profile(&p, None, true, MAX_BLOCKS, MAX_CELLS) {
                    Ok(r) => {
                        d.profiles += 1;
                        d.values += hash_profile(&mut h, &r);
                    }
                    Err(e) => {
                        d.errors += 1;
                        hash_err(&mut h, &e);
                    }
                }
            }
        }
        if t == dss_core::location::LOCATION {
            match f.read_location(&p) {
                Ok(l) => {
                    d.locations += 1;
                    hash_location(&mut h, &l);
                }
                Err(e) => {
                    d.errors += 1;
                    hash_err(&mut h, &e);
                }
            }
        }
        match f.read_associated_location(&p) {
            Ok(Some(l)) => {
                d.associated += 1;
                hash_location(&mut h, &l);
            }
            Ok(None) => h.str("<no location>"),
            Err(e) => {
                d.errors += 1;
                hash_err(&mut h, &e);
            }
        }
    }
    d.hash = h.0;
    d
}

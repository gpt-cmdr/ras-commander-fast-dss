//! Shared by the host test (`reader_fixtures.rs`) and the Node test (`node.rs`): a
//! digest of everything the read layer returns for a fixture. The same digest computed
//! on the host and inside `wasm32-unknown-unknown` must equal the recorded table, so a
//! 32-bit `usize` or float difference in WASM shows up as a mismatch.
#![allow(dead_code)]

pub mod expected;
pub mod expected_routes;
pub mod routes;

use dss_core::{GridReadOptions, TsKind, TsValues};
use dss_wasm::reader::{DssBuffer, ReadError, TsData};

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../../../../fixtures/public/", $name)) as &[u8],
        )
    };
}

/// Public fixtures covering every exported read on both versions: regular and
/// irregular time series, patterns, paired data (PD and PDD) and grids (HRAP, Albers,
/// specified, ZLIB, PRECIP_2_BYTE).
pub const FIXTURES: &[(&str, &[u8])] = &[
    fixture!("castro.dss"),
    fixture!("synthetic_python6.dss"),
    fixture!("synthetic_python7.dss"),
    fixture!("synthetic_hec_variants6.dss"),
    fixture!("synthetic_irregular_variants7.dss"),
    fixture!("p21_hec_its7.dss"),
    fixture!("pattern_offset7.dss"),
    fixture!("synthetic_pd6.dss"),
    fixture!("synthetic_pd7.dss"),
    fixture!("p23_hec_pd6.dss"),
    fixture!("p23_hec_pd7.dss"),
    fixture!("real_grid6.dss"),
    fixture!("real_grid7.dss"),
    fixture!("p24_hec_grid6.dss"),
    fixture!("p24_hec_grid7.dss"),
];

/// Counts and a hash of every read of one fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digest {
    pub entries: usize,
    pub records: usize,
    pub datasets: usize,
    pub paired: usize,
    pub grids: usize,
    pub errors: usize,
    pub values: usize,
    pub hash: u64,
}

/// 64-bit FNV-1a.
pub struct Fnv(pub u64);

impl Fnv {
    pub fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Separator so concatenations differ.
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }
    pub fn i64(&mut self, v: i64) {
        self.bytes(&v.to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

pub fn hash_values(h: &mut Fnv, v: &TsValues) -> usize {
    match v {
        TsValues::F32(x) => {
            h.i64(32);
            for f in x {
                h.bytes(&f.to_bits().to_le_bytes());
            }
            x.len()
        }
        TsValues::F64(x) => {
            h.i64(64);
            for f in x {
                h.bytes(&f.to_bits().to_le_bytes());
            }
            x.len()
        }
    }
}

pub fn hash_ts(h: &mut Fnv, d: &TsData) -> usize {
    let ts = &d.ts;
    h.i64(i64::from(d.version));
    h.bytes(&ts.pathname);
    h.i64(i64::from(ts.record_type));
    match ts.kind {
        TsKind::Regular { interval_seconds } => h.i64(interval_seconds),
        TsKind::Irregular { block } => h.str(&format!("{block:?}")),
    }
    for &t in &ts.times {
        h.i64(t);
    }
    let n = hash_values(h, &ts.values);
    if let Some(q) = &ts.quality {
        for &x in q {
            h.i64(i64::from(x));
        }
    }
    h.str(&ts.units);
    h.str(&ts.data_type);
    h.str(ts.time_zone.as_deref().unwrap_or("<none>"));
    h.i64(d.time_offset_seconds);
    h.i64(i64::from(d.pattern));
    h.i64(d.blocks_found as i64);
    h.i64(d.blocks_missing as i64);
    h.i64(i64::from(d.precision.unwrap_or(i32::MIN)));
    for w in &d.warnings {
        h.str(w);
    }
    n
}

fn hash_err(h: &mut Fnv, e: &ReadError) {
    h.str(e.kind.as_str());
    h.str(&e.message);
}

/// Dataset key: the pathname with an empty D part (DSS6 and DSS7 alike).
pub fn dataset_key(p: &str) -> Option<String> {
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() != 8 {
        return None;
    }
    Some(format!(
        "/{}/{}/{}//{}/{}/",
        parts[1], parts[2], parts[3], parts[5], parts[6]
    ))
}

/// Read everything the exports read: catalog, each live record by type, and each
/// time-series dataset once (whole, trimmed).
pub fn digest(bytes: &[u8]) -> Digest {
    let mut h = Fnv::new();
    let mut d = Digest {
        entries: 0,
        records: 0,
        datasets: 0,
        paired: 0,
        grids: 0,
        errors: 0,
        values: 0,
        hash: 0,
    };
    let mut f = DssBuffer::new(bytes.to_vec()).expect("open");
    let sig = f.signature().clone();
    h.str(&sig.version_string);
    h.i64(sig.number_records);
    let cat = f.catalog().expect("catalog");
    d.entries = cat.len();
    for i in 0..cat.len() {
        h.str(&cat.pathname[i]);
        h.str(&cat.stored_pathname[i]);
        h.i64(i64::from(cat.record_type[i]));
        h.str(cat.status[i]);
        h.i64(cat.raw_status[i]);
        h.i64(cat.last_write_ms[i]);
        h.i64(i64::from(cat.duplicate[i]));
    }
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..cat.len() {
        if !matches!(cat.status[i], "live" | "alias") || cat.duplicate[i] {
            continue;
        }
        let p = cat.stored_pathname[i].clone();
        let t = cat.record_type[i];
        h.str(&p);
        match t {
            100..=199 => {
                match f.read_record(&p) {
                    Ok(r) => {
                        d.records += 1;
                        d.values += hash_ts(&mut h, &r);
                    }
                    Err(e) => {
                        d.errors += 1;
                        hash_err(&mut h, &e);
                    }
                }
                if let Some(k) = dataset_key(&p) {
                    if seen.insert(k) {
                        match f.read_dataset(&p, None, true) {
                            Ok(r) => {
                                d.datasets += 1;
                                d.values += hash_ts(&mut h, &r);
                            }
                            Err(e) => {
                                d.errors += 1;
                                hash_err(&mut h, &e);
                            }
                        }
                    }
                }
            }
            200..=299 => match f.read_paired_data(&p) {
                Ok(r) => {
                    d.paired += 1;
                    let pd = &r.pd;
                    h.i64(i64::from(r.version));
                    h.i64(pd.number_ordinates as i64);
                    h.i64(pd.number_curves as i64);
                    d.values += hash_values(&mut h, &pd.ordinates);
                    d.values += hash_values(&mut h, &pd.values);
                    for s in [
                        &pd.units_independent,
                        &pd.type_independent,
                        &pd.units_dependent,
                        &pd.type_dependent,
                    ] {
                        h.str(s);
                    }
                    for l in &pd.labels {
                        h.str(l);
                    }
                    h.i64(i64::from(pd.x_precision));
                    h.i64(i64::from(pd.y_precision));
                }
                Err(e) => {
                    d.errors += 1;
                    hash_err(&mut h, &e);
                }
            },
            400..=499 => match f.read_grid(&p, &GridReadOptions::default()) {
                Ok(r) => {
                    d.grids += 1;
                    let g = &r.grid;
                    h.i64(i64::from(r.version));
                    h.str(&format!("{:?}", g.grid_type));
                    h.i64(g.nx as i64);
                    h.i64(g.ny as i64);
                    h.i64(i64::from(g.lower_left_cell_x));
                    h.i64(i64::from(g.lower_left_cell_y));
                    h.bytes(&g.cell_size.to_bits().to_le_bytes());
                    h.bytes(&g.null_value.to_bits().to_le_bytes());
                    h.str(&g.srs_definition);
                    h.str(&g.data_units);
                    h.i64(g.start_time.unwrap_or(i64::MIN));
                    h.i64(g.end_time.unwrap_or(i64::MIN));
                    h.str(&format!("{:?}", g.albers));
                    for c in &g.data {
                        h.bytes(&c.to_bits().to_le_bytes());
                    }
                    d.values += g.data.len();
                }
                Err(e) => {
                    d.errors += 1;
                    hash_err(&mut h, &e);
                }
            },
            _ => {}
        }
    }
    d.hash = h.0;
    d
}

//! P34: real HEC-written grids against values recorded from HEC Monolith 3.3.27.
//!
//! `fixtures/public/real_grid{6,7}.dss` were written by Monolith through its Java API:
//! five public USACE grids
//! copied unchanged (SHG Albers PER-CUM precipitation from the HEC-RAS 7.0 example
//! `BaldEagleCrkMulti2D`, HRAP PER-CUM precipitation from the HEC-HMS 4.13 sample
//! `tenk/hrap.dss`) plus two synthetic INST-VAL grids on a specified UTM zone 17N WKT
//! grid, ZLIB compressed. `fixtures/public/real_grid_monolith.json` holds what Monolith's
//! `HecDss.get` returned for each record; Monolith is not run here. No dependency is
//! added: the test carries a minimal JSON reader and SHA-256.
use dss_core::{dss6::Dss6File, dss7::Dss7File, time, Grid, GridType, GRID_UNDEFINED};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

// ------------------------------------------------------------------ minimal JSON
#[derive(Debug, Clone, PartialEq)]
enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(BTreeMap<String, J>),
}

impl J {
    fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(m) => m.get(k),
            _ => None,
        }
    }
    fn at(&self, k: &str) -> &J {
        self.get(k).unwrap_or_else(|| panic!("missing key {k}"))
    }
    fn s(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => panic!("not a string: {self:?}"),
        }
    }
    fn i(&self) -> i64 {
        match self {
            J::Num(v) => *v as i64,
            _ => panic!("not a number: {self:?}"),
        }
    }
    fn a(&self) -> &[J] {
        match self {
            J::Arr(v) => v,
            _ => panic!("not an array: {self:?}"),
        }
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) {
        self.ws();
        assert_eq!(self.b[self.i], c, "JSON at {}", self.i);
        self.i += 1;
    }
    fn string(&mut self) -> String {
        self.eat(b'"');
        let mut out = String::new();
        loop {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return out,
                b'\\' => {
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'u' => {
                            let h = std::str::from_utf8(&self.b[self.i..self.i + 4]).unwrap();
                            out.push(char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap());
                            self.i += 4;
                        }
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        other => out.push(other as char),
                    }
                }
                _ => {
                    // UTF-8 continuation bytes are copied through.
                    let start = self.i - 1;
                    let mut end = self.i;
                    while end < self.b.len() && (self.b[end] & 0xc0) == 0x80 {
                        end += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.b[start..end]).unwrap());
                    self.i = end;
                }
            }
        }
    }
    fn value(&mut self) -> J {
        self.ws();
        match self.b[self.i] {
            b'{' => {
                self.i += 1;
                let mut m = BTreeMap::new();
                self.ws();
                if self.b[self.i] == b'}' {
                    self.i += 1;
                    return J::Obj(m);
                }
                loop {
                    let k = self.string();
                    self.eat(b':');
                    m.insert(k, self.value());
                    self.ws();
                    self.i += 1;
                    if self.b[self.i - 1] == b'}' {
                        return J::Obj(m);
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.b[self.i] == b']' {
                    self.i += 1;
                    return J::Arr(v);
                }
                loop {
                    v.push(self.value());
                    self.ws();
                    self.i += 1;
                    if self.b[self.i - 1] == b']' {
                        return J::Arr(v);
                    }
                }
            }
            b'"' => J::Str(self.string()),
            b'n' => {
                self.i += 4;
                J::Null
            }
            b't' => {
                self.i += 4;
                J::Bool(true)
            }
            b'f' => {
                self.i += 5;
                J::Bool(false)
            }
            _ => {
                let s = self.i;
                while self.i < self.b.len() && b"+-.eE0123456789".contains(&self.b[self.i]) {
                    self.i += 1;
                }
                J::Num(
                    std::str::from_utf8(&self.b[s..self.i])
                        .unwrap()
                        .parse()
                        .unwrap(),
                )
            }
        }
    }
}

fn reference() -> J {
    let b = std::fs::read(fixture("real_grid_monolith.json")).unwrap();
    P { b: &b, i: 0 }.value()
}

// ------------------------------------------------------------------ SHA-256 (FIPS 180-4)
fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in m.chunks_exact(64) {
        let mut w = [0u32; 64];
        for t in 0..16 {
            w[t] = u32::from_be_bytes(chunk[4 * t..4 * t + 4].try_into().unwrap());
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for t in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[t])
                .wrapping_add(w[t]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

// ------------------------------------------------------------------ helpers
const SHA6: &str = "66dcdf3ed9c8474d13fdf95e78391bf2749a3bc0a3d5f87358008bb3cd44afb9";
const SHA7: &str = "48a6bef95a62cae1075eb95696df25ab1a4e60851634067106cd39733853c403";
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn bits(v: f32) -> String {
    format!("{:08x}", v.to_bits())
}

/// HecTime text as Monolith's `GridInfo.getStartTime` returns it (`1 January 2020, 06:00`).
fn hec_time(t: i64) -> String {
    let (julian, secs) = time::split(t, false);
    let (y, m, d) = time::julian_to_ymd(julian);
    format!(
        "{d} {} {y}, {:02}:{:02}",
        MONTHS[m as usize - 1],
        secs / 3600,
        secs % 3600 / 60
    )
}

fn read(name: &str, pathname: &str) -> Grid {
    if name.ends_with("6.dss") {
        Dss6File::open(fixture(name))
            .unwrap()
            .read_grid(pathname.as_bytes())
            .unwrap()
    } else {
        Dss7File::open(fixture(name))
            .unwrap()
            .read_grid(pathname.as_bytes())
            .unwrap()
    }
}

fn class(t: GridType) -> &'static str {
    match t {
        GridType::Albers => "hec.heclib.grid.AlbersInfo",
        GridType::Hrap => "hec.heclib.grid.HrapInfo",
        GridType::Specified => "hec.heclib.grid.SpecifiedGridInfo",
        GridType::Undefined => "hec.heclib.grid.GridInfo",
    }
}

fn data_type_name(code: i32) -> &'static str {
    // heclib zStructSpatialGrid.h / Java GridInfo.getDataTypeName.
    match code {
        0 => "PER-AVER",
        1 => "PER-CUM",
        2 => "INST-VAL",
        3 => "INST-CUM",
        _ => "?",
    }
}

fn records(r: &J) -> Vec<(String, J)> {
    let mut out = Vec::new();
    for name in ["real_grid6.dss", "real_grid7.dss"] {
        for rec in r.at("files").at(name).at("records").a() {
            out.push((name.to_string(), rec.clone()));
        }
    }
    out
}

fn null_zero(name: &str, rec: &J) -> bool {
    name == "real_grid7.dss"
        && rec.at("grid_class").s() != "hec.heclib.grid.SpecifiedGridInfo"
        && rec.at("min_bits").s() == "00000000"
}

// ------------------------------------------------------------------ tests
#[test]
fn fixtures_sha256_and_catalog() {
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256(&std::fs::read(fixture("real_grid6.dss")).unwrap()),
        SHA6
    );
    assert_eq!(
        sha256(&std::fs::read(fixture("real_grid7.dss")).unwrap()),
        SHA7
    );
    let r = reference();
    let want = |name: &str| {
        let mut v: Vec<String> = r
            .at("files")
            .at(name)
            .at("records")
            .a()
            .iter()
            .map(|x| x.at("pathname").s().to_string())
            .collect();
        v.sort();
        v
    };
    let mut six: Vec<String> = Dss6File::open(fixture("real_grid6.dss"))
        .unwrap()
        .catalog()
        .unwrap()
        .entries
        .iter()
        .map(|e| String::from_utf8(e.pathname.clone()).unwrap())
        .collect();
    six.sort();
    assert_eq!(six, want("real_grid6.dss"));
    let mut seven: Vec<String> = Dss7File::open(fixture("real_grid7.dss"))
        .unwrap()
        .catalog()
        .unwrap()
        .catalog
        .entries
        .iter()
        .map(|e| String::from_utf8(e.pathname.clone()).unwrap())
        .collect();
    seven.sort();
    assert_eq!(seven, want("real_grid7.dss"));
}

#[test]
fn values_metadata_and_crs_match_monolith() {
    let r = reference();
    let recs = records(&r);
    assert_eq!(recs.len(), 14);
    for (name, rec) in &recs {
        let p = rec.at("pathname").s();
        let g = read(name, p);
        let ctx = format!("{name} {p}");
        assert_eq!(class(g.grid_type), rec.at("grid_class").s(), "{ctx}");
        assert_eq!(
            i64::from(g.record_type),
            rec.at("grid_type_code").i(),
            "{ctx}"
        );
        assert_eq!(
            i64::from(g.data_type),
            rec.at("data_type_code").i(),
            "{ctx}"
        );
        assert_eq!(
            data_type_name(g.data_type),
            rec.at("data_type").s(),
            "{ctx}"
        );
        assert_eq!(g.data_units, rec.at("units").s(), "{ctx}");
        assert_eq!(g.nx as i64, rec.at("nx").i(), "{ctx}");
        assert_eq!(g.ny as i64, rec.at("ny").i(), "{ctx}");
        let llc = rec.at("lower_left_cell").a();
        assert_eq!(
            (
                i64::from(g.lower_left_cell_x),
                i64::from(g.lower_left_cell_y)
            ),
            (llc[0].i(), llc[1].i()),
            "{ctx}"
        );
        assert_eq!(bits(g.cell_size), rec.at("cell_size_bits").s(), "{ctx}");
        assert_eq!(g.srs_definition, rec.at("crs").s(), "{ctx}");
        if let Some(cz) = rec.get("cell_zero_bits") {
            let cz = cz.a();
            assert_eq!(bits(g.x_coord_of_grid_cell_zero), cz[0].s(), "{ctx}");
            assert_eq!(bits(g.y_coord_of_grid_cell_zero), cz[1].s(), "{ctx}");
        }
        let c = rec.at("compression");
        assert_eq!(i64::from(g.compression_method), c.at("method").i(), "{ctx}");
        assert_eq!(bits(g.compression_base), c.at("base_bits").s(), "{ctx}");
        assert_eq!(
            bits(g.compression_scale_factor),
            c.at("scale_factor_bits").s(),
            "{ctx}"
        );
        assert_eq!(
            g.sizeof_compressed_elements as i64,
            c.at("element_size").i(),
            "{ctx}"
        );
        assert_eq!(
            bits(g.min_data_value.unwrap()),
            rec.at("min_bits").s(),
            "{ctx}"
        );
        assert_eq!(
            bits(g.max_data_value.unwrap()),
            rec.at("max_bits").s(),
            "{ctx}"
        );
        assert_eq!(
            bits(g.mean_data_value.unwrap()),
            rec.at("mean_bits").s(),
            "{ctx}"
        );
        let t = rec.at("timing");
        let (start, end) = (
            hec_time(g.start_time.unwrap()),
            hec_time(g.end_time.unwrap()),
        );
        assert_eq!(start, t.at("start").s(), "{ctx}");
        assert_eq!(end, t.at("end").s(), "{ctx}");
        assert_eq!(format!("{start} to {end}"), t.at("period").s(), "{ctx}");
        // Cells in stored order are Monolith's GridData.getData() bits.
        assert_eq!(g.data.len() as i64, rec.at("n_cells").i(), "{ctx}");
        let le: Vec<u8> = g.data.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(sha256(&le), rec.at("data_sha256").s(), "{ctx}");
        for s in rec.at("samples").a() {
            let s = s.a();
            assert_eq!(bits(g.data[s[0].i() as usize]), s[1].s(), "{ctx}");
        }
        let nodata = g.data.iter().filter(|&&v| v == GRID_UNDEFINED).count();
        assert_eq!(nodata as i64, rec.at("n_nodata").i(), "{ctx}");
        let nan = g.data.iter().filter(|v| v.is_nan()).count();
        assert_eq!(nan as i64, rec.at("n_nan").i(), "{ctx}");
        // Missing cells agree with Monolith's GridData.getNumberMissing.
        let missing = g.data.iter().filter(|&&v| g.is_missing(v)).count();
        assert_eq!(missing as i64, rec.at("number_missing").i(), "{ctx}");
    }
}

fn albers_matches(name: &str) {
    let r = reference();
    let mut n = 0;
    for rec in r.at("files").at(name).at("records").a() {
        let Some(a) = rec.get("albers") else { continue };
        let p = rec.at("pathname").s();
        let g = read(name, p);
        let got = g
            .albers
            .as_ref()
            .unwrap_or_else(|| panic!("{name} {p}: no AlbersProjection"));
        assert_eq!(i64::from(got.datum), a.at("datum_code").i());
        assert_eq!(got.units, a.at("units").s());
        assert_eq!(
            bits(got.first_standard_parallel),
            a.at("standard_parallel_1").s()
        );
        assert_eq!(
            bits(got.second_standard_parallel),
            a.at("standard_parallel_2").s()
        );
        assert_eq!(bits(got.central_meridian), a.at("central_meridian").s());
        assert_eq!(
            bits(got.latitude_of_projection_origin),
            a.at("latitude_of_origin").s()
        );
        assert_eq!(bits(got.false_easting), a.at("false_easting").s());
        assert_eq!(bits(got.false_northing), a.at("false_northing").s());
        n += 1;
    }
    assert_eq!(n, 3);
}

#[test]
fn legacy_albers_parameters_match_monolith() {
    albers_matches("real_grid6.dss");
}

/// -100 Albers parameters come from the stored WKT, as Monolith's
/// `GridStructContainer.returnGridInfo` -> `setAlbersSpatialReferenceInfo`.
#[test]
fn modern_albers_parameters_match_monolith() {
    albers_matches("real_grid7.dss");
}

/// Monolith writes -100 HRAP/Albers grids with null value 0.0
/// (`GridStructContainer` carries `_nullValue` only for `SpecifiedGridInfo`); those
/// 0.0 cells are data, as in Monolith's `GridData.getNumberMissing`.
#[test]
fn modern_hrap_albers_zero_cells_are_not_missing() {
    let r = reference();
    let mut n = 0;
    for (name, rec) in records(&r) {
        if !null_zero(&name, &rec) {
            continue;
        }
        let g = read(&name, rec.at("pathname").s());
        let missing = g.data.iter().filter(|&&v| g.is_missing(v)).count();
        assert_eq!(missing as i64, rec.at("number_missing").i(), "{name}");
        n += 1;
    }
    assert_eq!(n, 4);
}

#[test]
fn modern_hrap_albers_null_value_is_zero_as_stored() {
    // Pins the stored bits behind F1: the value is kept, only not applied as missing.
    let r = reference();
    for (name, rec) in records(&r) {
        let g = read(&name, rec.at("pathname").s());
        let expect = if name == "real_grid7.dss" && g.grid_type != GridType::Specified {
            0.0
        } else {
            GRID_UNDEFINED
        };
        assert_eq!(g.null_value.to_bits(), expect.to_bits(), "{name}");
        assert_eq!(g.struct_version == -100, name == "real_grid7.dss", "{name}");
    }
}

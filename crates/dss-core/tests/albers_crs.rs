//! P13: SHG WKT selection for Albers grids (`Grid::srs_definition`).
//!
//! Every record of `fixtures/public/synthetic_albers{6,7}.dss` flips one input of
//! HEC Monolith 3.3.27 `AlbersInfo.getSpatialReferenceSystem` (legacy layouts,
//! via `AlbersInfoFlat.unflatten` + `AlbersInfo.convertToGridInfo`) or of
//! `GridStructContainer.setAlbersSpatialReferenceInfo` (modern -100 layout). The
//! expected value of each record (SHG WKT or empty) was checked against Monolith on
//! copies of these files. Synthetic data only; no HEC software is
//! used to generate the files. To regenerate, set DSS_PY_FIXTURE_OUT to a scratch
//! directory, run this test, and copy both files into fixtures/public/.
mod dss7_common;
#[path = "dss6_ts_support/mod.rs"]
mod v6;

use dss_core::{dss6::Dss6File, dss7::Dss7File, Grid};

/// zStructSpatialGrid.h:SHG_SRC_DEFINITION (spelling retained).
const SHG: &str = "PROJCS[\"USA_Contiguous_Albers_Equal_Area_Conic_USGS_version\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Albers\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",-96.0],PARAMETER[\"Standard_Parallel_1\",29.5],PARAMETER[\"Standard_Parallel_2\",45.5],PARAMETER[\"Latitude_Of_Origin\",23.0],UNIT[\"Meter\",1.0]]";
/// HecTime minutes of 01JAN2020 00:00.
const START: i32 = 43_830 * 1440;

fn bits(v: f32) -> i32 {
    v.to_bits() as i32
}
fn raw(words: &[i32]) -> Vec<u8> {
    words.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn pack(bytes: &[u8]) -> Vec<i32> {
    let mut b = bytes.to_vec();
    b.resize(b.len().div_ceil(4) * 4, 0);
    b.chunks_exact(4)
        .map(|v| i32::from_le_bytes([v[0], v[1], v[2], v[3]]))
        .collect()
}
fn path(tag: &str) -> String {
    format!("/PUBLIC/ALBERS-{tag}/PRECIP/01JAN2020:0000/01JAN2020:0100/SYNTH/")
}
fn cells() -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(&raw(&[0.5f32, 1.0, 1.5, 2.0].map(bits)), 6)
}

/// Legacy Albers header (GridInfoFlat 63-int base + AlbersInfoFlat 12 ints), 2 x 2
/// ZLIB, SHG projection with origin 0/0 unless a variant changes it.
#[derive(Clone)]
struct Legacy {
    datum: i32,
    units: [u8; 12],
    p: [f32; 6], // sp1, sp2, cm, lat0, fe, fn
    origin: [f32; 2],
}
impl Default for Legacy {
    fn default() -> Self {
        Self {
            datum: 2,
            units: units(b"Meters"),
            p: [29.5, 45.5, -96.0, 23.0, 0.0, 0.0],
            origin: [0.0, 0.0],
        }
    }
}
fn units(s: &[u8]) -> [u8; 12] {
    let mut u = [0u8; 12];
    u[..s.len()].copy_from_slice(s);
    u
}
fn legacy_header(v: &Legacy, nbytes: usize) -> Vec<i32> {
    let mut h = vec![0; 75];
    h[0] = 300;
    h[1] = 420;
    h[2] = 164;
    h[3] = 124;
    h[4] = START;
    h[5] = START + 60;
    h[6] = i32::from_le_bytes(*b"MM  ");
    h[9] = 1;
    h[12] = 2;
    h[13] = 2;
    h[14] = bits(2000.0);
    h[15] = 26;
    h[16] = nbytes as i32;
    h[19] = bits(2.0);
    h[20] = bits(0.5);
    h[21] = bits(1.25);
    h[63] = v.datum;
    h[64..67].copy_from_slice(&pack(&v.units));
    for (i, x) in v.p.iter().enumerate() {
        h[67 + i] = bits(*x);
    }
    h[73] = bits(v.origin[0]);
    h[74] = bits(v.origin[1]);
    h
}

/// (tag, variant, Monolith returns the SHG WKT)
fn legacy_cases() -> Vec<(&'static str, Legacy, bool)> {
    let d = Legacy::default;
    let with = |f: &dyn Fn(&mut Legacy)| {
        let mut v = d();
        f(&mut v);
        v
    };
    vec![
        ("L-BASE", d(), true),
        (
            "L-METERS-UPPER",
            with(&|v| v.units = units(b"METERS")),
            true,
        ),
        ("L-METER", with(&|v| v.units = units(b"meter")), true),
        ("L-METRE", with(&|v| v.units = units(b"metre")), true),
        ("L-METRES", with(&|v| v.units = units(b"Metres")), true),
        ("L-M", with(&|v| v.units = units(b"M")), true),
        ("L-FEET", with(&|v| v.units = units(b"Feet")), false),
        ("L-UNITS-EMPTY", with(&|v| v.units = [0; 12]), false),
        (
            "L-UNITS-BLANKPAD",
            with(&|v| v.units = units(b"Meters      ")),
            false,
        ),
        (
            "L-UNITS-2BLANK",
            with(&|v| v.units = units(b"Meters  ")),
            false,
        ),
        (
            "L-UNITS-LEADBLANK",
            with(&|v| v.units = units(b" Meters")),
            false,
        ),
        (
            "L-UNITS-NULJUNK",
            with(&|v| v.units = units(b"Meters\0xyz")),
            true,
        ),
        (
            "L-UNITS-METER-S",
            with(&|v| v.units = units(b"Meter s")),
            false,
        ),
        ("L-DATUM1", with(&|v| v.datum = 1), false),
        ("L-DATUM0", with(&|v| v.datum = 0), false),
        ("L-SP1", with(&|v| v.p[0] = 29.25), false),
        ("L-SP2", with(&|v| v.p[1] = 45.25), false),
        ("L-CM", with(&|v| v.p[2] = -95.0), false),
        ("L-LAT0", with(&|v| v.p[3] = 23.5), false),
        ("L-FE", with(&|v| v.p[4] = 1.0), false),
        ("L-FN", with(&|v| v.p[5] = 1.0), false),
        ("L-FE-NEGZERO", with(&|v| v.p[4] = -0.0), true),
        ("L-X0", with(&|v| v.origin[0] = 1000.0), false),
        ("L-Y0", with(&|v| v.origin[1] = 2000.0), false),
        ("L-X0-NEGZERO", with(&|v| v.origin = [-0.0, -0.0]), true),
        (
            "L-X0-SUBNORMAL",
            with(&|v| v.origin[0] = f32::from_bits(1)),
            false,
        ),
        (
            "L-SYNTH-GRID6",
            with(&|v| {
                v.units = units(b"Meters  ");
                v.origin = [1000.0, 2000.0];
            }),
            false,
        ),
    ]
}

/// Modern (-100) Albers: (tag, WKT, x0, y0, Monolith returns the SHG WKT).
fn modern_cases() -> Vec<(&'static str, String, f32, f32, bool)> {
    let r = |a: &str, b: &str| SHG.replacen(a, b, 1);
    vec![
        ("M-SHG", SHG.into(), 0.0, 0.0, true),
        ("M-SHG-X0", SHG.into(), 10.0, 0.0, false),
        ("M-SHG-Y0", SHG.into(), 0.0, 20.0, false),
        ("M-TEST", "TEST".into(), 0.0, 0.0, false),
        ("M-EMPTY", String::new(), 0.0, 0.0, false),
        (
            "M-NAD27",
            r("D_North_American_1983", "D_North_American_1927"),
            0.0,
            0.0,
            false,
        ),
        (
            "M-NAD83-HARN",
            r("D_North_American_1983", "D_North_American_1983_HARN"),
            0.0,
            0.0,
            true,
        ),
        (
            "M-UNIT-FOOT",
            r(
                "UNIT[\"Meter\",1.0]",
                "UNIT[\"Foot_US\",0.3048006096012192]",
            ),
            0.0,
            0.0,
            false,
        ),
        (
            "M-UNIT-METRE",
            r("UNIT[\"Meter\",1.0]", "UNIT[\"metre\",1.0]"),
            0.0,
            0.0,
            true,
        ),
        (
            "M-UNIT-NONE",
            r(",UNIT[\"Meter\",1.0]", ""),
            0.0,
            0.0,
            false,
        ),
        (
            "M-UNIT-NEWLINE",
            r("UNIT[\"Meter\"", "UNIT[\"Me\nter\""),
            0.0,
            0.0,
            false,
        ),
        (
            "M-NO-FE",
            r("PARAMETER[\"False_Easting\",0.0],", ""),
            0.0,
            0.0,
            true,
        ),
        (
            "M-FE-1",
            r("\"False_Easting\",0.0", "\"False_Easting\",1.0"),
            0.0,
            0.0,
            false,
        ),
        (
            "M-FE-SPACE",
            r("\"False_Easting\",0.0", "\"False_Easting\", 5.0"),
            0.0,
            0.0,
            true,
        ),
        (
            "M-CM-SPACE",
            r("\"Central_Meridian\",-96.0", "\"Central_Meridian\", -96.0"),
            0.0,
            0.0,
            false,
        ),
        (
            "M-CM-INT",
            r("\"Central_Meridian\",-96.0", "\"Central_Meridian\",-96"),
            0.0,
            0.0,
            true,
        ),
        (
            "M-CM-LONG",
            r(
                "\"Central_Meridian\",-96.0",
                "\"Central_Meridian\",-96.0000001",
            ),
            0.0,
            0.0,
            true,
        ),
        (
            "M-SP1-EXP",
            r(
                "\"Standard_Parallel_1\",29.5",
                "\"Standard_Parallel_1\",29.5e0",
            ),
            0.0,
            0.0,
            true,
        ),
        (
            "M-SP1-2DOT",
            r(
                "\"Standard_Parallel_1\",29.5",
                "\"Standard_Parallel_1\",29..5",
            ),
            0.0,
            0.0,
            false,
        ),
        (
            "M-LAT0-23.1",
            r("\"Latitude_Of_Origin\",23.0", "\"Latitude_Of_Origin\",23.1"),
            0.0,
            0.0,
            false,
        ),
        (
            "M-NODATUM",
            r("DATUM[\"D_North_American_1983\"", "DATUM[\"D_NAD_1983\""),
            0.0,
            0.0,
            false,
        ),
        // Standard_Parallel_2 and False_Northing flipped alone (each name is parsed
        // independently; a swapped name table would fail these).
        (
            "M-SP2-45.25",
            r(
                "\"Standard_Parallel_2\",45.5",
                "\"Standard_Parallel_2\",45.25",
            ),
            0.0,
            0.0,
            false,
        ),
        (
            "M-FN-1",
            r("\"False_Northing\",0.0", "\"False_Northing\",1.0"),
            0.0,
            0.0,
            false,
        ),
        (
            "M-FN-DOT-STYLE",
            r("\"False_Northing\",0.0", "\"False_Northing\",-.0"),
            0.0,
            0.0,
            true,
        ),
        (
            "M-FN-SPACE",
            r("\"False_Northing\",0.0", "\"False_Northing\", 1.0"),
            0.0,
            0.0,
            true,
        ),
    ]
}

fn modern_header(nbytes: usize) -> Vec<i32> {
    let mut h = vec![0; 24];
    h[0] = -100;
    h[1] = 420;
    h[2] = 2;
    h[3] = 1;
    h[6] = 2;
    h[7] = 2;
    h[8] = 26;
    h[9] = nbytes as i32;
    h[10] = 2;
    h[13] = 1;
    h[14] = 1;
    h
}

struct Rec {
    path: String,
    header: Vec<i32>,
    data: Vec<u8>,
    modern: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

fn records(version: u8) -> Vec<(Rec, bool)> {
    let z = cells();
    let mut out: Vec<(Rec, bool)> = legacy_cases()
        .into_iter()
        .map(|(tag, v, shg)| {
            (
                Rec {
                    path: path(tag),
                    header: legacy_header(&v, z.len()),
                    data: z.clone(),
                    modern: None,
                },
                shg,
            )
        })
        .collect();
    if version == 7 {
        for (tag, wkt, x0, y0, shg) in modern_cases() {
            let mut chars = b"MM\0PUBLIC\0WKT\0".to_vec();
            chars.extend_from_slice(wkt.as_bytes());
            chars.extend_from_slice(b"\0UTC\0");
            out.push((
                Rec {
                    path: path(tag),
                    header: modern_header(z.len()),
                    data: z.clone(),
                    modern: Some((
                        chars,
                        raw(&[bits(2000.0), bits(x0), bits(y0), bits(-9999.0)]),
                        raw(&[bits(0.5), bits(2.0), bits(1.25), bits(0.0), bits(1.0), 4, 3]),
                    )),
                },
                shg,
            ));
        }
    }
    out
}

fn file6(recs: &[(Rec, bool)]) -> Vec<u8> {
    let cat: Vec<_> = recs
        .iter()
        .map(|(r, _)| v6::Rec::live(&r.path, 420))
        .collect();
    let mut f = v6::ts_file(&cat, *b"6-YN");
    for (r, _) in recs {
        f.attach(
            &r.path,
            &v6::Payload {
                internal_header: r.header.clone(),
                data: pack(&r.data),
                ..Default::default()
            },
        );
    }
    f.finish()
}

fn file7(recs: &[(Rec, bool)]) -> Vec<u8> {
    use dss7_common::{build, set_word, Spec};
    let b = build(&Spec {
        recs: recs
            .iter()
            .map(|(r, _)| dss7_common::Rec::live(&r.path, 420))
            .collect(),
        ..Default::default()
    });
    let mut bytes = b.bytes;
    let mut next = bytes.len().div_ceil(8) as i64;
    for ((r, _), &info) in recs.iter().zip(&b.info_addr) {
        // Same area layout as grid_read.rs fx_file7().
        let mut attach = |key: i64, area: &[u8]| {
            let addr = next;
            next += area.len().div_ceil(8) as i64;
            bytes.resize((next * 8) as usize, 0);
            bytes[(addr * 8) as usize..(addr * 8) as usize + area.len()].copy_from_slice(area);
            set_word(&mut bytes, (info + key) as usize, addr);
            set_word(
                &mut bytes,
                (info + key + 1) as usize,
                area.len().div_ceil(4) as i64,
            );
        };
        attach(13, &raw(&r.header));
        match &r.modern {
            Some((chars, geometry, stats)) => {
                attach(15, chars);
                attach(17, geometry);
                attach(19, stats);
                attach(23, &r.data);
            }
            None => attach(19, &r.data),
        }
    }
    bytes.resize((next as usize + 1) * 8, 0);
    set_word(&mut bytes, 5, next);
    set_word(&mut bytes, next as usize, -97532);
    bytes
}

fn check(name: &str, recs: &[(Rec, bool)], read: &mut dyn FnMut(&[u8]) -> Grid) {
    let mut bad = Vec::new();
    for (r, shg) in recs {
        let g = read(r.path.as_bytes());
        // Payload is checked separately from the CRS metadata.
        assert_eq!(g.data, [0.5, 1.0, 1.5, 2.0], "{name} {} cells", r.path);
        let want = if *shg { SHG } else { "" };
        if g.srs_definition != want {
            bad.push(r.path.clone());
        }
    }
    assert!(bad.is_empty(), "{name}: SRS differs for {bad:#?}");
}

#[test]
fn albers_srs_matches_monolith() {
    let r6 = records(6);
    let r7 = records(7);
    let b6 = file6(&r6);
    let b7 = file7(&r7);
    if let Some(out) = std::env::var_os("DSS_PY_FIXTURE_OUT") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("synthetic_albers6.dss"), &b6).unwrap();
        std::fs::write(out.join("synthetic_albers7.dss"), &b7).unwrap();
    }
    let f6 = Dss6File::from_bytes(&b6).unwrap();
    check("dss6", &r6, &mut |p| f6.read_grid(p).unwrap());
    let mut f7 = Dss7File::from_bytes(&b7).unwrap();
    check("dss7", &r7, &mut |p| f7.read_grid(p).unwrap());

    let public = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public");
    for (name, bytes) in [
        ("synthetic_albers6.dss", &b6),
        ("synthetic_albers7.dss", &b7),
    ] {
        // A missing or unreadable committed fixture is a failure, not a skip.
        let committed = std::fs::read(public.join(name))
            .unwrap_or_else(|e| panic!("cannot read committed {name}: {e}"));
        assert!(
            committed == *bytes,
            "{name} differs from the generator; regenerate it"
        );
    }
}

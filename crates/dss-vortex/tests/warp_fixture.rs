//! Warp cases with no oracle at run time, so CI on every platform checks the same bits.
//!
//! Every expected value comes from Vortex 1.1.0 on public NOAA samples, and the portable maths
//! must reproduce it; any difference fails, on any platform:
//!
//! - `fixtures/public/vortex_p41/`: P41's 60 cases, through this production warp.
//! - `fixtures/public/vortex_p44/`: small grids cut from the decoded arrays, written by
//!   `oracle/vortex/make_p44_fixture.py`: no-envelope extents, windows off the source or past the
//!   projection's domain, other nodata markers, and whole-Earth, polar and thin synthetic grids.
//!
//! A digest covers every cell: value bits everywhere, and one word for all NaN encodings.

use dss_vortex::crs::{CrsDef, Envelope};
use dss_vortex::frame::GeoTransform;
use dss_vortex::warp::{Align, Method, SourceGrid, WarpEnvelope, WarpPlan, WarpSpec};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::PathBuf;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const P41_CASES: usize = 60;
const P41_CELLS: usize = 43_519_008;
const P44_CASES: usize = 162;
const P44_REFUSALS: usize = 3;

fn dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public")
        .join(name)
}

fn f64_bits(s: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(s, 16).unwrap())
}

fn digest(out: &[f32]) -> (usize, String) {
    let (mut valid, mut h) = (0, FNV_OFFSET);
    for v in out {
        let word = if v.is_nan() {
            0x7fc0_0000
        } else {
            valid += 1;
            v.to_bits()
        };
        for b in word.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    (valid, format!("{h:016x}"))
}

/// What Vortex gave for one case.
#[derive(Debug, Clone, PartialEq)]
struct Expected {
    nx: usize,
    ny: usize,
    gt: [f64; 4],
    valid: usize,
    digest: String,
}

/// The one comparison every case goes through, so the controls below test what the cases use.
fn differences(want: &Expected, nx: usize, ny: usize, gt: [f64; 4], out: &[f32]) -> Vec<String> {
    let mut found = Vec::new();
    if (nx, ny) != (want.nx, want.ny) {
        found.push(format!("grid {nx} x {ny}, want {} x {}", want.nx, want.ny));
    }
    if gt.map(f64::to_bits) != want.gt.map(f64::to_bits) {
        found.push(format!("geotransform {gt:?}, want {:?}", want.gt));
    }
    if out.len() != want.nx * want.ny {
        found.push(format!("{} cells, want {}", out.len(), want.nx * want.ny));
    }
    let (valid, got) = digest(out);
    if valid != want.valid {
        found.push(format!("{valid} valid cells, want {}", want.valid));
    }
    if got != want.digest {
        found.push(format!("digest {got}, want {}", want.digest));
    }
    found
}

fn run(plan: &WarpPlan, cells: &[f32], nodata: f32, want: &Expected) -> Vec<String> {
    let g = plan.geotransform();
    let out = plan.warp_cells(cells, nodata).unwrap();
    differences(want, plan.nx(), plan.ny(), [g[0], g[1], g[3], g[5]], &out)
}

fn decompress(path: PathBuf) -> Vec<f32> {
    let file = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(file.as_slice()).unwrap();
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw).unwrap();
    assert_eq!(raw.len() % 4, 0, "{}: trailing bytes", path.display());
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

struct Input {
    source: SourceGrid,
    nodata: f32,
    cells: Vec<f32>,
}

fn load_inputs() -> HashMap<String, Input> {
    let text = fs::read_to_string(dir("vortex_p44").join("inputs.txt")).unwrap();
    let mut inputs = HashMap::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let p: Vec<&str> = line.splitn(10, ' ').collect();
        let (nx, ny): (usize, usize) = (p[1].parse().unwrap(), p[2].parse().unwrap());
        let cells: Vec<f32> = if p[8] == "1" {
            // Generated as vortex_driver.py does; every value is exact in f32.
            (0..nx * ny)
                .map(|i| (i % 97) as f32 + (i % 89) as f32 * 0.25)
                .collect()
        } else {
            decompress(dir("vortex_p44").join(format!("{}.f32.zst", p[0])))
        };
        assert_eq!(cells.len(), nx * ny, "{}", p[0]);
        inputs.insert(
            p[0].to_string(),
            Input {
                source: SourceGrid {
                    nx,
                    ny,
                    transform: GeoTransform::axis_aligned(
                        f64_bits(p[3]),
                        f64_bits(p[4]),
                        f64_bits(p[5]),
                        f64_bits(p[6]),
                    ),
                    crs: CrsDef::parse_wkt(p[9]).unwrap(),
                },
                nodata: f32::from_bits(u32::from_str_radix(p[7], 16).unwrap()),
                cells,
            },
        );
    }
    inputs
}

fn method(name: &str) -> Method {
    match name {
        "near" => Method::Near,
        "bilinear" => Method::Bilinear,
        "average" => Method::Average,
        other => panic!("method {other}"),
    }
}

fn spec(method: Method, envelope: Option<Envelope>, cell: Option<f64>) -> WarpSpec {
    let mut spec = WarpSpec::new(method);
    spec.target = Some(CrsDef::shg());
    spec.align = Align::Vortex;
    spec.cell_size = cell;
    spec.envelope = envelope.map(|envelope| WarpEnvelope {
        envelope,
        crs: Some(CrsDef::shg()),
    });
    spec
}

#[test]
fn every_p44_fixture_case_gives_the_grid_and_bits_vortex_gave() {
    let inputs = load_inputs();
    let text = fs::read_to_string(dir("vortex_p44").join("cases.txt")).unwrap();
    let (mut count, mut refused, mut failures) = (0, 0, Vec::new());
    let mut seen = HashSet::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let p: Vec<&str> = line.split(' ').collect();
        assert!(seen.insert(p[0]), "{} is listed twice", p[0]);
        let input = &inputs[p[1]];
        let envelope = (p[3] != "-").then(|| {
            let v: Vec<f64> = p[3].split(',').map(f64_bits).collect();
            Envelope {
                min_x: v[0],
                max_x: v[1],
                min_y: v[2],
                max_y: v[3],
            }
        });
        let cell = (p[4] != "-").then(|| f64_bits(p[4]));
        count += 1;
        let plan = WarpPlan::new(&input.source, &spec(method(p[2]), envelope, cell));
        if p[5] == "refused" {
            refused += 1;
            if plan.is_ok() {
                failures.push(format!(
                    "{}: Vortex failed here, the warp did not refuse",
                    p[0]
                ));
            }
            continue;
        }
        let plan = match plan {
            Ok(plan) => plan,
            Err(e) => {
                failures.push(format!("{}: {e}", p[0]));
                continue;
            }
        };
        let want = Expected {
            nx: p[5].parse().unwrap(),
            ny: p[6].parse().unwrap(),
            gt: [
                f64_bits(p[7]),
                f64_bits(p[8]),
                f64_bits(p[9]),
                f64_bits(p[10]),
            ],
            valid: p[11].parse().unwrap(),
            digest: p[12].to_string(),
        };
        for problem in run(&plan, &input.cells, input.nodata, &want) {
            failures.push(format!("{}: {problem}", p[0]));
        }
    }
    assert_eq!(count, P44_CASES, "the fixture must hold every case");
    assert_eq!(refused, P44_REFUSALS, "the fixture's refusals");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The source grid of a P41 row. P41's rows state parameters; the WKTs are Vortex's, from inputs.txt.
fn p41_source(inputs: &HashMap<String, Input>, product: &str, row: &[&str]) -> SourceGrid {
    let f = |i: usize| f64_bits(row[i]);
    SourceGrid {
        nx: row[4].parse().unwrap(),
        ny: row[5].parse().unwrap(),
        transform: GeoTransform::axis_aligned(f(6), f(7), f(8), f(9)),
        crs: inputs[&format!("crop_{product}_wet")].source.crs.clone(),
    }
}

#[test]
fn the_60_p41_cases_give_vortex_bits_through_the_production_warp() {
    let inputs = load_inputs();
    let text = fs::read_to_string(dir("vortex_p41").join("cases.txt")).unwrap();
    let mut data: HashMap<String, Vec<f32>> = HashMap::new();
    let (mut count, mut cells_seen, mut failures) = (0, 0usize, Vec::new());
    let mut seen = HashSet::new();
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let p: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(p.len(), 25, "bad case row: {line}");
        assert!(seen.insert(p[0]), "{} is listed twice", p[0]);
        let cells = data
            .entry(p[1].to_string())
            .or_insert_with(|| decompress(dir("vortex_p41").join(format!("{}.f32.zst", p[1]))));
        let f = |i: usize| f64_bits(p[i]);
        let (te, tr) = ([f(11), f(12), f(13), f(14)], f(15));
        let envelope = Envelope {
            min_x: te[0],
            min_y: te[1],
            max_x: te[2],
            max_y: te[3],
        };
        let mut spec = spec(method(p[2]), Some(envelope), Some(tr));
        // A snapped envelope with a cell size needs no alignment option.
        spec.align = Align::Refuse;
        let source = p41_source(&inputs, p[1], &p);
        let plan = WarpPlan::new(&source, &spec).unwrap();
        let total: usize = p[22].parse().unwrap();
        let nx = ((te[2] - te[0]) / tr + 0.5) as usize;
        let want = Expected {
            nx,
            ny: total / nx,
            gt: [te[0], tr, te[3], -tr],
            valid: p[23].parse().unwrap(),
            digest: p[24].to_string(),
        };
        let nodata = f(10) as f32;
        for problem in run(&plan, cells, nodata, &want) {
            failures.push(format!("{}: {problem}", p[0]));
        }
        count += 1;
        cells_seen += total;
        // Thread count must not change a bit; the large windows are the ones that split.
        if p[0].contains(".large.") {
            let mut threaded = spec.clone();
            threaded.threads = 4;
            let plan = WarpPlan::new(&source, &threaded).unwrap();
            for problem in run(&plan, cells, nodata, &want) {
                failures.push(format!("{} on 4 threads: {problem}", p[0]));
            }
        }
    }
    assert_eq!(count, P41_CASES, "the fixture must hold all 60 cases");
    assert_eq!(cells_seen, P41_CELLS);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// The comparison must be able to fail. Each control disturbs a result the way a real defect would.

fn good_result() -> (Expected, Vec<f32>) {
    let mut out: Vec<f32> = (0..12).map(|i| i as f32 * 0.5).collect();
    out[3] = f32::NAN;
    let (valid, digest) = digest(&out);
    let want = Expected {
        nx: 4,
        ny: 3,
        gt: [0.0, 2000.0, 4000.0, -2000.0],
        valid,
        digest,
    };
    (want, out)
}

fn check(want: &Expected, out: &[f32]) -> Vec<String> {
    differences(want, want.nx, want.ny, want.gt, out)
}

#[test]
fn control_the_undisturbed_result_passes() {
    let (want, out) = good_result();
    assert!(check(&want, &out).is_empty());
}

#[test]
fn control_one_cell_one_ulp_fails() {
    let (want, mut out) = good_result();
    out[7] = f32::from_bits(out[7].to_bits() + 1);
    assert!(check(&want, &out).iter().any(|p| p.contains("digest")));
}

#[test]
fn control_signed_zero_fails() {
    let (want, mut out) = good_result();
    out[0] = -0.0;
    assert!(check(&want, &out).iter().any(|p| p.contains("digest")));
}

#[test]
fn control_a_mask_change_fails_either_way() {
    let (want, mut out) = good_result();
    out[7] = f32::NAN;
    assert!(check(&want, &out).iter().any(|p| p.contains("valid")));
    let (want, mut out) = good_result();
    out[3] = 0.0;
    assert!(check(&want, &out).iter().any(|p| p.contains("valid")));
}

#[test]
fn control_a_marker_in_place_of_nan_fails() {
    // Both mean missing downstream, and they are different bits here.
    let (want, mut out) = good_result();
    out[3] = -1.0;
    assert!(!check(&want, &out).is_empty());
}

#[test]
fn control_an_extra_or_missing_time_step_fails() {
    let (want, out) = good_result();
    let mut two = out.clone();
    two.extend_from_slice(&out);
    assert!(check(&want, &two).iter().any(|p| p.contains("cells")));
    assert!(check(&want, &out[..6]).iter().any(|p| p.contains("cells")));
}

#[test]
fn control_a_moved_or_resized_grid_fails() {
    let (want, out) = good_result();
    let mut gt = want.gt;
    gt[0] = f64::from_bits(gt[0].to_bits() + 1);
    assert!(!differences(&want, want.nx, want.ny, gt, &out).is_empty());
    assert!(!differences(&want, 3, 4, want.gt, &out).is_empty());
}

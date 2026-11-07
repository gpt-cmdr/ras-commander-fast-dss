//! Maintainer decision, "Platform parity": the value path must not call platform maths,
//! including `f64::sin` and friends, because Linux and macOS must match the Windows reference
//! bit for bit. Under `src/crs/`, all maths goes through `crs/maths.rs`.

use std::fs;
use std::path::Path;

/// Methods that reach the platform's libm, and the crates that do so inside.
const FORBIDDEN: &[&str] = &[
    ".sin()",
    ".cos()",
    ".tan()",
    ".asin()",
    ".acos()",
    ".atan()",
    ".atan2(",
    ".sinh()",
    ".cosh()",
    ".tanh()",
    ".sin_cos()",
    ".exp()",
    ".exp2()",
    ".exp_m1()",
    ".ln()",
    ".ln_1p()",
    ".log(",
    ".log10()",
    ".log2()",
    ".powf(",
    ".powi(",
    ".hypot(",
    ".cbrt()",
    ".to_radians()",
    ".to_degrees()",
    "proj4rs",
    "libm::",
];

fn offences(source: &str) -> Vec<&'static str> {
    let code: String = source
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    FORBIDDEN
        .iter()
        .copied()
        .filter(|f| code.contains(f))
        .collect()
}

#[test]
fn crs_sources_reach_maths_only_through_the_one_module() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/crs");
    let mut checked = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() == "maths.rs" {
            continue;
        }
        let found = offences(&fs::read_to_string(&path).unwrap());
        assert!(found.is_empty(), "{}: {found:?}", path.display());
        checked += 1;
    }
    assert!(checked >= 5, "only {checked} files checked");
}

#[test]
fn the_gate_detects_platform_maths() {
    assert_eq!(offences("let x = y.sin();"), [".sin()"]);
    assert_eq!(offences("let x = y.powf(2.0);"), [".powf("]);
    assert_eq!(offences("let x = libm::sin(y);"), ["libm::"]);
    assert!(offences("// y.sin() in a comment\nlet x = m::sin(y);").is_empty());
}

fn shipped_platform_paths(maths: &str, manifest: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = offences(maths);
    // Selecting another implementation is how a platform path would come back.
    if maths.contains("cfg(") || maths.contains("cfg!(") {
        found.push("cfg");
    }
    if manifest.contains("platform-maths") {
        found.push("feature");
    }
    found
}

/// Nothing in the shipped crate can select platform maths, so no build of it, wheel or not, can
/// include it. Evidence runs replace the file in a copy outside the workspace.
#[test]
fn the_shipped_crate_has_no_platform_maths_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let maths = fs::read_to_string(root.join("src/crs/maths.rs")).unwrap();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(
        shipped_platform_paths(&maths, &manifest).is_empty(),
        "{:?}",
        shipped_platform_paths(&maths, &manifest)
    );
    assert!(
        !fs::read_to_string(root.join("../../Cargo.toml"))
            .unwrap()
            .contains("crs_evidence"),
        "the evidence copy must stay outside the workspace"
    );
}

#[test]
fn the_shipped_path_check_detects_a_platform_path() {
    let clean = "pub fn sin(x: f64) -> f64 { maths::sin(x) }";
    assert!(shipped_platform_paths(clean, "").is_empty());
    assert_eq!(
        shipped_platform_paths("pub fn sin(x: f64) -> f64 { libm::sin(x) }", ""),
        ["libm::"]
    );
    assert_eq!(
        shipped_platform_paths("pub fn sin(x: f64) -> f64 { x.sin() }", ""),
        [".sin()"]
    );
    assert_eq!(
        shipped_platform_paths("#[cfg(feature = \"x\")] fn f() {}", ""),
        ["cfg"]
    );
    assert_eq!(
        shipped_platform_paths(clean, "platform-maths-evidence = []"),
        ["feature"]
    );
}

/// `crs::maths::floor` is written out, so it is compared with the exact result.
#[test]
fn floor_is_exact_including_its_signs_and_the_edges() {
    use dss_vortex::crs::maths::floor;
    let two52 = 4503599627370496.0_f64;
    let mut values = vec![
        0.0,
        -0.0,
        0.5,
        -0.5,
        1.0,
        -1.0,
        1.5,
        -1.5,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        5e-324,
        -5e-324,
        two52,
        -two52,
        two52 - 1.0,
        1.0 - two52,
        two52 - 0.5,
        0.5 - two52,
        two52 / 2.0 + 0.5,
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    let mut bits = 0x9E37_79B9_7F4A_7C15_u64;
    for _ in 0..200_000 {
        bits ^= bits << 13;
        bits ^= bits >> 7;
        bits ^= bits << 17;
        values.push(f64::from_bits(bits));
        values.push((bits as i64 >> 12) as f64 / 1024.0);
    }
    for v in values {
        if !v.is_nan() {
            assert_eq!(floor(v).to_bits(), v.floor().to_bits(), "{v:e}");
        }
    }
    assert!(floor(f64::NAN).is_nan());
}

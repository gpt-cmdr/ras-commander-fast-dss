//! Maintainer decision, "Platform parity": under `src/warp/`, transcendental maths goes through
//! the one interface in `crs/maths.rs`. The warp itself needs none: its only maths is
//! exact (`floor`, `ceil`, `trunc`, `abs`, `sqrt`, comparisons), identical on every platform.

use std::fs;
use std::path::Path;

const FORBIDDEN: &[&str] = &[
    ".sin",
    ".cos",
    ".tan",
    ".asin",
    ".acos",
    ".atan",
    ".sinh",
    ".cosh",
    ".tanh",
    ".exp",
    ".ln",
    ".log",
    ".powf",
    ".powi",
    ".hypot",
    ".cbrt",
    ".to_radians",
    ".to_degrees",
    "f64::sin",
    "f64::cos",
    "f64::tan",
    "f64::ln",
    "f64::exp",
    "f64::powf",
    "libm::",
    "mul_add",
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
        .filter(|f| {
            code.match_indices(f).any(|(i, m)| {
                // ".sin" must be a whole method name, so ".single" is not an offence.
                !code[i + m.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        })
        .collect()
}

#[test]
fn warp_sources_use_no_platform_maths() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/warp");
    let mut checked = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let found = offences(&fs::read_to_string(&path).unwrap());
        assert!(found.is_empty(), "{}: {found:?}", path.display());
        checked += 1;
    }
    assert_eq!(checked, 5, "the gate must see every warp source file");
}

#[test]
fn the_gate_detects_platform_maths() {
    assert_eq!(offences("let x = y.sin();"), [".sin"]);
    assert_eq!(offences("let x = y.powf (2.0);"), [".powf"]);
    assert_eq!(offences("let f = f64::tan;"), ["f64::tan"]);
    assert!(offences("// y.sin() in a comment\nlet x = y.floor();").is_empty());
    assert!(offences("let a = b.single();").is_empty());
}

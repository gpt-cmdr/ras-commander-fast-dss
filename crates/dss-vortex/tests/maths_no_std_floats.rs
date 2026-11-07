//! Platform parity: value-affecting code in this crate must not reach the platform maths library.
//!
//! **This is a text scan with known gaps. It is a guard against easy mistakes, not proof.** It
//! reads this crate's own `src`, with comments and strings removed and whitespace closed up. It
//! catches method calls, `f64::sin`-style paths used as calls or values, `<f64>::sin`, `libm`
//! anywhere, naming `uncertified` outside `maths`, and `mul_add` outside `maths/fused.rs`.
//!
//! Known gaps: dependencies and generated code are not scanned, so a third-party crate that calls
//! std floats (`proj4rs` does) passes; an alias such as `type F = f64; F::sin` passes; a trait
//! method such as `num_traits::Float::sin` passes; and code a macro assembles from pieces passes.
//!
//! `sqrt`, `abs`, `floor`, `ceil`, `round` and `trunc` are exact by IEEE 754 and are allowed.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "sinh", "cosh", "tanh", "asinh", "acosh",
    "atanh", "sin_cos", "exp", "exp2", "exp_m1", "ln", "ln_1p", "log", "log2", "log10", "powf",
    "powi", "hypot", "cbrt",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Source with comments, string contents and all whitespace removed.
fn squeeze(source: &str) -> String {
    let b: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            '/' if b.get(i + 1) == Some(&'/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            '"' => {
                i += 1;
                while i < b.len() && b[i] != '"' {
                    i += if b[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
                out.push('"');
            }
            _ if b[i].is_whitespace() => {
                // Keep `use libm` two words; everything else closes up.
                while i < b.len() && b[i].is_whitespace() {
                    i += 1;
                }
                let next = b.get(i).copied();
                if out.chars().next_back().is_some_and(is_ident) && next.is_some_and(is_ident) {
                    out.push(' ');
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `needle` occurs in `code` as a whole word at its end, and at its start when
/// `word_start` is set.
fn has_word(code: &str, needle: &str, word_start: bool) -> bool {
    code.match_indices(needle).any(|(at, _)| {
        let after = code[at + needle.len()..].chars().next();
        let before = code[..at].chars().next_back();
        after.is_none_or(|c| !is_ident(c)) && (!word_start || before.is_none_or(|c| !is_ident(c)))
    })
}

/// Every forbidden use in one file's squeezed source.
fn violations(code: &str, in_maths: bool, is_fused: bool) -> Vec<String> {
    let mut found = Vec::new();
    for name in FORBIDDEN {
        for (prefix, word_start) in [
            (".", false),
            ("f64::", true),
            ("f32::", true),
            (">::", false),
        ] {
            let needle = format!("{prefix}{name}");
            if has_word(code, &needle, word_start) {
                found.push(needle);
            }
        }
    }
    if has_word(code, "libm", true) {
        found.push("libm".to_string());
    }
    if !in_maths && code.contains("uncertified") {
        found.push("uncertified".to_string());
    }
    if !is_fused && code.contains(".mul_add") {
        found.push("mul_add".to_string());
    }
    found
}

fn scan(source: &str) -> Vec<String> {
    violations(&squeeze(source), false, false)
}

#[test]
fn the_scanner_catches_each_form() {
    for (bad, what) in [
        ("let a = x.sin();", ".sin"),
        ("let a = x.sin ();", ".sin"),
        ("let a = x\n    .sin\n    ();", ".sin"),
        ("let a = f64::powf(x, y);", "f64::powf"),
        ("let f = f64::sin; let z = f(x);", "f64::sin"),
        ("let f = f64 :: sin;", "f64::sin"),
        ("let a = <f64>::exp(x);", ">::exp"),
        ("let a = libm::exp(x);", "libm"),
        ("use libm::sin;", "libm"),
        ("use libm as m;", "libm"),
        ("let a = maths::uncertified::exp(x);", "uncertified"),
        ("let a = x.mul_add(y, z);", "mul_add"),
    ] {
        assert!(
            scan(bad).iter().any(|v| v == what),
            "{bad} gave {:?}",
            scan(bad)
        );
    }
    for good in [
        "let a = maths::sin(x); // x.sin() is banned",
        "let a = x.sqrt();",
        "/* f64::sin */ let b = 1;",
        "let s = \"x.sin()\";",
        "let a = maths::log(x) + my_exp;",
        "let a = x.sine();",
    ] {
        assert!(scan(good).is_empty(), "{good} gave {:?}", scan(good));
    }
}

#[test]
fn no_value_affecting_code_calls_std_float_transcendentals() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(files.len() > 3, "scanned too little: {files:?}");
    let mut bad = Vec::new();
    for file in files {
        let in_maths = file.components().any(|c| c.as_os_str() == "maths");
        let is_fused = file.file_name().is_some_and(|n| n == "fused.rs");
        let code = squeeze(&fs::read_to_string(&file).unwrap());
        for name in violations(&code, in_maths, is_fused) {
            bad.push(format!("{}: {name}", file.display()));
        }
    }
    assert!(
        bad.is_empty(),
        "use crate::maths instead:\n{}",
        bad.join("\n")
    );
}

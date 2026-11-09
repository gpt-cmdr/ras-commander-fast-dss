//! "No C in the build graph", with exact pins and a locked, checksummed graph.
//!
//! Reads `Cargo.lock`, which records every platform and every enabled feature, so a C
//! dependency on any target fails here, including one this host would not build.
//! `build_time_code.rs` reads what build scripts actually do.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Crates that compile or locate native code at build time.
const NATIVE_BUILD_TOOLS: &[&str] = &[
    "autotools",
    "bindgen",
    "cc",
    "cmake",
    "cxx",
    "cxx-build",
    "meson",
    "nasm-rs",
    "pkg-config",
    "vcpkg",
];

/// `-sys` by name, but declarations only: no native source and no compiler.
const PURE_SYS_CRATES: &[&str] = &["js-sys", "linux-raw-sys", "web-sys", "windows-sys"];

/// The graphs that ship: this crate, and the wheel with its `vortex` feature on.
const SHIPPED_ROOTS: &[&str] = &["dss-vortex", "dss-py", "dss-core"];

#[derive(Debug, Default)]
struct Package {
    name: String,
    version: String,
    registry: bool,
    checksum: bool,
    dependencies: Vec<String>,
}

fn parse_lock(text: &str) -> Vec<Package> {
    let mut packages = Vec::new();
    let mut in_dependencies = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            packages.push(Package::default());
            in_dependencies = false;
        } else if line == "dependencies = [" {
            in_dependencies = true;
        } else if line == "]" {
            in_dependencies = false;
        } else if let Some(p) = packages.last_mut() {
            if in_dependencies {
                p.dependencies
                    .push(line.trim_matches(|c| c == '"' || c == ',').to_string());
            } else if let Some(v) = line.strip_prefix("name = ") {
                p.name = v.trim_matches('"').to_string();
            } else if let Some(v) = line.strip_prefix("version = ") {
                p.version = v.trim_matches('"').to_string();
            } else if line.starts_with("source = ") {
                p.registry = true;
            } else if line.starts_with("checksum = ") {
                p.checksum = true;
            }
        }
    }
    packages
}

/// Package indices reachable from `roots`. A lock entry is `name` or `name version`.
fn reachable(packages: &[Package], roots: &[&str]) -> BTreeSet<usize> {
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, p) in packages.iter().enumerate() {
        by_name.entry(&p.name).or_default().push(i);
    }
    let mut seen = BTreeSet::new();
    let mut stack: Vec<usize> = roots
        .iter()
        .flat_map(|r| by_name.get(r).cloned().unwrap_or_default())
        .collect();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        for dep in &packages[i].dependencies {
            let mut words = dep.split(' ');
            let name = words.next().unwrap_or_default();
            let version = words.next();
            for &j in by_name.get(name).map(Vec::as_slice).unwrap_or_default() {
                if version.is_none_or(|v| v == packages[j].version) {
                    stack.push(j);
                }
            }
        }
    }
    seen
}

fn native_dependencies(lock: &str, roots: &[&str]) -> Vec<String> {
    let packages = parse_lock(lock);
    let mut found: Vec<String> = reachable(&packages, roots)
        .into_iter()
        .map(|i| packages[i].name.clone())
        .filter(|n| {
            NATIVE_BUILD_TOOLS.contains(&n.as_str())
                || (n.ends_with("-sys") && !PURE_SYS_CRATES.contains(&n.as_str()))
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn real_lock() -> String {
    fs::read_to_string(workspace_root().join("Cargo.lock")).unwrap()
}

#[test]
fn shipped_graphs_have_no_native_build_dependency() {
    let found = native_dependencies(&real_lock(), SHIPPED_ROOTS);
    assert!(found.is_empty(), "C in the build graph: {found:?}");
}

#[test]
fn the_check_detects_each_kind_of_native_dependency() {
    for native in ["libz-sys", "cc", "cmake", "bindgen", "pkg-config"] {
        let lock = format!(
            "[[package]]\nname = \"dss-vortex\"\nversion = \"0.0.1\"\ndependencies = [\n \"mid 1.0.0\",\n]\n\n\
             [[package]]\nname = \"mid\"\nversion = \"1.0.0\"\ndependencies = [\n \"{native}\",\n]\n\n\
             [[package]]\nname = \"mid\"\nversion = \"2.0.0\"\n\n\
             [[package]]\nname = \"{native}\"\nversion = \"1.0.0\"\n"
        );
        assert_eq!(native_dependencies(&lock, SHIPPED_ROOTS), [native]);
    }
}

#[test]
fn the_check_follows_versions_and_ignores_unreachable_and_pure_crates() {
    let lock = "[[package]]\nname = \"dss-vortex\"\nversion = \"0.0.1\"\ndependencies = [\n \"js-sys\",\n \"mid 2.0.0\",\n]\n\n\
         [[package]]\nname = \"js-sys\"\nversion = \"0.3.0\"\n\n\
         [[package]]\nname = \"mid\"\nversion = \"1.0.0\"\ndependencies = [\n \"cc\",\n]\n\n\
         [[package]]\nname = \"mid\"\nversion = \"2.0.0\"\n\n\
         [[package]]\nname = \"cc\"\nversion = \"1.0.0\"\n";
    assert!(native_dependencies(lock, SHIPPED_ROOTS).is_empty());
}

#[test]
fn every_registry_crate_has_a_lock_checksum() {
    for p in parse_lock(&real_lock()) {
        assert!(
            !p.registry || p.checksum,
            "{} {} has no checksum in Cargo.lock",
            p.name,
            p.version
        );
    }
}

#[test]
fn direct_dependencies_are_pinned_exactly() {
    let manifest = fs::read_to_string(workspace_root().join("crates/dss-vortex/Cargo.toml"));
    let manifest = manifest.unwrap().replace('\r', "");
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .split("\n[")
        .next()
        .unwrap();
    let mut checked = 0;
    for line in dependencies.lines().filter(|l| !l.trim().is_empty()) {
        if line.starts_with("dss-core") {
            continue;
        }
        assert!(line.contains("version = \"="), "not pinned: {line}");
        checked += 1;
    }
    assert!(checked >= 6, "only {checked} direct dependencies found");
}

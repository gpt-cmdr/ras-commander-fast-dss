//! No C compilation hides in build-time code.
//!
//! `build_graph.rs` checks crate names. This test reads the code that runs during a build:
//! every build script, every proc macro, and everything those reach, in the graphs of
//! `dss-vortex`, `dss-core` and `dss-py --all-features` on the targets we ship. It fails
//! closed. A crate with a build script, or with a spawn or link directive, must be listed in
//! `REVIEWED` with the exact checksum in `Cargo.lock`, so a new crate, a new version or a
//! local patch needs a fresh review before this passes.

mod reviewed;

use reviewed::{Reviewed, INERT_FILES, REVIEWED};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHIPPED_TARGETS: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];
const SHIPPED_ROOTS: &[&str] = &["dss-vortex", "dss-core", "dss-py"];

const SPAWNS: &[&str] = &[
    "Command::new",
    "process::Command",
    "process::{",
    "use std::process",
    "process as ",
    "CreateProcess",
    "posix_spawn",
    "execvp",
];
const LINK_DIRECTIVES: &[&str] = &[
    "rustc-link-lib",
    "rustc-link-search",
    "rustc-link-arg",
    "rustc-cdylib-link-arg",
    "rustc-flags",
];
/// Quoted names that fail a file that also spawns a process, with no allowlist.
const COMPILERS: &[&str] = &[
    "\"cc\"",
    "\"c++\"",
    "\"gcc\"",
    "\"g++\"",
    "\"clang\"",
    "\"clang++\"",
    "\"clang-cl\"",
    "\"cl\"",
    "\"cl.exe\"",
    "\"CC\"",
    "\"CXX\"",
    "\"gfortran\"",
    "\"nasm\"",
    "\"yasm\"",
    "\"cmake\"",
    "\"make\"",
    "\"nmake\"",
    "\"msbuild\"",
    "\"meson\"",
    "\"ninja\"",
    "\"ar\"",
    "\"lib.exe\"",
    "\"link.exe\"",
];
const NATIVE_FILES: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hpp", "s", "asm", "f", "f90", "lib", "a", "o", "obj", "dll",
    "so", "dylib",
];

struct Package {
    name: String,
    version: String,
    registry: bool,
    root: PathBuf,
    links: Option<String>,
    build_script: Option<PathBuf>,
    /// A proc macro, or reached from one or from a build script: its library runs during the
    /// build.
    build_time: bool,
    /// Reached through normal edges from a root. Conservative for the licence bundle: this
    /// includes proc macros and their helpers, which do not end up in the artifact.
    linked: bool,
}

/// The packages reachable from `roots` through normal and build edges, on `targets`.
fn load(workspace: &Path, roots: &[&str], targets: &[&str], extra: &[&str]) -> Vec<Package> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(workspace)
        .args(["metadata", "--format-version", "1", "--locked"])
        .args(["--all-features"])
        .args(extra);
    for target in targets {
        command.args(["--filter-platform", target]);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();

    let packages = metadata["packages"].as_array().unwrap();
    let by_id: BTreeMap<&str, &Value> = packages
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p))
        .collect();
    let nodes: BTreeMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();

    let is_proc_macro = |p: &Value| {
        let targets = p["targets"].as_array().unwrap();
        targets.iter().any(|t| {
            t["kind"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "proc-macro")
        })
    };
    // (id, runs at compile time, reached by normal edges alone)
    let mut stack: Vec<(&str, bool, bool)> = packages
        .iter()
        .filter(|p| roots.contains(&p["name"].as_str().unwrap()))
        .map(|p| (p["id"].as_str().unwrap(), false, true))
        .collect();
    let mut seen: BTreeSet<(&str, bool, bool)> = BTreeSet::new();
    while let Some((id, build_time, normal)) = stack.pop() {
        // A proc macro runs in the compiler, and so does everything it reaches.
        let build_time = build_time || is_proc_macro(by_id[id]);
        if !seen.insert((id, build_time, normal)) {
            continue;
        }
        let Some(node) = nodes.get(id) else { continue };
        for dep in node["deps"].as_array().unwrap() {
            let dep_id = dep["pkg"].as_str().unwrap();
            let kinds = dep["dep_kinds"].as_array().unwrap();
            if kinds.iter().any(|k| k["kind"].is_null()) {
                stack.push((dep_id, build_time, normal));
            }
            if kinds.iter().any(|k| k["kind"] == "build") {
                stack.push((dep_id, true, false));
            }
        }
    }

    let reached: BTreeSet<&str> = seen.iter().map(|(id, _, _)| *id).collect();
    reached
        .into_iter()
        .map(|id| {
            let p = by_id[id];
            let targets = p["targets"].as_array().unwrap();
            let has_kind =
                |t: &Value, kind: &str| t["kind"].as_array().unwrap().iter().any(|k| k == kind);
            let build_script = targets
                .iter()
                .find(|t| has_kind(t, "custom-build"))
                .map(|t| PathBuf::from(t["src_path"].as_str().unwrap()));
            Package {
                name: p["name"].as_str().unwrap().to_string(),
                version: p["version"].as_str().unwrap().to_string(),
                registry: p["source"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("registry+")),
                root: Path::new(p["manifest_path"].as_str().unwrap())
                    .parent()
                    .unwrap()
                    .to_path_buf(),
                links: p["links"].as_str().map(str::to_string),
                build_time: seen.iter().any(|&(i, b, _)| i == id && b),
                linked: seen.iter().any(|&(i, _, n)| i == id && n),
                build_script,
            }
        })
        .collect()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The Rust files that run at build time for this package.
fn build_time_sources(p: &Package) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(script) = &p.build_script {
        let dir = script.parent().unwrap_or(&p.root);
        if dir == p.root {
            // build.rs at the root may use sibling files and a build/ directory.
            files.extend(
                fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|f| f.is_file()),
            );
            walk(&p.root.join("build"), &mut files);
        } else {
            walk(dir, &mut files);
        }
    }
    if p.build_time {
        // The whole package, since a library need not live under src/.
        let mut package = Vec::new();
        walk(&p.root, &mut package);
        package.retain(|f| {
            let inner = f.strip_prefix(&p.root).unwrap_or(f);
            let top = inner.components().next().map(|c| c.as_os_str());
            !["tests", "benches", "examples"]
                .iter()
                .any(|d| top == Some(std::ffi::OsStr::new(d)))
        });
        files.extend(package);
    }
    files.retain(|f| f.extension().is_some_and(|e| e == "rs"));
    files
}

#[derive(Debug, Default, PartialEq)]
struct Findings {
    spawns: bool,
    link_directives: bool,
    compilers: Vec<String>,
}

fn scan(files: &[PathBuf]) -> Findings {
    let mut findings = Findings::default();
    for file in files {
        let Ok(bytes) = fs::read(file) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let code: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        let has = |needles: &[&str]| code.iter().any(|l| needles.iter().any(|n| l.contains(n)));
        let spawns = has(SPAWNS);
        findings.spawns |= spawns;
        findings.link_directives |= has(LINK_DIRECTIVES);
        if spawns {
            for name in COMPILERS {
                if code.iter().any(|l| l.contains(name)) {
                    findings
                        .compilers
                        .push(format!("{name} in {}", file.display()));
                }
            }
        }
    }
    findings
}

fn relative(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.to_string_lossy().replace('\\', "/")
}

/// Every problem in the graph. Empty means the graph matches its review.
fn review(packages: &[Package], lock: &str, table: &[Reviewed]) -> Vec<String> {
    let mut problems = Vec::new();
    let checksums = lock_checksums(lock);
    let mut used = BTreeSet::new();
    for p in packages {
        let id = format!("{} {}", p.name, p.version);
        let entry = table
            .iter()
            .find(|r| r.name == p.name && r.version == p.version);
        if let Some(entry) = entry {
            used.insert((entry.name, entry.version));
        }
        let mut files = Vec::new();
        walk(&p.root, &mut files);
        for file in &files {
            let extension = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            let path = format!("{}/{}", id.replace(' ', "-"), relative(file, &p.root));
            if NATIVE_FILES.contains(&extension.to_ascii_lowercase().as_str())
                && !INERT_FILES.contains(&path.as_str())
            {
                problems.push(format!("{id}: native file {path}"));
            }
        }
        let findings = scan(&build_time_sources(p));
        for compiler in &findings.compilers {
            problems.push(format!("{id}: spawns a compiler: {compiler}"));
        }
        let needs_review = p.build_script.is_some()
            || p.links.is_some()
            || findings.spawns
            || findings.link_directives;
        if !needs_review {
            continue;
        }
        let Some(entry) = entry else {
            problems.push(format!(
                "{id}: build-time code is not reviewed (build script {}, links {:?}, spawns {}, link directives {})",
                p.build_script.is_some(),
                p.links,
                findings.spawns,
                findings.link_directives
            ));
            continue;
        };
        if !p.registry
            || checksums.get(&(p.name.as_str(), p.version.as_str())) != Some(&entry.checksum)
        {
            problems.push(format!("{id}: source differs from the reviewed checksum"));
        }
        if findings.spawns && entry.spawns.is_none() {
            problems.push(format!("{id}: spawns a process; not reviewed"));
        }
        if findings.link_directives && entry.link_directives.is_none() {
            problems.push(format!("{id}: emits link directives; not reviewed"));
        }
        if p.links.as_deref() != entry.links {
            problems.push(format!(
                "{id}: links {:?}; reviewed {:?}",
                p.links, entry.links
            ));
        }
    }
    for r in table {
        if r.purpose.trim().is_empty() {
            problems.push(format!("{} {}: no stated purpose", r.name, r.version));
        }
        if !used.contains(&(r.name, r.version)) {
            problems.push(format!(
                "{} {}: reviewed but not in the graph",
                r.name, r.version
            ));
        }
    }
    problems
}

fn lock_checksums(lock: &str) -> BTreeMap<(&str, &str), &str> {
    let mut map = BTreeMap::new();
    let (mut name, mut version) = ("", "");
    for line in lock.lines() {
        let value = |prefix: &str| {
            line.strip_prefix(prefix)
                .map(|v| v.trim().trim_matches('"'))
        };
        if let Some(v) = value("name = ") {
            name = v;
        } else if let Some(v) = value("version = ") {
            version = v;
        } else if let Some(v) = value("checksum = ") {
            map.insert((name, version), v);
        }
    }
    map
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn shipped_build_time_code_matches_its_review() {
    let root = workspace_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let packages = load(&root, SHIPPED_ROOTS, SHIPPED_TARGETS, &[]);
    assert!(
        packages.iter().any(|p| p.name == "hayro-jpeg2000"),
        "vortex graph missing"
    );
    let problems = review(&packages, &lock, REVIEWED);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// A real Cargo workspace whose path dependency has `build_rs` as its build script. Neither
/// its name nor its manifest says anything native.
fn synthetic(name: &str, build_rs: &str) -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    let helper = dir.join("decoder-helper");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(helper.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ndecoder-helper = { path = \"decoder-helper\" }\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(dir.join("src/lib.rs"), "").unwrap();
    fs::write(
        helper.join("Cargo.toml"),
        "[package]\nname = \"decoder-helper\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(helper.join("src/lib.rs"), "").unwrap();
    fs::write(helper.join("build.rs"), build_rs).unwrap();
    fs::write(
        dir.join("Cargo.lock"),
        "version = 3\n\n[[package]]\nname = \"decoder-helper\"\nversion = \"1.0.0\"\n\n\
         [[package]]\nname = \"root\"\nversion = \"0.0.0\"\ndependencies = [\n \"decoder-helper\",\n]\n",
    )
    .unwrap();
    let packages = load(&dir, &["root"], SHIPPED_TARGETS, &["--offline"]);
    review(&packages, "", &[])
}

#[test]
fn a_build_script_spawning_a_compiler_fails() {
    let problems = synthetic(
        "spawns_compiler",
        r#"fn main() {
            std::fs::write("x.c", "int x;").unwrap();
            std::process::Command::new("clang").arg("x.c").status().unwrap();
            println!("cargo:rustc-link-lib=static=x");
        }"#,
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("spawns a compiler") && p.contains("clang")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not reviewed") && p.contains("link directives true")),
        "{problems:?}"
    );
}

#[test]
fn a_compiler_named_through_a_variable_still_fails_as_unreviewed() {
    let problems = synthetic(
        "indirect_compiler",
        r#"fn main() {
            let tool = std::env::var("TOOL").unwrap();
            std::process::Command::new(tool).status().unwrap();
        }"#,
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not reviewed") && p.contains("spawns true")),
        "{problems:?}"
    );
}

#[test]
fn any_new_build_script_fails_even_an_innocent_one() {
    let problems = synthetic("innocent", "fn main() {}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("decoder-helper 1.0.0") && p.contains("not reviewed")),
        "{problems:?}"
    );
}

/// In-tree through `[patch.crates-io]`, with CLB's patches.
const VENDORED: &[&str] = &["hdf5-reader", "netcdf-reader"];

#[test]
fn the_vendored_readers_keep_their_licences_and_add_no_build_step() {
    let root = workspace_root();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let patch = manifest.split("[patch.crates-io]").nth(1).unwrap();
    let listed: Vec<&str> = patch
        .lines()
        .filter_map(|line| line.split(" = ").next())
        .filter(|name| !name.trim().is_empty())
        .collect();
    assert_eq!(listed, VENDORED, "only the readers are patched");
    for crate_name in VENDORED {
        let dir = root.join("vendor").join(crate_name);
        for licence in ["LICENSE-MIT", "LICENSE-APACHE"] {
            assert!(dir.join(licence).is_file(), "{crate_name} lacks {licence}");
        }
        assert!(
            !dir.join("build.rs").exists(),
            "{crate_name} has a build.rs"
        );
        let toml = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("license = \"MIT OR Apache-2.0\""),
            "{crate_name}"
        );
        assert!(toml.contains("version = \"0.12.1\""), "{crate_name}");
    }
    let patches = fs::read_to_string(root.join("vendor/PATCHES.md")).unwrap();
    assert!(patches.contains("| # |"), "the record of changes is there");
}

#[test]
fn the_wheel_licence_bundle_lists_exactly_the_linked_crates() {
    let root = workspace_root();
    let packages = load(&root, &["dss-py"], SHIPPED_TARGETS, &[]);
    let linked: BTreeSet<String> = packages
        .iter()
        .filter(|p| p.linked && (p.registry || VENDORED.contains(&p.name.as_str())))
        .map(|p| format!("{} {}", p.name, p.version))
        .collect();
    let bundle = root.join("python/ras_commander_fast_dss/THIRD_PARTY_LICENSES.txt");
    let bundle = fs::read_to_string(bundle).unwrap();
    // Ported source has no crate entry, so its notice must travel in the bundle: the whole
    // NOTICE, as it stands now, and THIRD_PARTY_NOTICES.md repeats it.
    let notice = fs::read_to_string(root.join("NOTICE")).unwrap();
    let notice = notice.replace("\r\n", "\n");
    let notice = notice.trim();
    assert!(
        bundle.replace("\r\n", "\n").contains(notice),
        "wheel bundle does not carry the current NOTICE; run scripts/third_party_licenses.py"
    );
    let summary = fs::read_to_string(root.join("THIRD_PARTY_NOTICES.md")).unwrap();
    assert!(
        summary.replace("\r\n", "\n").contains(notice),
        "THIRD_PARTY_NOTICES.md does not repeat the current NOTICE"
    );
    for ported in [
        "aocl-libm-ose",
        "Copyright (C) 2008-2022 Advanced Micro Devices, Inc. All rights reserved.",
        "optimized-routines",
        "Copyright (c) 1999-2022, Arm Limited.",
        "Copyright (c) 1998-2018, University Corporation for Atmospheric Research/Unidata",
        "Copyright (c) 2019 U.S. Federal Government (in countries where recognized)",
        "Copyright (c) 2000, Frank Warmerdam",
        "Copyright (c) 2003, Frank Warmerdam <warmerdam@pobox.com>",
    ] {
        assert!(
            notice.contains(ported),
            "NOTICE lacks the upstream notice text {ported:?}"
        );
    }
    let listed: BTreeSet<String> = bundle
        .lines()
        .filter_map(|l| l.strip_prefix("crate "))
        .filter_map(|l| l.split(" | ").next())
        .map(str::to_string)
        .collect();
    assert_eq!(linked, listed, "run scripts/third_party_licenses.py");
    for line in bundle.lines().filter(|l| l.starts_with("crate ")) {
        let references = line.rsplit(" | ").next().unwrap();
        let references = references.split(" (crate ships").next().unwrap();
        assert!(!references.is_empty(), "no licence text: {line}");
        for id in references.split(", ") {
            assert!(bundle.contains(&format!("Licence text {id}")), "{line}");
        }
    }
}

/// A workspace with no build script at all: root -> proc macro -> `first` -> `second`, where
/// `second` keeps its library at the package root and spawns `tool`.
fn proc_macro_chain(name: &str, spawn: &str) -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    let manifest = |name: &str, extra: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n{extra}")
    };
    let write = |path: PathBuf, text: String| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    let dep = |d: &str| format!("[dependencies]\n{d} = {{ path = \"../{d}\" }}\n");
    write(
        dir.join("Cargo.toml"),
        manifest(
            "root",
            "[dependencies]\nmacro_crate = { path = \"macro_crate\" }\n\n[workspace]\n",
        ),
    );
    write(dir.join("src/lib.rs"), String::new());
    write(
        dir.join("macro_crate/Cargo.toml"),
        manifest(
            "macro_crate",
            &format!("[lib]\nproc-macro = true\n\n{}", dep("first")),
        ),
    );
    write(dir.join("macro_crate/src/lib.rs"), String::new());
    write(
        dir.join("first/Cargo.toml"),
        manifest("first", &dep("second")),
    );
    write(dir.join("first/src/lib.rs"), String::new());
    write(
        dir.join("second/Cargo.toml"),
        manifest("second", "[lib]\npath = \"helper.rs\"\n"),
    );
    write(dir.join("second/helper.rs"), spawn.to_string());

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&dir)
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap();
    assert!(status.success());
    let packages = load(&dir, &["root"], SHIPPED_TARGETS, &["--offline"]);
    review(&packages, "", &[])
}

#[test]
fn a_helper_reached_through_a_proc_macro_spawning_a_compiler_fails() {
    let problems = proc_macro_chain(
        "proc_macro_helper",
        r#"pub fn run() { std::process::Command::new("clang").arg("x.c").status().unwrap(); }"#,
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("second 1.0.0: spawns a compiler") && p.contains("clang")),
        "{problems:?}"
    );
}

#[test]
fn a_helper_reached_through_a_proc_macro_spawning_anything_needs_review() {
    let problems = proc_macro_chain(
        "proc_macro_helper_indirect",
        r#"use std::process as p;
        pub fn run(tool: &str) { p::Command::new(tool).status().unwrap(); }"#,
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("second 1.0.0") && p.contains("not reviewed")),
        "{problems:?}"
    );
}

#[test]
fn a_changed_checksum_or_a_stale_entry_fails() {
    let root = workspace_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let packages = load(&root, SHIPPED_ROOTS, SHIPPED_TARGETS, &[]);
    let changed = lock.replacen(REVIEWED[0].checksum, "0000", 1);
    let problems = review(&packages, &changed, REVIEWED);
    assert!(
        problems.iter().any(|p| p.contains("reviewed checksum")),
        "{problems:?}"
    );
    let stale = [Reviewed {
        name: "not-in-graph",
        version: "1.0.0",
        checksum: "0",
        purpose: "test",
        links: None,
        spawns: None,
        link_directives: None,
    }];
    let problems = review(&[], "", &stale);
    assert_eq!(
        problems,
        ["not-in-graph 1.0.0: reviewed but not in the graph"]
    );
}

//! The path of a `dss-vortex` example, built by Cargo for the test that asks.
//!
//! The path comes from Cargo's own `compiler-artifact` message, not from the test
//! binary's location: Cargo 1.100.0 builds test binaries under
//! `target/<profile>/build/<pkg>/<hash>/out/`, where earlier releases used
//! `target/<profile>/deps/` (Cargo 1.100). The example is built with the test's own profile:
//! the name of the nearest directory above the test binary that holds `deps/` or `build/`
//! (`debug` is the `dev` profile). `DSS_EXAMPLE_PROFILE` overrides it. A cross-compiled
//! test (`--target`) is not supported: the example is built for the host.
//!
//! Each example is built once per test process, behind a lock: the tests of one binary
//! run in parallel threads, and a second `cargo build` re-links the uplifted executable
//! while another thread may be starting it (seen as `NotFound` on macOS).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub fn example_exe(name: &str) -> PathBuf {
    static BUILT: Mutex<BTreeMap<String, PathBuf>> = Mutex::new(BTreeMap::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    built
        .entry(name.to_string())
        .or_insert_with(|| build_example(name))
        .clone()
}

fn build_example(name: &str) -> PathBuf {
    let profile = std::env::var("DSS_EXAMPLE_PROFILE")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| test_profile(&std::env::current_exe().unwrap()));
    let out = Command::new(env!("CARGO"))
        .args(["build", "--locked", "-p", "dss-vortex", "--example", name])
        .args([
            "--profile",
            &profile,
            "--message-format=json-render-diagnostics",
        ])
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo build --example {name} --profile {profile} failed"
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| panic!("cargo reported no {name} executable"))
}

/// The Cargo profile a test binary at `exe` was built with.
fn test_profile(exe: &Path) -> String {
    let dir = exe
        .ancestors()
        .skip(1)
        .find(|a| {
            matches!(
                a.file_name().and_then(|n| n.to_str()),
                Some("deps" | "build")
            )
        })
        .and_then(|a| a.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    match dir {
        Some("debug") => "dev".into(),
        Some(other) => other.into(),
        None if cfg!(debug_assertions) => "dev".into(),
        None => "release".into(),
    }
}

#[test]
fn test_profile_reads_both_layouts() {
    let p = |s: &str| test_profile(Path::new(s));
    assert_eq!(p("/w/release/target/debug/deps/t-1"), "dev");
    assert_eq!(p("/w/target/release/deps/t-1"), "release");
    assert_eq!(
        p("/w/build/target/debug/build/dss-vortex/abc/out/t-1"),
        "dev"
    );
    assert_eq!(p("/w/target/bench2/build/dss-vortex/abc/out/t-1"), "bench2");
    assert_eq!(
        p("/w/target/x86_64-apple-darwin/release/deps/t-1"),
        "release"
    );
}

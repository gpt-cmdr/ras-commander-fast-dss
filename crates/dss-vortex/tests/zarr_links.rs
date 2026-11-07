//! A store must not be a way to read outside its folder (review of `8fc6610a`, finding 1).
//!
//! A junction or symbolic link anywhere below the store's root is refused: an array's folder, an
//! axis's folder, a parent of a `/`-separated chunk, a chunk, a metadata file. Junctions need no
//! privilege on Windows and are always tested there; symbolic links are tested on Unix, and on
//! Windows where the account may make them. Not covered: a link swapped in while a read is under
//! way, between the check of a path and its opening.

mod zarr_support;

use dss_vortex::source::zarr::ZarrStore;
use std::fs;
use std::path::Path;
use zarr_support::*;

const VARIABLE: &str = "TMP_2maboveground";

fn base(root: &Path) {
    let attrs = r#""units":"K","crs":"EPSG:4326","_FillValue":-1"#;
    write_store(
        root,
        &grid_arrays(VARIABLE, &[0.0, 1.0, 2.0, 3.0], 4, 6, attrs, &ramp(96)),
    );
}

/// A directory link that needs no privilege: a junction on Windows, a symbolic link elsewhere.
fn link_dir(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            // `mklink` reads a forward slash as a switch.
            .arg(link.to_string_lossy().replace('/', "\\"))
            .arg(target.to_string_lossy().replace('/', "\\"))
            .output()
            .unwrap();
        assert!(status.status.success(), "mklink /J: {status:?}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A file link, or `false` where this account may not make one (Windows without the privilege).
fn link_file(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link).is_ok();
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).is_ok();
}

/// A directory symbolic link, as against a junction, where the account may make one.
fn symlink_dir(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(target, link).is_ok();
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).is_ok();
}

/// What the reader says of a store: the error, or the cells it read.
fn report(store: &Path) -> String {
    let store = match ZarrStore::open(store) {
        Ok(store) => store,
        Err(error) => return format!("open: {error}"),
    };
    let mut out = format!("left_out: {:?}\n", store.left_out());
    for name in store.variable_names() {
        match store
            .variable(&name)
            .and_then(|v| v.read(0..v.step_count(), &v.full_window()))
        {
            Ok(cells) => out.push_str(&format!("READ {name}: {} cells\n", cells.len())),
            Err(error) => out.push_str(&format!("{name}: {error}\n")),
        }
    }
    out
}

/// `.zmetadata` for a store written by `write_store`, from its own per-array files.
fn consolidate(root: &Path) {
    let mut entries = vec![r#"".zgroup":{"zarr_format":2}"#.to_string()];
    for name in ["time", "latitude", "longitude", VARIABLE] {
        for file in [".zarray", ".zattrs"] {
            let text = fs::read_to_string(root.join(name).join(file)).unwrap();
            entries.push(format!("\"{name}/{file}\":{text}"));
        }
    }
    fs::write(
        root.join(".zmetadata"),
        format!(
            r#"{{"zarr_consolidated_format":1,"metadata":{{{}}}}}"#,
            entries.join(",")
        ),
    )
    .unwrap();
}

/// Move a folder of the store outside it and leave a link in its place.
fn replace_with_link(store: &Path, inside: &str, outside: &Path) {
    let from = store.join(inside);
    fs::rename(&from, outside).unwrap();
    link_dir(&from, outside);
}

fn assert_refused(store: &Path, case: &str) {
    let said = report(store);
    assert!(!said.contains("READ"), "{case}: cells were read: {said}");
    assert!(said.contains("is a link"), "{case}: {said}");
}

#[test]
fn a_linked_array_folder_is_not_read() {
    for consolidated in [true, false] {
        let root = scratch(&format!("zarr_links_array_{consolidated}"));
        let store = root.join("store.zarr");
        base(&store);
        if consolidated {
            consolidate(&store);
        }
        // The control: before the link, the same store reads.
        assert!(report(&store).contains("READ TMP_2maboveground: 96 cells"));
        replace_with_link(&store, VARIABLE, &root.join("outside"));
        assert_refused(
            &store,
            &format!("array folder, consolidated {consolidated}"),
        );
    }
}

#[test]
fn a_linked_axis_folder_is_not_read() {
    for axis in ["latitude", "time"] {
        let root = scratch(&format!("zarr_links_axis_{axis}"));
        let store = root.join("store.zarr");
        base(&store);
        consolidate(&store);
        replace_with_link(&store, axis, &root.join("outside"));
        assert_refused(&store, axis);
    }
}

/// With `/` in chunk keys a chunk has parent folders, and each is a place for a link.
#[test]
fn a_linked_parent_of_a_slash_separated_chunk_is_not_read() {
    for depth in [1usize, 2] {
        let root = scratch(&format!("zarr_links_slash_{depth}"));
        let store = root.join("store.zarr");
        base(&store);
        let dir = store.join(VARIABLE);
        // Listed first: the folders made below must not turn up in the listing being walked, as they
        // do on Linux.
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect();
        for name in names {
            let parts: Vec<&str> = name.split('.').collect();
            assert_eq!(parts.len(), 3, "{name}");
            let nested = dir.join(parts[0]).join(parts[1]);
            fs::create_dir_all(&nested).unwrap();
            fs::rename(dir.join(&name), nested.join(parts[2])).unwrap();
        }
        let zarray = dir.join(".zarray");
        let text = fs::read_to_string(&zarray).unwrap();
        fs::write(
            &zarray,
            text.replace(
                "\"order\":\"C\"",
                "\"order\":\"C\",\"dimension_separator\":\"/\"",
            ),
        )
        .unwrap();
        assert!(report(&store).contains("READ TMP_2maboveground: 96 cells"));
        let inside = if depth == 1 {
            format!("{VARIABLE}/0")
        } else {
            format!("{VARIABLE}/0/1")
        };
        replace_with_link(&store, &inside, &root.join("outside"));
        assert_refused(&store, &format!("chunk parent at depth {depth}"));
    }
}

#[test]
fn linked_files_and_symbolic_folders_are_not_read() {
    let root = scratch("zarr_links_files");
    // A chunk that is a link to a file outside.
    let store = root.join("chunk.zarr");
    base(&store);
    let chunk = store.join(VARIABLE).join("0.0.0");
    let outside = root.join("outside_chunk");
    fs::rename(&chunk, &outside).unwrap();
    if !link_file(&chunk, &outside) {
        if cfg!(unix) {
            panic!("a Unix account can always make a symbolic link");
        }
        eprintln!("skipped: this account may not make symbolic links");
        return;
    }
    assert_refused(&store, "chunk file");

    // Metadata that is a link: an array's, and the consolidated file.
    let store = root.join("zarray.zarr");
    base(&store);
    let zarray = store.join(VARIABLE).join(".zarray");
    let outside = root.join("outside_zarray");
    fs::rename(&zarray, &outside).unwrap();
    assert!(link_file(&zarray, &outside));
    assert_refused(&store, "array metadata");

    let store = root.join("zmetadata.zarr");
    base(&store);
    consolidate(&store);
    let outside = root.join("outside_zmetadata");
    fs::rename(store.join(".zmetadata"), &outside).unwrap();
    assert!(link_file(&store.join(".zmetadata"), &outside));
    assert_refused(&store, "consolidated metadata");

    // A folder that is a symbolic link, not a junction.
    let store = root.join("symlink.zarr");
    base(&store);
    let outside = root.join("outside_array");
    fs::rename(store.join(VARIABLE), &outside).unwrap();
    assert!(symlink_dir(&store.join(VARIABLE), &outside));
    assert_refused(&store, "symbolic folder");
}

/// The store's own path is the caller's, and may itself be reached through a link.
#[test]
fn a_store_reached_through_a_link_is_read() {
    let root = scratch("zarr_links_root");
    let real = root.join("real.zarr");
    base(&real);
    let alias = root.join("alias.zarr");
    link_dir(&alias, &real);
    assert!(report(&alias).contains("READ TMP_2maboveground: 96 cells"));
}

/// Windows device names the first filter let through: the console's, and a port numbered with a
/// superscript digit, which Windows also takes as the port.
#[test]
fn every_device_name_is_left_out() {
    let root = scratch("zarr_links_devices").join("store.zarr");
    base(&root);
    let zarray = fs::read_to_string(root.join(VARIABLE).join(".zarray")).unwrap();
    let zattrs = fs::read_to_string(root.join(VARIABLE).join(".zattrs")).unwrap();
    let names = [
        "CONIN$",
        "CONOUT$",
        "conin$",
        "COM\u{b9}",
        "COM\u{b2}",
        "LPT\u{b3}",
        "com\u{b9}.txt",
        "COM0",
        "LPT0",
        "NUL",
        "AUX.zarr",
        "CON ",
    ];
    let mut entries = vec![r#"".zgroup":{"zarr_format":2}"#.to_string()];
    for name in names {
        entries.push(format!("\"{name}/.zarray\":{zarray}"));
        entries.push(format!("\"{name}/.zattrs\":{zattrs}"));
    }
    fs::write(
        root.join(".zmetadata"),
        format!(
            r#"{{"zarr_consolidated_format":1,"metadata":{{{}}}}}"#,
            entries.join(",")
        ),
    )
    .unwrap();
    let store = ZarrStore::open(&root).unwrap();
    assert_eq!(store.variable_names(), Vec::<String>::new());
    assert_eq!(
        store.left_out().len(),
        names.len(),
        "{:?}",
        store.left_out()
    );
    // Negative control: an ordinary name with the same metadata is offered.
    fs::write(
        root.join(".zmetadata"),
        format!(
            r#"{{"zarr_consolidated_format":1,"metadata":{{".zgroup":{{"zarr_format":2}},"COMET/.zarray":{zarray},"COMET/.zattrs":{zattrs}}}}}"#
        ),
    )
    .unwrap();
    assert_eq!(ZarrStore::open(&root).unwrap().variable_names(), ["COMET"]);
}

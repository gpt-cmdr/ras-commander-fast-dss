//! A write that fails at any one step leaves an existing target byte-identical and nothing else.
//! Each step is made to fail in turn through `write_*_with_faults`, under each combination of old
//! files. `a_failed_undo_...` then fails the undo as well, where the old file must stay reachable.

use dss_vortex::crs::CrsDef;
use dss_vortex::sink_raster::{
    write_asc, write_asc_with_faults, write_geotiff, write_geotiff_with_faults, AscOptions,
    TiffOptions, WriteStep,
};
use dss_vortex::source::raster::RasterFile;
use dss_vortex::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder, VortexError,
};
use std::fs;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("p50_faults_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn frame(crs: Crs) -> GridFrame {
    GridFrame::new(GridFrameParts {
        cells: vec![1.5, 2.5, 3.5, 4.5],
        nx: 2,
        ny: 2,
        row_order: RowOrder::NorthToSouth,
        transform: GeoTransform::axis_aligned(10.0, 1.0, 20.0, -1.0),
        crs,
        nodata: None,
        time: FrameTime::Invariant,
        data_type: DataType::InstVal,
        units: String::new(),
        variable: String::new(),
        description: None,
    })
    .unwrap()
}

fn wgs84ish() -> Crs {
    Crs::Wkt(CrsDef::sphere_lonlat(6371000.0).to_wkt())
}

fn injected() -> std::io::Error {
    std::io::Error::other("injected")
}

/// The steps a successful write passes through, in order.
fn asc_steps(path: &Path, crs: Crs) -> Vec<WriteStep> {
    let mut seen = Vec::new();
    write_asc_with_faults(&frame(crs), path, &AscOptions::default(), &mut |s| {
        seen.push(s);
        Ok(())
    })
    .unwrap();
    seen
}

const ORIGINAL_GRID: &[u8] = b"ORIGINAL GRID";
const ORIGINAL_PRJ: &[u8] = b"ORIGINAL PRJ";

/// (has an old grid, has an old `.prj`, the new frame's CRS)
fn asc_cases() -> Vec<(&'static str, bool, bool, Crs)> {
    vec![
        ("old pair, new pair", true, true, wgs84ish()),
        ("old grid only, new pair", true, false, wgs84ish()),
        ("old pair, new grid only", true, true, Crs::Undefined),
        ("nothing old, new pair", false, false, wgs84ish()),
        ("stale prj, new pair", false, true, wgs84ish()),
    ]
}

fn set_up(dir: &Path, grid: bool, prj: bool) -> (PathBuf, PathBuf) {
    let (path, prj_path) = (dir.join("a.asc"), dir.join("a.prj"));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&prj_path);
    if grid {
        fs::write(&path, ORIGINAL_GRID).unwrap();
    }
    if prj {
        fs::write(&prj_path, ORIGINAL_PRJ).unwrap();
    }
    (path, prj_path)
}

#[test]
fn an_asc_write_failing_at_any_step_changes_nothing() {
    let dir = scratch("asc");
    for (label, grid, prj, crs) in asc_cases() {
        let (path, prj_path) = set_up(&dir, grid, prj);
        let steps = asc_steps(&path, crs.clone());
        assert!(steps.contains(&WriteStep::RenameGrid), "{label}");
        // Every step before the write has finished. RemoveOld comes after it; see below.
        let failing: Vec<WriteStep> = steps
            .iter()
            .copied()
            .filter(|s| *s != WriteStep::RemoveOld)
            .collect();
        assert!(failing.len() >= 6, "{label}: {failing:?}");
        for step in failing {
            let (path, prj_path) = set_up(&dir, grid, prj);
            let before = names(&dir);
            let result = write_asc_with_faults(
                &frame(crs.clone()),
                &path,
                &AscOptions::default(),
                &mut |s| if s == step { Err(injected()) } else { Ok(()) },
            );
            assert!(
                matches!(result, Err(VortexError::Io(_))),
                "{label} {step:?}: {result:?}"
            );
            assert_eq!(names(&dir), before, "{label} {step:?}");
            if grid {
                assert_eq!(fs::read(&path).unwrap(), ORIGINAL_GRID, "{label} {step:?}");
            }
            if prj {
                assert_eq!(
                    fs::read(&prj_path).unwrap(),
                    ORIGINAL_PRJ,
                    "{label} {step:?}"
                );
            }
        }
        let _ = (path, prj_path);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_geotiff_write_failing_at_any_step_changes_nothing() {
    let dir = scratch("tif");
    let path = dir.join("a.tif");
    for existing in [true, false] {
        let _ = fs::remove_file(&path);
        if existing {
            fs::write(&path, ORIGINAL_GRID).unwrap();
        }
        let mut steps = Vec::new();
        let probe = dir.join("probe.tif");
        write_geotiff_with_faults(
            &frame(wgs84ish()),
            &probe,
            &TiffOptions::default(),
            &mut |s| {
                steps.push(s);
                Ok(())
            },
        )
        .unwrap();
        fs::remove_file(&probe).unwrap();
        assert_eq!(
            steps,
            [
                WriteStep::CreateTemp,
                WriteStep::WriteData,
                WriteStep::Flush,
                WriteStep::ReadBack,
                WriteStep::SyncTemp,
                WriteStep::RenameGrid
            ]
        );
        for step in steps {
            let before = names(&dir);
            let result = write_geotiff_with_faults(
                &frame(wgs84ish()),
                &path,
                &TiffOptions::default(),
                &mut |s| if s == step { Err(injected()) } else { Ok(()) },
            );
            assert!(matches!(result, Err(VortexError::Io(_))), "{step:?}");
            assert_eq!(names(&dir), before, "{step:?}");
            if existing {
                assert_eq!(fs::read(&path).unwrap(), ORIGINAL_GRID, "{step:?}");
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_undo_keeps_the_old_file_and_says_where() {
    let dir = scratch("undo");
    for undo in [WriteStep::RestorePrj, WriteStep::RestoreGrid] {
        let (path, prj_path) = set_up(&dir, true, true);
        let result = write_asc_with_faults(
            &frame(wgs84ish()),
            &path,
            &AscOptions::default(),
            &mut |s| {
                if s == WriteStep::RenameGrid || s == undo {
                    Err(injected())
                } else {
                    Ok(())
                }
            },
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("kept at"), "{undo:?}: {message}");
        // Whichever file could not go back is named in the error, whole, under its `.old` name.
        let kept = names(&dir)
            .into_iter()
            .find(|n| n.ends_with(".old") && message.contains(n.as_str()))
            .unwrap_or_else(|| panic!("{undo:?}: {message}"));
        let bytes = fs::read(dir.join(&kept)).unwrap();
        let original = if kept.contains(".a.prj.") {
            ORIGINAL_PRJ
        } else {
            ORIGINAL_GRID
        };
        assert_eq!(bytes, original, "{undo:?}");
        let _ = prj_path;
        for name in names(&dir) {
            fs::remove_file(dir.join(name)).unwrap();
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_prj_undo_names_the_grid_it_left_aside_too() {
    let dir = scratch("undo_both");
    let (path, _) = set_up(&dir, true, true);
    let result = write_asc_with_faults(
        &frame(wgs84ish()),
        &path,
        &AscOptions::default(),
        &mut |s| {
            if s == WriteStep::RenameGrid || s == WriteStep::RestorePrj {
                Err(injected())
            } else {
                Ok(())
            }
        },
    );
    let message = result.unwrap_err().to_string();
    let old: Vec<String> = names(&dir)
        .into_iter()
        .filter(|n| n.ends_with(".old"))
        .collect();
    assert_eq!(old.len(), 2, "{old:?}");
    for name in &old {
        assert!(message.contains(name.as_str()), "{name} not in: {message}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_new_prj_that_cannot_be_removed_on_undo_is_reported() {
    let dir = scratch("undo_new");
    let (path, _) = set_up(&dir, true, false);
    let result = write_asc_with_faults(
        &frame(wgs84ish()),
        &path,
        &AscOptions::default(),
        &mut |s| {
            if s == WriteStep::RenameGrid || s == WriteStep::RemoveNewPrj {
                Err(injected())
            } else {
                Ok(())
            }
        },
    );
    let message = result.unwrap_err().to_string();
    assert!(message.contains("kept at"), "{message}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_success_that_cannot_clean_up_is_still_a_whole_new_pair() {
    let dir = scratch("cleanup");
    let (path, prj_path) = set_up(&dir, true, true);
    write_asc_with_faults(
        &frame(wgs84ish()),
        &path,
        &AscOptions::default(),
        &mut |s| {
            if s == WriteStep::RemoveOld {
                Err(injected())
            } else {
                Ok(())
            }
        },
    )
    .unwrap();
    let read = RasterFile::open(&path).unwrap();
    assert_eq!(read.frame().cells(), [1.5f32, 2.5, 3.5, 4.5]);
    assert_ne!(fs::read(&prj_path).unwrap(), ORIGINAL_PRJ);
    let old = names(&dir)
        .into_iter()
        .filter(|n| n.ends_with(".old"))
        .count();
    assert_eq!(old, 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_successful_replace_leaves_the_pair_and_nothing_else() {
    let dir = scratch("replace");
    let (path, _) = set_up(&dir, true, true);
    write_asc(&frame(wgs84ish()), &path, &AscOptions::default()).unwrap();
    assert_eq!(names(&dir), ["a.asc", "a.prj"]);
    write_asc(&frame(Crs::Undefined), &path, &AscOptions::default()).unwrap();
    assert_eq!(names(&dir), ["a.asc"]);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn a_target_another_process_holds_open_is_an_error_that_changes_nothing() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = scratch("locked");
    for name in ["a.asc", "a.tif"] {
        let path = dir.join(name);
        fs::write(&path, ORIGINAL_GRID).unwrap();
        // No sharing: a rename or a delete of this file fails until it is closed.
        let held = fs::File::options()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let result = if name.ends_with("asc") {
            write_asc(&frame(wgs84ish()), &path, &AscOptions::default())
        } else {
            write_geotiff(&frame(wgs84ish()), &path, &TiffOptions::default())
        };
        assert!(matches!(result, Err(VortexError::Io(_))), "{name}");
        drop(held);
        assert_eq!(fs::read(&path).unwrap(), ORIGINAL_GRID, "{name}");
        assert_eq!(names(&dir).iter().filter(|n| n.starts_with('.')).count(), 0);
        fs::remove_file(&path).unwrap();
    }
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn a_prj_in_another_case_refuses_where_a_folder_keeps_case() {
    let dir = scratch("case");
    let path = dir.join("a.asc");
    fs::write(dir.join("a.PRJ"), ORIGINAL_PRJ).unwrap();
    let result = write_asc(&frame(Crs::Undefined), &path, &AscOptions::default());
    assert!(matches!(result, Err(VortexError::Refused { .. })));
    assert_eq!(names(&dir), ["a.PRJ"]);
    let _ = fs::remove_dir_all(&dir);
}

//! When a store's precipitation is AORC's hour ending at its time, and when it is not (review of
//! `8fc6610a`, finding 2).
//!
//! The period comes from what the product is, not from how its times are spaced: a store is
//! AORC v1.1 when its `APCP_surface` states `aorc_version` as a recognized text, its times are at
//! the top of the hour, and nothing it states contradicts one hour ending at the time. A subset
//! with gaps is still hourly totals. Anything else has no period of its own, and the caller states
//! one or the import is refused.

#![allow(clippy::result_large_err)] // `ImportError`: see `import.rs`.
mod zarr_support;

use dss_vortex::frame::FrameTime;
use dss_vortex::import::{ImportError, ImportRequest, ImportedRecord, Selection};
use dss_vortex::products::window::Accumulation;
use dss_vortex::products::WindowRule;
use dss_vortex::source::zarr::{self, StoreSelection, ZarrStore};
use dss_vortex::warp::{Align, Method};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use zarr_support::*;

const BASE: &str = r#""units":"kg/m^2","crs":"EPSG:4326","_FillValue":-32767,"scale_factor":0.1"#;

/// A store of `APCP_surface` at the given hours since 2000-01-01, with extra attributes.
fn store(name: &str, hours: &[f64], extra: &str) -> PathBuf {
    let root = scratch(&format!("zarr_aorc_{name}")).join("s.zarr");
    let attrs = if extra.is_empty() {
        BASE.to_string()
    } else {
        format!("{BASE},{extra}")
    };
    write_store(
        &root,
        &grid_arrays("APCP_surface", hours, 4, 6, &attrs, &ramp(hours.len() * 24)),
    );
    root
}

fn import(root: &Path, rule: WindowRule) -> Result<Vec<ImportedRecord>, ImportError> {
    let out = root.parent().unwrap().join("out.dss");
    let _ = std::fs::remove_file(&out);
    let mut r = ImportRequest::new(
        vec![root.to_path_buf()],
        out,
        Selection::Variable("APCP_surface".into()),
        Method::Bilinear,
    );
    r.parts.a = "A".into();
    r.parts.f = "F".into();
    r.sync = false;
    r.align = Align::Vortex;
    r.window_rule = rule;
    zarr::import_stores(&r, &StoreSelection::default(), &mut |_| {
        ControlFlow::Continue(())
    })
}

fn stated(root: &Path) -> Option<i64> {
    ZarrStore::open(root)
        .unwrap()
        .variable("APCP_surface")
        .unwrap()
        .stated_accumulation()
}

fn stamps(records: &[ImportedRecord]) -> Vec<(String, String)> {
    records
        .iter()
        .map(|r| {
            let parts: Vec<&str> = r.record.pathname.split('/').collect();
            (parts[4].to_string(), parts[5].to_string())
        })
        .collect()
}

const V11: &str = r#""aorc_version":"v1.1""#;

#[test]
fn a_recognized_store_is_hourly_whatever_its_spacing() {
    // Hourly, a subset with a gap, and one step alone: each is the hour ending at its time.
    for (name, hours) in [
        ("hourly", vec![1.0, 2.0, 3.0]),
        ("gapped", vec![1.0, 2.0, 7.0]),
        ("single", vec![5.0]),
    ] {
        let root = store(name, &hours, V11);
        assert_eq!(stated(&root), Some(3600), "{name}");
        let records = import(&root, WindowRule::Refuse).unwrap();
        assert_eq!(records.len(), hours.len(), "{name}");
        let last = *hours.last().unwrap() as u32;
        assert_eq!(
            stamps(&records).last().unwrap(),
            &(
                format!("01JAN2000:{:02}00", last - 1),
                format!("01JAN2000:{last:02}00")
            ),
            "{name}"
        );
    }
    let v = ZarrStore::open(&store("period", &[1.0, 2.0, 7.0], V11)).unwrap();
    let v = v.variable("APCP_surface").unwrap();
    let julian = dss_core::time::ymd_to_julian(2000, 1, 1);
    assert_eq!(
        v.time(2).unwrap(),
        FrameTime::Period {
            start: (julian, 6 * 3600),
            end: (julian, 7 * 3600),
        }
    );
}

/// Each of these was given 3600 seconds before. None states AORC v1.1's hour, so none has a
/// period of its own; the import says what is wrong and what to state.
#[test]
fn a_store_that_is_not_recognized_has_no_period_of_its_own() {
    for (name, hours, extra, why) in [
        (
            "null_version",
            vec![1.0, 2.0],
            r#""aorc_version":null"#,
            "aorc_version",
        ),
        (
            "number_version",
            vec![1.0, 2.0],
            r#""aorc_version":1.1"#,
            "aorc_version",
        ),
        (
            "other_version",
            vec![1.0, 2.0],
            r#""aorc_version":"v9.9""#,
            "aorc_version",
        ),
        ("half_hour", vec![0.5, 1.5, 2.5], V11, "top of the hour"),
        ("half_hour_single", vec![5.5], V11, "top of the hour"),
        (
            "start_label",
            vec![1.0, 2.0],
            r#""aorc_version":"v1.1","time_label":"start""#,
            "time_label",
        ),
        (
            "daily_interval",
            vec![1.0, 2.0],
            r#""aorc_version":"v1.1","accumulation_interval_hours":24"#,
            "accumulation_interval_hours",
        ),
        (
            "mean_cells",
            vec![1.0, 2.0],
            r#""aorc_version":"v1.1","cell_methods":"time: mean""#,
            "cell_methods",
        ),
    ] {
        let root = store(name, &hours, extra);
        assert_eq!(stated(&root), None, "{name}");
        let e = import(&root, WindowRule::Refuse).unwrap_err().to_string();
        assert!(e.contains(why), "{name}: {e}");
        assert!(e.contains("accumulation:"), "{name}: {e}");
        assert!(!root.parent().unwrap().join("out.dss").exists(), "{name}");
    }
    // Attributes that agree with the hour do not unmake it.
    let agreeing = store(
        "agreeing",
        &[1.0, 2.0],
        r#""aorc_version":"v1.1","time_label":"end","accumulation_interval_hours":1.0,"cell_methods":"time: sum""#,
    );
    assert_eq!(stated(&agreeing), Some(3600));
    // No claim to be AORC at all: the importer's own refusal of rain at an instant.
    let plain = store("plain", &[1.0, 2.0], "");
    assert_eq!(stated(&plain), None);
    let e = import(&plain, WindowRule::Refuse).unwrap_err().to_string();
    assert!(e.contains("precipitation at an instant"), "{e}");
}

/// The caller's statement stands where the store has none, and is refused where the store states
/// another duration.
#[test]
fn a_callers_period_must_not_contradict_the_store() {
    let h01 = WindowRule::Accumulation(Accumulation::H01);
    let h24 = WindowRule::Accumulation(Accumulation::H24);
    // Unrecognized version, nothing contradicting: the caller's hour, noted as the caller's.
    let root = store("caller", &[1.0, 2.0], r#""aorc_version":null"#);
    let records = import(&root, h01).unwrap();
    assert_eq!(records[0].window.as_ref().unwrap().stated_by, "caller");
    assert_eq!(
        stamps(&records)[1],
        ("01JAN2000:0100".to_string(), "01JAN2000:0200".to_string())
    );
    // The store states 24 hours: 24H stands, one hour is refused.
    let daily = store(
        "caller_daily",
        &[24.0, 48.0],
        r#""aorc_version":"v1.1","accumulation_interval_hours":24"#,
    );
    let records = import(&daily, h24).unwrap();
    assert_eq!(
        stamps(&records)[0],
        ("01JAN2000:0000".to_string(), "01JAN2000:2400".to_string())
    );
    let e = import(&daily, h01).unwrap_err().to_string();
    assert!(e.contains("the store states 86400 s"), "{e}");
    // A label that says the times are starts: no period ending at them, whoever states it.
    let start = store(
        "caller_start",
        &[1.0, 2.0],
        r#""aorc_version":"v1.1","time_label":"start""#,
    );
    let e = import(&start, h01).unwrap_err().to_string();
    assert!(e.contains("time_label"), "{e}");
    // A recognized store: only its own hour.
    let aorc = store("caller_aorc", &[1.0, 2.0], V11);
    assert_eq!(
        import(&aorc, h01).unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .stated_by,
        "store"
    );
    let e = import(&aorc, h24).unwrap_err().to_string();
    assert!(e.contains("the store states 3600 s"), "{e}");
}

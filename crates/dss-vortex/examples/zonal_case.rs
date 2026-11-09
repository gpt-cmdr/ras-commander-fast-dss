//! Run grid-to-point cases from a JSON job and record masks and statistics as raw bits, in the shape
//! of `fixtures/public/vortex_p51/expected.json`. Test tool only.
//!
//! Usage: `zonal_case <job.json>`. A case's `options` are Vortex's own write options.

use dss_vortex::crs::DatumPolicy;
use dss_vortex::zonal::{
    self, Compat, GridToPointRequest, PointParts, Statistic, ZonalRequest, ZoneLimits, ZoneSet,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn bits(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

fn run(case: &Value) -> Result<Value, String> {
    let text = |k: &str| case[k].as_str().map(str::to_string);
    let compat = if text("compat").as_deref() == Some("vortex") {
        Compat::Vortex
    } else {
        Compat::Engine
    };
    let options = case["options"].as_object().cloned().unwrap_or_default();
    let on = |k: &str| {
        options
            .get(k)
            .and_then(Value::as_str)
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    };
    let mut statistics = Vec::new();
    for (k, s) in [
        ("Average", Statistic::Average),
        ("Min", Statistic::Min),
        ("Max", Statistic::Max),
        ("Median", Statistic::Median),
        ("1Q", Statistic::FirstQuartile),
        ("3Q", Statistic::ThirdQuartile),
        ("Pct>0", Statistic::PctGtZero),
        ("Pct>1Q", Statistic::PctGtFirstQuartile),
    ] {
        if on(k) {
            statistics.push(s);
        }
    }
    let zonal = ZonalRequest {
        grids: PathBuf::from(text("grids").ok_or("grids")?),
        variables: case["variables"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        }),
        zones: PathBuf::from(text("zones").ok_or("zones")?),
        field: text("field").ok_or("field")?,
        zones_crs: None,
        datum_policy: DatumPolicy::Refuse,
        compat,
        limits: ZoneLimits::default(),
        area_weighted: false,
    };
    let table = zonal::compute(&zonal).map_err(|e| e.to_string())?;
    let set = ZoneSet::read(&zonal.zones, &zonal.field, None, &zonal.limits)
        .map_err(|e| e.to_string())?;
    let mut masks = Vec::new();
    for grid in &table.placements {
        let built = zonal::zone_masks(&set, grid, zonal.datum_policy, &zonal.limits)
            .map_err(|e| e.to_string())?;
        let zones: BTreeMap<String, Value> = built
            .iter()
            .map(|m| (m.id.clone(), json!(m.cells)))
            .collect();
        let fallback: Vec<&str> = built
            .iter()
            .filter(|m| m.centroid_fallback)
            .map(|m| m.id.as_str())
            .collect();
        masks.push(json!({
            "nx": grid.nx, "ny": grid.ny,
            "dx": bits(grid.transform[1]), "dy": bits(grid.transform[5]),
            "origin_x": bits(grid.transform[0]), "origin_y": bits(grid.transform[3]),
            "zones": zones, "fallback": fallback,
        }));
    }
    let n = table.zones.len();
    let mut frames = Vec::new();
    for (i, f) in table.frames.iter().enumerate() {
        let mut stats = BTreeMap::new();
        for (z, zone) in table.zones.iter().enumerate() {
            let s = &table.stats[i * n + z];
            stats.insert(
                zone.id.clone(),
                json!({
                    "average": bits(s.average), "min": bits(s.min), "max": bits(s.max),
                    "median": bits(s.median), "q1": bits(s.first_quartile),
                    "q3": bits(s.third_quartile), "pct0": bits(s.pct_gt_zero),
                    "pctq1": bits(s.pct_gt_first_quartile),
                }),
            );
        }
        frames.push(json!({
            "description": f.description, "units": f.units,
            "start_seconds": f.start, "end_seconds": f.end, "stats": stats,
        }));
    }
    let mut result = json!({"id": case["id"], "masks": masks, "frames": frames});
    if let Some(out) = text("out") {
        let parts = PointParts {
            a: options
                .get("partA")
                .and_then(Value::as_str)
                .map(str::to_string),
            f: options
                .get("partF")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        let request = GridToPointRequest {
            zonal,
            output: PathBuf::from(out),
            statistics,
            accumulate: on("isAccumulate"),
            parts,
            version: case["version"].as_u64().map(|v| v as u8),
            allow_undetectable_readers: false,
            allow_in_place: false,
            fault: None,
        };
        let mut outcome = zonal::grid_to_point(&request).map_err(|e| e.to_string())?;
        // A second run into the same output, as Vortex's driver does for such a case.
        if let Some(second) = text("rerun") {
            let mut request = request.clone();
            request.zonal.grids = PathBuf::from(second);
            outcome = zonal::grid_to_point(&request).map_err(|e| e.to_string())?;
        }
        result["records"] = outcome
            .records
            .iter()
            .map(|r| {
                json!({
                    "pathname": r.pathname, "statistic": r.statistic, "regular": r.regular,
                    "values": r.values, "units": r.units, "data_type": r.data_type,
                    "outcome": r.outcome,
                })
            })
            .collect();
    }
    Ok(result)
}

fn main() {
    let path = std::env::args().nth(1).expect("job path");
    let job: Value = serde_json::from_slice(&std::fs::read(path).expect("job")).expect("job json");
    let mut out = Vec::new();
    for case in job["cases"].as_array().expect("cases") {
        match run(case) {
            Ok(r) => out.push(r),
            Err(e) => out.push(json!({"id": case["id"], "error": e})),
        }
    }
    std::fs::write(
        job["result"].as_str().expect("result"),
        serde_json::to_vec(&out).expect("json"),
    )
    .expect("write");
}

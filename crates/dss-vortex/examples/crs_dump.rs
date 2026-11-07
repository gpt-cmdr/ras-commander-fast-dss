//! P43 evidence: runs `crs::Transformer` over the job `oracle/vortex/crs_cases.py` writes and
//! dumps f64 bits for comparison with PROJ 7.2.0 inside Vortex. A failed point is written as
//! +infinity, which is what `HUGE_VAL` is. Usage: `crs_dump <job.json> <suffix>`.

use dss_vortex::crs::{reproject_envelope, CrsDef, DatumPolicy, Envelope, Transformer};
use std::fs;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let job: serde_json::Value =
        serde_json::from_slice(&fs::read(&args[1]).expect("job")).expect("json");
    let suffix = &args[2];
    let policy = |case: &serde_json::Value| {
        if case["opt_in"].as_bool().unwrap_or(false) {
            DatumPolicy::AssumeNoShift
        } else {
            DatumPolicy::Refuse
        }
    };
    for case in job["points"].as_array().expect("points") {
        let id = case["id"].as_str().expect("id");
        let parse = |k: &str| CrsDef::parse_wkt(case[k].as_str().expect("wkt"));
        let t = match (parse("src"), parse("dst")) {
            (Ok(s), Ok(d)) => Transformer::new(&s, &d, policy(case)),
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
        let t = match t {
            Ok(t) => t,
            Err(e) => {
                println!("CASE-FAIL {id} {e}");
                continue;
            }
        };
        let center = case["center_long"].as_f64();
        let t = t.with_center_long(center, center);
        let input = fs::read(case["in"].as_str().expect("in")).expect("input");
        let mut out = Vec::with_capacity(input.len());
        for p in input.as_chunks::<16>().0 {
            let x = f64::from_le_bytes(p[..8].try_into().expect("8"));
            let y = f64::from_le_bytes(p[8..].try_into().expect("8"));
            let (rx, ry) = t.transform(x, y).unwrap_or((f64::INFINITY, f64::INFINITY));
            out.extend_from_slice(&rx.to_le_bytes());
            out.extend_from_slice(&ry.to_le_bytes());
        }
        let path = format!("{}.{suffix}", case["out"].as_str().expect("out"));
        fs::write(path, out).expect("write");
    }
    let mut envelopes = serde_json::Map::new();
    for case in job["envelopes"].as_array().expect("envelopes") {
        let id = case["id"].as_str().expect("id").to_string();
        let e: Vec<f64> = case["env"]
            .as_array()
            .expect("env")
            .iter()
            .map(|v| v.as_f64().expect("number"))
            .collect();
        let env = Envelope {
            min_x: e[0],
            max_x: e[1],
            min_y: e[2],
            max_y: e[3],
        };
        let parse = |k: &str| CrsDef::parse_wkt(case[k].as_str().expect("wkt")).expect("parse");
        let r = reproject_envelope(env, &parse("src"), &parse("dst"), policy(case));
        let v = match r {
            Ok(r) => serde_json::json!([r.min_x, r.max_x, r.min_y, r.max_y]),
            Err(err) => serde_json::json!(["refused", err.to_string()]),
        };
        envelopes.insert(id, v);
    }
    let path = format!("{}.{suffix}", job["meta_out"].as_str().expect("meta_out"));
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::Value::Object(envelopes)).expect("json"),
    )
    .expect("write");
    println!("DONE");
}

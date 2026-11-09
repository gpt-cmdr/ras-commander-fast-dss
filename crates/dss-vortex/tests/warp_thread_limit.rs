//! Meant to run inside a 64 MiB Windows Job Object (memory limit). On its own it
//! does nothing unless `WARP_THREAD_PROBE` is set, and then any panic fails it.

use dss_vortex::crs::{CrsDef, Envelope};
use dss_vortex::frame::GeoTransform;
use dss_vortex::warp::{Method, SourceGrid, WarpEnvelope, WarpPlan, WarpSpec};

#[test]
fn thread_resource_probe() {
    let Ok(threads) = std::env::var("WARP_THREAD_PROBE") else {
        return;
    };
    let source = SourceGrid {
        nx: 1,
        ny: 1,
        transform: GeoTransform::axis_aligned(0.0, 1.0, 65536.0, -65536.0),
        crs: CrsDef::shg(),
    };
    let mut spec = WarpSpec::new(Method::Near);
    spec.cell_size = Some(1.0);
    spec.threads = if threads == "max" {
        usize::MAX
    } else {
        threads.parse().unwrap()
    };
    spec.envelope = Some(WarpEnvelope {
        envelope: Envelope {
            min_x: 0.0,
            max_x: 1.0,
            min_y: 0.0,
            max_y: 65536.0,
        },
        crs: Some(CrsDef::shg()),
    });
    let plan = WarpPlan::new(&source, &spec).unwrap();
    assert_eq!((plan.nx(), plan.ny()), (1, 65536));
    match plan.warp_cells(&[1.0], -1.0) {
        Ok(out) => {
            assert_eq!(out.len(), 65536);
            eprintln!("probe-outcome capped success");
        }
        Err(e) => eprintln!("probe-outcome typed refusal: {e}"),
    }
}

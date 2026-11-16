//! P76: `Align::SnapOut`, `WarpPlan::uncovered_cells` and the alignment note.
//!
//! A geographic grid warped to SHG Albers at 2 km with no envelope: GDAL's suggested extent
//! starts off whole cells. The default refuses it; `Vortex` accepts it as computed (and the write
//! later rounds the corner); `SnapOut` rounds the minimum corner down and the maximum up to whole
//! cells and warps onto that lattice, which then contains Vortex's extent. With an envelope the
//! three agree with the default where it plans.

use dss_vortex::crs::{CrsDef, Envelope};
use dss_vortex::frame::GeoTransform;
use dss_vortex::sink_dss::{alignment_note, placement};
use dss_vortex::warp::{Align, Method, SourceGrid, WarpEnvelope, WarpPlan, WarpSpec};

const NAD83: &str = "GEOGCS[\"NAD83\",DATUM[\"North_American_Datum_1983\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]";

fn source() -> SourceGrid {
    SourceGrid {
        nx: 120,
        ny: 90,
        transform: GeoTransform::axis_aligned(-90.0123, 0.01, 35.0071, -0.01),
        crs: CrsDef::parse_wkt(NAD83).unwrap(),
    }
}

fn spec(align: Align, envelope: Option<Envelope>) -> WarpSpec {
    let mut s = WarpSpec::new(Method::Bilinear);
    s.target = Some(CrsDef::shg());
    s.cell_size = Some(2000.0);
    s.align = align;
    s.envelope = envelope.map(|e| WarpEnvelope {
        envelope: e,
        crs: Some(CrsDef::shg()),
    });
    s
}

fn whole(v: f64, c: f64) -> bool {
    ((v / c) - (v / c).round()).abs() < 1e-9
}

fn extent(p: &WarpPlan) -> [f64; 4] {
    let g = p.geotransform();
    let (nx, ny) = (p.nx() as f64, p.ny() as f64);
    [g[0], g[3] + g[5] * ny, g[0] + g[1] * nx, g[3]]
}

#[test]
fn snap_out_covers_the_suggested_extent_on_whole_cells() {
    let src = source();
    assert!(WarpPlan::new(&src, &spec(Align::Refuse, None)).is_err());
    let vortex = WarpPlan::new(&src, &spec(Align::Vortex, None)).unwrap();
    let snap = WarpPlan::new(&src, &spec(Align::SnapOut, None)).unwrap();
    let c = 2000.0;
    let (gv, gs) = (vortex.geotransform(), snap.geotransform());
    assert!(
        !whole(gv[0], c) || !whole(gv[3], c),
        "the case must start off whole cells"
    );
    assert_eq!((gs[1], gs[5]), (c, -c));
    assert!(whole(gs[0], c) && whole(gs[3], c));
    let (ev, es) = (extent(&vortex), extent(&snap));
    assert!(es[0] <= ev[0] && es[1] <= ev[1] && es[2] >= ev[2] && es[3] >= ev[3]);
    // Outward by less than one cell on each side.
    assert!(ev[0] - es[0] < c && ev[1] - es[1] < c && es[2] - ev[2] < c && es[3] - ev[3] < c);
    // A snapped lattice has more cells; geometry decides which have no source value.
    assert!(snap.nx() * snap.ny() >= vortex.nx() * vortex.ny());
    let (uv, us) = (vortex.uncovered_cells(), snap.uncovered_cells());
    assert!(us <= (snap.nx() * snap.ny()) as u64 && uv <= (vortex.nx() * vortex.ny()) as u64);
    // Snapping outward does not give every cell support: corners of the reprojected footprint
    // stay outside it, on either lattice.
    assert!(us > 0 && uv > 0);
    assert_eq!(snap.uncovered_cells(), us, "computed once, the same");

    // With an envelope on whole cells, SnapOut is the default's plan.
    let env = Envelope {
        min_x: c * 300.0,
        max_x: c * 340.0,
        min_y: c * 700.0,
        max_y: c * 730.0,
    };
    let own = WarpPlan::new(&src, &spec(Align::Refuse, Some(env))).unwrap();
    let snap_env = WarpPlan::new(&src, &spec(Align::SnapOut, Some(env))).unwrap();
    assert_eq!(own.geotransform(), snap_env.geotransform());
    assert_eq!((own.nx(), own.ny()), (snap_env.nx(), snap_env.ny()));
}

#[test]
fn placement_and_alignment_note() {
    // A grid 1.25 cells from zero in x: SnapOut is strict for a grid written unwarped.
    let t = GeoTransform::axis_aligned(2500.0, 2000.0, 20000.0, -2000.0);
    assert!(placement(&t, 3, Align::Refuse).is_err());
    assert!(placement(&t, 3, Align::SnapOut).is_err());
    let p = placement(&t, 3, Align::Vortex).unwrap();
    let n = alignment_note(&t, 3, p, 2);
    assert_eq!(
        n.sampled_transform,
        [2500.0, 2000.0, 0.0, 20000.0, 0.0, -2000.0]
    );
    assert_eq!(n.sampled_lower_left, (2500.0, 14000.0));
    assert_eq!(n.stored_lower_left, (2000.0, 14000.0));
    assert_eq!(n.stored_cell_size, 2000.0);
    assert_eq!(n.shift_map_units, (-500.0, 0.0));
    assert_eq!(n.shift_cells, (-0.25, 0.0));
    assert_eq!(n.missing_cells, 2);
    assert_eq!(n.uncovered_centres, None);
    // On whole cells: no shift in any mode.
    let w = GeoTransform::axis_aligned(4000.0, 2000.0, 20000.0, -2000.0);
    for a in [Align::Refuse, Align::SnapOut, Align::Vortex] {
        let n = alignment_note(&w, 3, placement(&w, 3, a).unwrap(), 0);
        assert_eq!((n.shift_cells, n.shift_map_units), ((0.0, 0.0), (0.0, 0.0)));
    }
}

//! Warp rules that need no oracle: refusals, the nodata rules, streaming and thread independence.
//! Values against Vortex are in `warp_fixture.rs`.

use dss_vortex::crs::{CrsDef, Envelope};
use dss_vortex::frame::{
    Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder,
};
use dss_vortex::warp::{Align, Method, SourceGrid, WarpEnvelope, WarpLimits, WarpPlan, WarpSpec};
use dss_vortex::VortexError;

const NX: usize = 300;
const NY: usize = 200;

fn source() -> SourceGrid {
    SourceGrid {
        nx: NX,
        ny: NY,
        transform: GeoTransform::axis_aligned(-100.0, 0.01, 40.0, -0.01),
        crs: CrsDef::sphere_lonlat(6371229.0),
    }
}

fn cells() -> Vec<f32> {
    (0..NX * NY)
        .map(|i| {
            let (x, y) = ((i % NX) as f32, (i / NX) as f32);
            (x * 0.37).sin() * 5.0 + y * 0.11 + 7.0
        })
        .collect()
}

fn spec(method: Method) -> WarpSpec {
    let mut s = WarpSpec::new(method);
    s.target = Some(CrsDef::shg());
    s.cell_size = Some(2000.0);
    s.envelope = Some(WarpEnvelope {
        envelope: Envelope {
            min_x: -240_500.0,
            max_x: -210_000.0,
            min_y: 1_800_100.0,
            max_y: 1_830_000.0,
        },
        crs: Some(CrsDef::shg()),
    });
    s
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|c| c.to_bits()).collect()
}

const METHODS: [Method; 3] = [Method::Near, Method::Bilinear, Method::Average];

#[test]
fn envelope_snaps_outwards_to_whole_cells() {
    let plan = WarpPlan::new(&source(), &spec(Method::Near)).unwrap();
    let g = plan.geotransform();
    assert_eq!(g[0], -242_000.0);
    assert_eq!(g[3], 1_830_000.0);
    assert_eq!((g[1], g[5]), (2000.0, -2000.0));
    assert_eq!((plan.nx(), plan.ny()), (16, 15));
}

#[test]
fn no_envelope_is_refused_unless_vortex_alignment_is_asked_for() {
    let mut s = spec(Method::Bilinear);
    s.envelope = None;
    let err = WarpPlan::new(&source(), &s).unwrap_err().to_string();
    assert!(
        err.contains("Observed behaviour we did not implement"),
        "{err}"
    );
    assert!(err.contains("align=\"vortex\""), "{err}");
    s.align = Align::Vortex;
    let plan = WarpPlan::new(&source(), &s).unwrap();
    assert!(plan.nx() > 1 && plan.ny() > 1);
    assert!(plan.geotransform()[0] % 2000.0 != 0.0);
    assert_eq!(
        plan.warp_cells(&cells(), -1.0).unwrap().len(),
        plan.nx() * plan.ny()
    );
}

#[test]
fn no_cell_size_on_a_stated_envelope_is_refused_by_default() {
    let mut s = spec(Method::Near);
    s.cell_size = None;
    assert!(matches!(
        WarpPlan::new(&source(), &s),
        Err(VortexError::Refused { .. })
    ));
    s.align = Align::Vortex;
    assert!(WarpPlan::new(&source(), &s).is_ok());
}

#[test]
fn output_does_not_depend_on_the_thread_count() {
    let data = {
        let mut d = cells();
        for i in (0..d.len()).step_by(17) {
            d[i] = f32::NAN;
        }
        for i in (0..d.len()).step_by(23) {
            d[i] = -1.0;
        }
        d
    };
    for method in METHODS {
        for nodata in [-1.0f32, f32::NAN] {
            let mut s = spec(method);
            let one = WarpPlan::new(&source(), &s)
                .unwrap()
                .warp_cells(&data, nodata)
                .unwrap();
            for threads in [0, 2, 3, 8, 64] {
                s.threads = threads;
                let many = WarpPlan::new(&source(), &s)
                    .unwrap()
                    .warp_cells(&data, nodata)
                    .unwrap();
                assert_eq!(bits(&one), bits(&many), "{method:?} {nodata} {threads}");
            }
        }
    }
}

#[test]
fn nan_is_data_when_nodata_is_another_value() {
    // MRMS: `-srcnodata -1` over an array holding NaN and no -1. No mask exists, so a NaN spreads
    // through a bilinear or average footprint. With NaN as nodata, valid neighbours carry on.
    let mut data = cells();
    for row in 0..NY {
        for col in 0..NX {
            if (130..134).contains(&col) || (70..74).contains(&row) {
                data[row * NX + col] = f32::NAN;
            }
        }
    }
    let count = |method, nodata: f32| {
        WarpPlan::new(&source(), &spec(method))
            .unwrap()
            .warp_cells(&data, nodata)
            .unwrap()
            .iter()
            .filter(|v| v.is_nan())
            .count()
    };
    for method in METHODS {
        let as_data = count(method, -1.0);
        let as_nodata = count(method, f32::NAN);
        match method {
            // Nearest copies one source value, so the marker makes no difference to the mask.
            Method::Near => assert_eq!(as_data, as_nodata),
            _ => assert!(as_data >= as_nodata, "{method:?} {as_data} {as_nodata}"),
        }
    }
}

#[test]
fn centre_validity_masks_a_cell_whose_truncated_source_pixel_is_invalid() {
    // Every other source column is nodata. Bilinear: a destination cell exists only where its
    // truncated source pixel is valid, as in GWKRealCaseThread. Near has the same rule.
    let data: Vec<f32> = (0..NX * NY)
        .map(|i| {
            if (i % NX).is_multiple_of(2) {
                -1.0
            } else {
                3.0
            }
        })
        .collect();
    let near = WarpPlan::new(&source(), &spec(Method::Near))
        .unwrap()
        .warp_cells(&data, -1.0)
        .unwrap();
    let bilinear = WarpPlan::new(&source(), &spec(Method::Bilinear))
        .unwrap()
        .warp_cells(&data, -1.0)
        .unwrap();
    assert!(near.contains(&-1.0) && near.contains(&3.0));
    // Valid weights renormalise, so a kept cell holds only the valid value.
    assert!(bilinear.iter().all(|v| *v == -1.0 || *v == 3.0));
}

#[test]
fn a_window_off_the_source_is_all_nodata() {
    let mut s = spec(Method::Bilinear);
    s.envelope = Some(WarpEnvelope {
        envelope: Envelope {
            min_x: 9_000_000.0,
            max_x: 9_020_000.0,
            min_y: 9_000_000.0,
            max_y: 9_020_000.0,
        },
        crs: Some(CrsDef::shg()),
    });
    let out = WarpPlan::new(&source(), &s)
        .unwrap()
        .warp_cells(&cells(), -1.0)
        .unwrap();
    assert!(out.iter().all(|v| *v == -1.0));
}

fn frame(values: Vec<f32>, nodata: Option<f32>) -> GridFrame {
    let s = source();
    GridFrame::new(GridFrameParts {
        cells: values,
        nx: s.nx,
        ny: s.ny,
        row_order: RowOrder::NorthToSouth,
        transform: s.transform,
        crs: Crs::Wkt(s.crs.to_wkt()),
        nodata,
        time: FrameTime::Period {
            start: (45000, 0),
            end: (45000, 3600),
        },
        data_type: DataType::PerCum,
        units: "MM".into(),
        variable: "precip".into(),
        description: None,
    })
    .unwrap()
}

#[test]
fn frames_stream_one_at_a_time_and_keep_their_metadata() {
    let first = frame(cells(), Some(-1.0));
    let plan = WarpPlan::for_frame(&first, &spec(Method::Average)).unwrap();
    let second = frame(cells().iter().map(|v| v * 2.0).collect(), Some(-1.0));
    let want: Vec<Vec<u32>> = [&first, &second]
        .iter()
        .map(|f| bits(&plan.warp_cells(f.cells(), -1.0).unwrap()))
        .collect();
    let got: Vec<GridFrame> = plan
        .warp_frames(vec![first, second])
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(got.len(), 2);
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(&bits(g.cells()), w);
        let p = g.parts();
        assert_eq!(p.crs, Crs::Named(NamedCrs::Shg));
        assert_eq!(p.row_order, RowOrder::NorthToSouth);
        assert_eq!(p.units, "MM");
        assert_eq!(p.data_type, DataType::PerCum);
        assert_eq!(p.nodata, Some(-1.0));
        assert_eq!(p.transform.origin_x, -242_000.0);
    }
}

#[test]
fn a_frame_without_nodata_or_on_another_grid_is_refused() {
    let plan = WarpPlan::new(&source(), &spec(Method::Near)).unwrap();
    assert!(matches!(
        plan.warp_frame(&frame(cells(), None)),
        Err(VortexError::Refused { .. })
    ));
    let mut other = frame(cells(), Some(-1.0)).into_parts();
    other.transform.origin_x += 1.0;
    assert!(matches!(
        plan.warp_frame(&GridFrame::new(other).unwrap()),
        Err(VortexError::Refused { .. })
    ));
    assert!(matches!(
        plan.warp_cells(&[0.0; 3], -1.0),
        Err(VortexError::Refused { .. })
    ));
}

#[test]
fn limits_refuse_before_allocating() {
    let mut s = spec(Method::Near);
    s.cell_size = Some(0.001);
    s.limits = WarpLimits {
        max_cells: 1_000_000,
        ..WarpLimits::default()
    };
    assert!(matches!(
        WarpPlan::new(&source(), &s),
        Err(VortexError::Refused { .. })
    ));
    let mut s = spec(Method::Near);
    s.limits.max_chunks = 0;
    assert!(matches!(
        WarpPlan::new(&source(), &s),
        Err(VortexError::Refused { .. })
    ));
}

#[test]
fn a_rotated_source_is_refused() {
    for (row, column) in [(0.001, 0.0), (0.0, 0.001), (1e-300, 0.0), (0.001, -0.001)] {
        let mut s = source();
        s.transform.row_rotation = row;
        s.transform.column_rotation = column;
        match WarpPlan::new(&s, &spec(Method::Near)) {
            Err(VortexError::Refused { what, reason }) => {
                assert_eq!(what, "source geotransform");
                assert!(reason.contains("rotated"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    // Signed zero is not a rotation.
    let mut s = source();
    s.transform.row_rotation = -0.0;
    s.transform.column_rotation = -0.0;
    assert!(WarpPlan::new(&s, &spec(Method::Near)).is_ok());
}

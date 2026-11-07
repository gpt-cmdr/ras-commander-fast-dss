//! `GridFrame` invariants: every malformed input is a typed error.

use dss_vortex::*;

fn parts() -> GridFrameParts {
    GridFrameParts {
        cells: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
        nx: 3,
        ny: 2,
        row_order: RowOrder::NorthToSouth,
        transform: GeoTransform::axis_aligned(100.0, 2000.0, 8000.0, -2000.0),
        crs: Crs::Named(NamedCrs::Shg),
        nodata: Some(-9999.0),
        time: FrameTime::Period {
            start: (45000, 0),
            end: (45000, 3600),
        },
        data_type: DataType::PerCum,
        units: "MM".into(),
        variable: "APCP".into(),
        description: None,
    }
}

#[test]
fn valid_frame_exposes_cells_and_rows() {
    let f = GridFrame::new(parts()).unwrap();
    assert_eq!(f.cell(2, 1), Some(6.0));
    assert_eq!(f.cell(3, 0), None);
    assert_eq!(f.cell(0, 2), None);
    assert_eq!(f.row(1), Some(&[4.0, 5.0, 6.0][..]));
    assert_eq!(f.row(2), None);
}

#[test]
fn dimension_errors() {
    let mut p = parts();
    p.nx = 0;
    assert_eq!(
        GridFrame::new(p).unwrap_err(),
        FrameError::EmptyGrid { nx: 0, ny: 2 }
    );
    let mut p = parts();
    p.cells.pop();
    assert_eq!(
        GridFrame::new(p).unwrap_err(),
        FrameError::CellCountMismatch {
            expected: 6,
            actual: 5
        }
    );
    for (nx, ny) in [(usize::MAX, 2), (1 << 16, 1 << 16)] {
        let mut p = parts();
        (p.nx, p.ny) = (nx, ny);
        assert_eq!(
            GridFrame::new(p).unwrap_err(),
            FrameError::GridTooLarge { nx, ny }
        );
    }
}

#[test]
fn transform_errors() {
    for bad in [f64::NAN, f64::INFINITY] {
        let mut p = parts();
        p.transform.origin_x = bad;
        assert_eq!(
            GridFrame::new(p).unwrap_err(),
            FrameError::NonFiniteTransform
        );
    }
    let mut p = parts();
    p.transform.dx = 0.0;
    assert_eq!(
        GridFrame::new(p).unwrap_err(),
        FrameError::DegenerateTransform
    );
    let mut p = parts();
    p.row_order = RowOrder::SouthToNorth;
    assert!(matches!(
        GridFrame::new(p).unwrap_err(),
        FrameError::RowOrderMismatch { .. }
    ));
}

#[test]
fn time_errors() {
    let mut p = parts();
    p.time = FrameTime::Period {
        start: (45000, 3600),
        end: (45000, 3600),
    };
    assert!(matches!(
        GridFrame::new(p).unwrap_err(),
        FrameError::InvalidTime { .. }
    ));
    let mut p = parts();
    p.data_type = DataType::InstVal;
    p.time = FrameTime::Instant((45000, 86_401));
    assert!(matches!(
        GridFrame::new(p).unwrap_err(),
        FrameError::InvalidTime { .. }
    ));
    // Extreme julian days from hostile bytes must not overflow.
    let mut p = parts();
    p.time = FrameTime::Period {
        start: (i64::MAX, 0),
        end: (i64::MIN, 0),
    };
    assert!(GridFrame::new(p).is_err());
    let mut p = parts();
    p.time = FrameTime::Instant((45000, 0));
    assert_eq!(
        GridFrame::new(p).unwrap_err(),
        FrameError::DataTypeTimeMismatch {
            data_type: DataType::PerCum
        }
    );
    let mut p = parts();
    p.data_type = DataType::InstVal;
    assert_eq!(
        GridFrame::new(p).unwrap_err(),
        FrameError::DataTypeTimeMismatch {
            data_type: DataType::InstVal
        }
    );
}

#[test]
fn data_type_codes_match_the_dss_writer() {
    use dss_core::grid_write::{INST_CUM, INST_VAL, PER_AVER, PER_CUM};
    assert_eq!(DataType::PerAver.dss_code(), PER_AVER);
    assert_eq!(DataType::PerCum.dss_code(), PER_CUM);
    assert_eq!(DataType::InstVal.dss_code(), INST_VAL);
    assert_eq!(DataType::InstCum.dss_code(), INST_CUM);
}

#[test]
fn nodata_comparison() {
    let f = GridFrame::new(parts()).unwrap();
    assert!(f.is_nodata(-9999.0));
    assert!(!f.is_nodata(f32::NAN));
    let mut p = parts();
    p.nodata = Some(f32::NAN);
    let f = GridFrame::new(p).unwrap();
    assert!(f.is_nodata(f32::from_bits(0x7fc0_0001)));
    assert!(!f.is_nodata(-9999.0));
    let mut p = parts();
    p.nodata = None;
    assert!(!GridFrame::new(p).unwrap().is_nodata(-9999.0));
}

#[test]
fn row_order_flip_keeps_bits_and_georeference() {
    let mut p = parts();
    let signalling = f32::from_bits(0x7f80_0001);
    p.cells[0] = signalling;
    let f = GridFrame::new(p).unwrap();
    let s = f.clone().with_row_order(RowOrder::SouthToNorth).unwrap();
    assert_eq!(s.row(0), Some(&[4.0, 5.0, 6.0][..]));
    assert_eq!(s.cell(0, 1).unwrap().to_bits(), signalling.to_bits());
    let t = s.parts().transform;
    assert_eq!((t.origin_y, t.dy), (4000.0, 2000.0));
    let back = s.with_row_order(RowOrder::NorthToSouth).unwrap();
    assert_eq!(back.parts().transform, f.parts().transform);
    assert_eq!(back.cells()[1..], f.cells()[1..]);
    assert_eq!(back.cells()[0].to_bits(), signalling.to_bits());
}

#[test]
fn rotated_frame_refuses_a_row_flip() {
    let mut p = parts();
    p.transform.row_rotation = 10.0;
    let f = GridFrame::new(p).unwrap();
    assert_eq!(
        f.with_row_order(RowOrder::SouthToNorth).unwrap_err(),
        FrameError::RotatedRowFlip
    );
}

#[test]
fn errors_convert_and_display() {
    let e: VortexError = FrameError::NonFiniteTransform.into();
    assert_eq!(e.to_string(), "transform has a non-finite coefficient");
    assert!(std::error::Error::source(&e).is_some());
    let e = VortexError::Refused {
        what: "datum shift".into(),
        reason: "no grid supplied".into(),
    };
    assert!(e.to_string().contains("no grid supplied"));
}

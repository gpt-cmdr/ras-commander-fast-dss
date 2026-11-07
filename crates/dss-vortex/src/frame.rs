//! `GridFrame`: one time step of one variable on one grid.
//!
//! Sources produce frames, tools transform them, and the DSS sink consumes them. Cells keep
//! their stored `f32` bits. A frame states its row order rather than assuming one, because
//! DSS grids run south to north (`zStructSpatialGrid.h`: the origin is the lower-left corner
//! of cell 0,0) and a source may run either way.

use crate::error::FrameError;
use dss_core::grid_write::{INST_CUM, INST_VAL, PER_AVER, PER_CUM};

/// Direction of the rows in the cell buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOrder {
    /// Row 0 is the northernmost; the transform's row spacing is negative.
    NorthToSouth,
    /// Row 0 is the southernmost, as DSS stores grids; the row spacing is positive.
    SouthToNorth,
}

/// Affine map from (column, row) to CRS coordinates, with the origin at the outer corner of
/// the buffer's first cell, in GDAL's geotransform order so `f64` values carry over unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoTransform {
    pub origin_x: f64,
    pub dx: f64,
    pub row_rotation: f64,
    pub origin_y: f64,
    pub column_rotation: f64,
    pub dy: f64,
}

impl GeoTransform {
    /// A transform with no rotation.
    pub fn axis_aligned(origin_x: f64, dx: f64, origin_y: f64, dy: f64) -> Self {
        GeoTransform {
            origin_x,
            dx,
            row_rotation: 0.0,
            origin_y,
            column_rotation: 0.0,
            dy,
        }
    }

    fn is_rotated(&self) -> bool {
        self.row_rotation != 0.0 || self.column_rotation != 0.0
    }

    fn validate(&self, order: RowOrder) -> Result<(), FrameError> {
        let coefficients = [
            self.origin_x,
            self.dx,
            self.row_rotation,
            self.origin_y,
            self.column_rotation,
            self.dy,
        ];
        if coefficients.iter().any(|c| !c.is_finite()) {
            return Err(FrameError::NonFiniteTransform);
        }
        let determinant = self.dx * self.dy - self.row_rotation * self.column_rotation;
        if determinant == 0.0 || !determinant.is_finite() {
            return Err(FrameError::DegenerateTransform);
        }
        let agrees = match order {
            RowOrder::NorthToSouth => self.dy < 0.0,
            RowOrder::SouthToNorth => self.dy > 0.0,
        };
        if !self.is_rotated() && !agrees {
            return Err(FrameError::RowOrderMismatch { order, dy: self.dy });
        }
        Ok(())
    }
}

/// Grids DSS names without a definition string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedCrs {
    /// Standard Hydrologic Grid (`dss_core::grid_write::AlbersParameters::shg`).
    Shg,
    /// HRAP (DSS record types 410 and 411).
    Hrap,
}

/// The frame's coordinate reference system, as the source stated it. Parsing and
/// transformation belong to the CRS module.
#[derive(Debug, Clone, PartialEq)]
pub enum Crs {
    Named(NamedCrs),
    Wkt(String),
    Proj(String),
    /// The source gave none.
    Undefined,
}

/// HEC grid data type (`zStructSpatialGrid.h` `dataType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    PerAver,
    PerCum,
    InstVal,
    InstCum,
}

impl DataType {
    /// The code `dss_core::grid_write::GridInput` takes.
    pub fn dss_code(self) -> i32 {
        match self {
            DataType::PerAver => PER_AVER,
            DataType::PerCum => PER_CUM,
            DataType::InstVal => INST_VAL,
            DataType::InstCum => INST_CUM,
        }
    }

    fn is_period(self) -> bool {
        matches!(self, DataType::PerAver | DataType::PerCum)
    }
}

/// A frame's place in time. Times are (julian day, seconds past midnight), the pairs
/// `dss_core::grid_write::parse_grid_time` returns; 86400 seconds is 2400 of the same day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTime {
    /// No time: a time-invariant grid, or an instantaneous grid with empty D and E parts.
    Invariant,
    Instant((i64, i64)),
    /// A period, stored in DSS as a D part and a later E part.
    Period {
        start: (i64, i64),
        end: (i64, i64),
    },
}

impl FrameTime {
    fn validate(&self) -> Result<(), FrameError> {
        let in_day = |t: &(i64, i64)| (0..=86_400).contains(&t.1);
        match self {
            FrameTime::Invariant => Ok(()),
            FrameTime::Instant(t) if in_day(t) => Ok(()),
            FrameTime::Period { start, end } if in_day(start) && in_day(end) => {
                // i128: julian days can come straight from file bytes.
                let seconds = |t: &(i64, i64)| i128::from(t.0) * 86_400 + i128::from(t.1);
                if seconds(end) > seconds(start) {
                    Ok(())
                } else {
                    Err(FrameError::InvalidTime {
                        reason: "period does not end after it starts",
                    })
                }
            }
            _ => Err(FrameError::InvalidTime {
                reason: "seconds past midnight outside 0..=86400",
            }),
        }
    }
}

/// Everything a frame holds, before validation.
#[derive(Debug, Clone, PartialEq)]
pub struct GridFrameParts {
    pub cells: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
    pub row_order: RowOrder,
    pub transform: GeoTransform,
    pub crs: Crs,
    /// The value marking a missing cell. `Some(NaN)` makes every NaN missing.
    pub nodata: Option<f32>,
    pub time: FrameTime,
    pub data_type: DataType,
    pub units: String,
    /// The variable's name in its source.
    pub variable: String,
    /// The source's longer description, when it has one.
    pub description: Option<String>,
}

/// The checks [`GridFrame::new`] makes on everything but the cells, for a source that plans a run from
/// metadata: a header that passes here, with `nx * ny` cells, is a frame. This file's test holds the
/// two to the same answer.
pub(crate) fn check_header(
    nx: usize,
    ny: usize,
    row_order: RowOrder,
    transform: &GeoTransform,
    time: &FrameTime,
    data_type: DataType,
) -> Result<(), FrameError> {
    if nx == 0 || ny == 0 {
        return Err(FrameError::EmptyGrid { nx, ny });
    }
    nx.checked_mul(ny)
        .filter(|n| i32::try_from(*n).is_ok())
        .ok_or(FrameError::GridTooLarge { nx, ny })?;
    transform.validate(row_order)?;
    time.validate()?;
    let time_fits = match time {
        FrameTime::Period { .. } => data_type.is_period(),
        FrameTime::Instant(_) => !data_type.is_period(),
        FrameTime::Invariant => true,
    };
    if !time_fits {
        return Err(FrameError::DataTypeTimeMismatch { data_type });
    }
    Ok(())
}

/// A validated frame. See [`GridFrameParts`] for the fields.
#[derive(Debug, Clone, PartialEq)]
pub struct GridFrame {
    parts: GridFrameParts,
}

impl GridFrame {
    /// Validate the parts. Malformed input from any source is an error, never a panic.
    pub fn new(parts: GridFrameParts) -> Result<Self, FrameError> {
        let (nx, ny) = (parts.nx, parts.ny);
        if nx == 0 || ny == 0 {
            return Err(FrameError::EmptyGrid { nx, ny });
        }
        // DSS grid records store the counts and the data size as 32-bit integers.
        let expected = nx
            .checked_mul(ny)
            .filter(|n| i32::try_from(*n).is_ok())
            .ok_or(FrameError::GridTooLarge { nx, ny })?;
        if parts.cells.len() != expected {
            return Err(FrameError::CellCountMismatch {
                expected,
                actual: parts.cells.len(),
            });
        }
        parts.transform.validate(parts.row_order)?;
        parts.time.validate()?;
        let time_fits = match parts.time {
            FrameTime::Period { .. } => parts.data_type.is_period(),
            FrameTime::Instant(_) => !parts.data_type.is_period(),
            // grid_write::GridInput accepts empty D and E parts for time-invariant grids.
            FrameTime::Invariant => true,
        };
        if !time_fits {
            return Err(FrameError::DataTypeTimeMismatch {
                data_type: parts.data_type,
            });
        }
        Ok(GridFrame { parts })
    }

    pub fn parts(&self) -> &GridFrameParts {
        &self.parts
    }

    pub fn into_parts(self) -> GridFrameParts {
        self.parts
    }

    pub fn cells(&self) -> &[f32] {
        &self.parts.cells
    }

    /// The cell at a column and a buffer row, or `None` outside the grid.
    pub fn cell(&self, column: usize, row: usize) -> Option<f32> {
        if column >= self.parts.nx || row >= self.parts.ny {
            return None;
        }
        self.parts.cells.get(row * self.parts.nx + column).copied()
    }

    /// One buffer row, or `None` outside the grid.
    pub fn row(&self, row: usize) -> Option<&[f32]> {
        if row >= self.parts.ny {
            return None;
        }
        let start = row * self.parts.nx;
        self.parts.cells.get(start..start + self.parts.nx)
    }

    /// Whether a value is this frame's missing marker.
    pub fn is_nodata(&self, value: f32) -> bool {
        match self.parts.nodata {
            Some(n) if n.is_nan() => value.is_nan(),
            Some(n) => value == n,
            None => false,
        }
    }

    /// The same grid with its rows in `order`, cell bits unchanged. The new origin is
    /// computed in `f64` as `origin_y + ny * dy`. A rotated frame already in `order` is
    /// returned as it is; otherwise it is refused.
    pub fn with_row_order(self, order: RowOrder) -> Result<Self, FrameError> {
        let mut parts = self.parts;
        if parts.row_order == order {
            return Ok(GridFrame { parts });
        }
        let t = parts.transform;
        if t.is_rotated() {
            return Err(FrameError::RotatedRowFlip);
        }
        let mut flipped = Vec::with_capacity(parts.cells.len());
        for row in parts.cells.chunks_exact(parts.nx).rev() {
            flipped.extend_from_slice(row);
        }
        parts.cells = flipped;
        parts.transform.origin_y = t.origin_y + parts.ny as f64 * t.dy;
        parts.transform.dy = -t.dy;
        parts.row_order = order;
        GridFrame::new(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `check_header` and `GridFrame::new` must give one answer, error for error.
    #[test]
    fn a_header_passes_exactly_when_its_frame_does() {
        let day = (2_400_000, 3_600);
        let times = [
            FrameTime::Invariant,
            FrameTime::Instant(day),
            FrameTime::Instant((1, 86_401)),
            FrameTime::Period {
                start: day,
                end: (2_400_000, 7_200),
            },
            FrameTime::Period {
                start: day,
                end: day,
            },
        ];
        let types = [
            DataType::PerAver,
            DataType::PerCum,
            DataType::InstVal,
            DataType::InstCum,
        ];
        let transforms = [
            GeoTransform::axis_aligned(0.0, 1.0, 0.0, -1.0),
            GeoTransform::axis_aligned(0.0, 1.0, 0.0, 1.0),
            GeoTransform::axis_aligned(0.0, 0.0, 0.0, -1.0),
            GeoTransform::axis_aligned(f64::NAN, 1.0, 0.0, -1.0),
        ];
        let shapes = [(2usize, 3usize), (0, 3), (2, 0)];
        let (mut passed, mut failed) = (0, 0);
        for time in times {
            for data_type in types {
                for transform in transforms {
                    for order in [RowOrder::NorthToSouth, RowOrder::SouthToNorth] {
                        for (nx, ny) in shapes {
                            let header = check_header(nx, ny, order, &transform, &time, data_type);
                            let frame = GridFrame::new(GridFrameParts {
                                cells: vec![0.0; nx * ny],
                                nx,
                                ny,
                                row_order: order,
                                transform,
                                crs: Crs::Named(NamedCrs::Shg),
                                nodata: None,
                                time,
                                data_type,
                                units: String::new(),
                                variable: String::new(),
                                description: None,
                            })
                            .map(|_| ());
                            assert_eq!(header, frame, "{nx} x {ny} {time:?} {data_type:?}");
                            if header.is_ok() {
                                passed += 1;
                            } else {
                                failed += 1;
                            }
                        }
                    }
                }
            }
        }
        // Both outcomes are reached, so the comparison is not of errors alone.
        assert!(passed > 0 && failed > 0, "{passed} passed, {failed} failed");
        let huge = check_header(
            1 << 20,
            1 << 20,
            RowOrder::NorthToSouth,
            &transforms[0],
            &times[0],
            types[0],
        );
        assert!(matches!(huge, Err(FrameError::GridTooLarge { .. })));
    }
}

//! The arithmetic. Pure functions on cell buffers, in `f32`, in Vortex's own order of operations.
//!
//! Output buffers always mark a missing cell with [`GRID_NODATA`], so a result equal to the input's
//! own marker stays a value. Allocation is fallible: a grid is bounded before it gets here, and an
//! allocation failure must not abort the process.

use crate::error::VortexError;
use crate::sink_dss::reader_takes_as_missing;
use dss_core::grid_write::GRID_NODATA;

/// `math/Operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Multiply,
    Divide,
    Add,
    Subtract,
}

impl Operation {
    fn apply(self, a: f32, b: f32) -> f32 {
        match self {
            Operation::Multiply => a * b,
            Operation::Divide => a / b,
            Operation::Add => a + b,
            Operation::Subtract => a - b,
        }
    }
}

/// A cell no result may be computed from: NaN, infinite, or the marker's own bits. The same test as
/// `DssDataWriter.write`, and as [`crate::sink_dss::to_stored_cells`].
pub fn is_missing(value: f32, marker: Option<f32>) -> bool {
    value.is_nan() || value.is_infinite() || marker.is_some_and(|m| value.to_bits() == m.to_bits())
}

fn output_buffer(len: usize) -> Result<Vec<f32>, VortexError> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| VortexError::Refused {
            what: "grid".into(),
            reason: format!("no memory for {len} cells"),
        })?;
    Ok(out)
}

fn checked(input: f32, result: f32, how: &str) -> Result<f32, VortexError> {
    if result.is_finite() && !reader_takes_as_missing(result) {
        return Ok(result);
    }
    let vortex = if result == -901.0 || result == -902.0 {
        "store the number, and this library's reader then finds it missing"
    } else {
        "store it as missing"
    };
    Err(VortexError::Refused {
        what: "cell value".into(),
        reason: format!(
            "the valid cell {input} {how} gives {result}, which a DSS grid cannot hold as a value: \
             readers would take as missing what was written as a value. Observed behaviour we did \
             not implement: Vortex 1.1.0 and 0.13.3 {vortex}"
        ),
    })
}

/// `Calculator.calculate`: one operation by one constant, on valid cells only.
pub fn with_constant(
    cells: &[f32],
    marker: Option<f32>,
    operation: Operation,
    value: f32,
) -> Result<Vec<f32>, VortexError> {
    let mut out = output_buffer(cells.len())?;
    for &cell in cells {
        out.push(if is_missing(cell, marker) {
            GRID_NODATA
        } else {
            checked(cell, operation.apply(cell, value), "under the operation")?
        });
    }
    Ok(out)
}

/// `GridCalculator.calculate`, cell by cell, with the input on the left. A cell missing on either
/// side is missing in the result: observed behaviour we did not implement, see the module note.
pub fn with_raster(
    cells: &[f32],
    marker: Option<f32>,
    raster: &[f32],
    raster_marker: Option<f32>,
    operation: Operation,
) -> Result<Vec<f32>, VortexError> {
    if cells.len() != raster.len() {
        return Err(VortexError::Refused {
            what: "raster calculator".into(),
            reason: format!(
                "the input has {} cells and the raster {}",
                cells.len(),
                raster.len()
            ),
        });
    }
    let mut out = output_buffer(cells.len())?;
    for (&cell, &other) in cells.iter().zip(raster) {
        out.push(
            if is_missing(cell, marker) || is_missing(other, raster_marker) {
                GRID_NODATA
            } else {
                checked(
                    cell,
                    operation.apply(cell, other),
                    "under the raster's operation",
                )?
            },
        );
    }
    Ok(out)
}

/// `Sanitizer`'s fields. Thresholds are doubles and replacements floats, as there; a replacement of
/// NaN or HEC's marker means missing. A cell at either threshold is replaced (`<=` and `>=`), and
/// the minimum is tested first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub minimum: f64,
    pub maximum: f64,
    pub minimum_replacement: f32,
    pub maximum_replacement: f32,
}

impl Default for Thresholds {
    /// `Sanitizer.Builder`'s defaults.
    fn default() -> Self {
        Thresholds {
            minimum: -f64::MAX,
            maximum: f64::MAX,
            minimum_replacement: f32::NAN,
            maximum_replacement: f32::NAN,
        }
    }
}

/// `Sanitizer.sanitize`, on valid cells only: observed behaviour we did not implement, see the
/// module note. The comparison widens the cell to `f64`, as Java does.
pub fn sanitize(
    cells: &[f32],
    marker: Option<f32>,
    t: &Thresholds,
) -> Result<Vec<f32>, VortexError> {
    let missing_if_nan = |v: f32| if v.is_nan() { GRID_NODATA } else { v };
    let mut out = output_buffer(cells.len())?;
    for &cell in cells {
        out.push(if is_missing(cell, marker) {
            GRID_NODATA
        } else if f64::from(cell) <= t.minimum {
            missing_if_nan(t.minimum_replacement)
        } else if f64::from(cell) >= t.maximum {
            missing_if_nan(t.maximum_replacement)
        } else {
            cell
        });
    }
    Ok(out)
}

/// `new float[length]`, fallibly.
pub fn zero_totals(len: usize) -> Result<Vec<f32>, VortexError> {
    let mut out = output_buffer(len)?;
    out.resize(len, 0.0);
    Ok(out)
}

/// `Normalizer.normalize`, source totals: a missing cell adds 0, as NaN does there. The sum is in
/// `f32`, in the caller's order, which changes the last bits: see [`super::java_order`].
pub fn add_source(
    totals: &mut [f32],
    cells: &[f32],
    marker: Option<f32>,
) -> Result<(), VortexError> {
    same_length(totals, cells)?;
    for (total, &cell) in totals.iter_mut().zip(cells) {
        *total += if is_missing(cell, marker) { 0.0 } else { cell };
    }
    Ok(())
}

/// `Normalizer.normalize`, normal totals: a missing cell makes the total NaN, as NaN does there, and
/// [`normalize_counted`] writes that cell as missing. One missing normal in an interval is enough.
pub fn add_normal(
    totals: &mut [f32],
    cells: &[f32],
    marker: Option<f32>,
) -> Result<(), VortexError> {
    same_length(totals, cells)?;
    for (total, &cell) in totals.iter_mut().zip(cells) {
        *total += if is_missing(cell, marker) {
            f32::NAN
        } else {
            cell
        };
    }
    Ok(())
}

// A shorter grid must not leave the rest of the totals as they were.
fn same_length(totals: &[f32], cells: &[f32]) -> Result<(), VortexError> {
    if totals.len() == cells.len() {
        return Ok(());
    }
    Err(VortexError::Refused {
        what: "normalizer".into(),
        reason: format!(
            "a grid of {} cells cannot join totals of {}",
            cells.len(),
            totals.len()
        ),
    })
}

/// `Normalizer.normalize`, one output grid: `normalTotal / sourceTotal * value`, left to right in
/// `f32`. A missing input stays missing, whatever its total, and a missing normal gives missing,
/// even where the source's total is 0. After those two, Vortex's branch: a total of 0 gives 0.
pub fn normalize(
    cells: &[f32],
    marker: Option<f32>,
    source_totals: &[f32],
    normal_totals: &[f32],
) -> Result<Vec<f32>, VortexError> {
    Ok(normalize_counted(cells, marker, source_totals, normal_totals)?.0)
}

/// [`normalize`], and how many valid source cells it wrote as missing because their normal was
/// missing. A cell already missing in the source is not among them. By maintainer decision, a result
/// holds no unscaled cell. Observed behaviour we did not implement: on a DSS normal, Vortex uses
/// -FLT_MAX in the normal total; on NaN, `Normalizer.normalize` passes the source through.
pub fn normalize_counted(
    cells: &[f32],
    marker: Option<f32>,
    source_totals: &[f32],
    normal_totals: &[f32],
) -> Result<(Vec<f32>, u64), VortexError> {
    if cells.len() != source_totals.len() || cells.len() != normal_totals.len() {
        return Err(VortexError::Refused {
            what: "normalizer".into(),
            reason: "the grids and totals differ in length".into(),
        });
    }
    let mut out = output_buffer(cells.len())?;
    let mut made_missing = 0u64;
    for ((&cell, &source), &normal) in cells.iter().zip(source_totals).zip(normal_totals) {
        out.push(if is_missing(cell, marker) {
            GRID_NODATA
        } else if normal.is_nan() {
            made_missing += 1;
            GRID_NODATA
        } else if source == 0.0 {
            0.0
        } else {
            checked(cell, normal / source * cell, "under the normal's scale")?
        });
    }
    Ok((out, made_missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Option<f32> = Some(GRID_NODATA);

    fn bits(v: &[f32]) -> Vec<u32> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    // `CalculatorTest`, and every other operation.
    #[test]
    fn constant_operations_match_vortexs_unit_tests() {
        let data = [0.0, -2.0, 2.0, 0.25];
        let run = |op, v| with_constant(&data, M, op, v).unwrap();
        assert_eq!(run(Operation::Multiply, 2.0), [0.0, -4.0, 4.0, 0.5]);
        assert_eq!(run(Operation::Divide, 2.0), [0.0, -1.0, 1.0, 0.125]);
        assert_eq!(run(Operation::Add, 2.0), [2.0, 0.0, 4.0, 2.25]);
        assert_eq!(run(Operation::Subtract, 2.0), [-2.0, -4.0, 0.0, -1.75]);
    }

    #[test]
    fn a_missing_cell_is_never_an_operand() {
        let data = [GRID_NODATA, f32::NAN, f32::INFINITY, 3.0];
        let out = with_constant(&data, M, Operation::Multiply, 0.5).unwrap();
        assert_eq!(
            bits(&out),
            bits(&[GRID_NODATA, GRID_NODATA, GRID_NODATA, 1.5])
        );
        let raster = [2.0, 2.0, 2.0, GRID_NODATA];
        let out = with_raster(&data, M, &raster, M, Operation::Multiply).unwrap();
        assert_eq!(bits(&out), bits(&[GRID_NODATA; 4]));
    }

    #[test]
    fn a_valid_cell_equal_to_another_marker_stays_valid() {
        // The input's marker is -9999; a computed -9999 is a value, and only the input's -9999 is not.
        let out =
            with_constant(&[-9999.0, 4999.5], Some(-9999.0), Operation::Multiply, -2.0).unwrap();
        assert_eq!(bits(&out), bits(&[GRID_NODATA, -9999.0]));
        // A valid cell equal to a raster's marker in the input stays valid too.
        let out = with_raster(&[-9999.0], M, &[1.0], Some(-9999.0), Operation::Add).unwrap();
        assert_eq!(bits(&out), bits(&[-9998.0]));
    }

    #[test]
    fn a_result_no_grid_can_hold_is_refused() {
        for (cell, op, v) in [
            (1.0, Operation::Divide, 0.0),
            (0.0, Operation::Divide, 0.0),
            (f32::MAX, Operation::Multiply, 2.0),
            (-f32::MAX / 2.0, Operation::Multiply, 2.0),
        ] {
            let e = with_constant(&[cell], M, op, v).unwrap_err().to_string();
            assert!(e.contains("cannot hold"), "{e}");
            assert!(e.contains("Observed behaviour we did not implement"), "{e}");
        }
        let e = with_raster(&[1.0], M, &[0.0], M, Operation::Divide).unwrap_err();
        assert!(e.to_string().contains("cannot hold"));
    }

    #[test]
    fn rasters_of_another_length_are_refused() {
        let e = with_raster(&[1.0, 2.0], M, &[1.0], M, Operation::Add).unwrap_err();
        assert!(e.to_string().contains("2 cells"), "{e}");
    }

    #[test]
    fn thresholds_are_inclusive_and_the_minimum_comes_first() {
        let t = Thresholds {
            minimum: 1.0,
            maximum: 3.0,
            minimum_replacement: -1.0,
            maximum_replacement: 9.0,
        };
        let out = sanitize(&[0.5, 1.0, 2.0, 3.0, 4.0], M, &t).unwrap();
        assert_eq!(out, [-1.0, -1.0, 2.0, 9.0, 9.0]);
        let crossed = Thresholds {
            minimum: 5.0,
            maximum: 1.0,
            ..t
        };
        assert_eq!(sanitize(&[3.0], M, &crossed).unwrap(), [-1.0]);
    }

    #[test]
    fn sanitizer_compares_in_double() {
        // 0.1f32 is above 0.1f64, so it is not at or below the threshold; in f32 it would be.
        let t = Thresholds {
            minimum: 0.1,
            minimum_replacement: -1.0,
            ..Thresholds::default()
        };
        assert_eq!(sanitize(&[0.1f32], M, &t).unwrap(), [0.1f32]);
    }

    #[test]
    fn sanitizer_leaves_missing_cells_missing() {
        let t = Thresholds {
            minimum: 0.0,
            maximum: 10.0,
            minimum_replacement: 0.0,
            maximum_replacement: 10.0,
        };
        let data = [
            GRID_NODATA,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.0,
        ];
        let out = sanitize(&data, M, &t).unwrap();
        assert_eq!(
            bits(&out),
            bits(&[GRID_NODATA, GRID_NODATA, GRID_NODATA, GRID_NODATA, 0.0])
        );
        // The default replacement means missing.
        let out = sanitize(
            &[-1.0],
            M,
            &Thresholds {
                minimum: 0.0,
                ..Thresholds::default()
            },
        )
        .unwrap();
        assert_eq!(bits(&out), bits(&[GRID_NODATA]));
    }

    // `NormalizerTest.DtoArraysAreNormalized`, with Vortex's own numbers, NaN standing for missing.
    // Cell 2 is the one change from Vortex's expected values: its normal is NaN, so it is missing here,
    // where Vortex keeps 2.0 in both grids.
    #[test]
    fn normalizer_matches_vortexs_unit_test() {
        let a = [0.0, f32::NAN, 2.0, 4.0, 2.0, 2.0, 2.0, 2.0];
        let b = [0.0, 2.0, 2.0, 4.0, 2.0, 2.0, 2.0, 1.0];
        let normal = [0.0, 2.0, f32::NAN, 6.0, 2.0, 8.0, 2.0, 2.0];
        let mut source = zero_totals(8).unwrap();
        add_source(&mut source, &a, M).unwrap();
        add_source(&mut source, &b, M).unwrap();
        let mut normals = zero_totals(8).unwrap();
        add_normal(&mut normals, &normal, M).unwrap();
        let out_a = normalize(&a, M, &source, &normals).unwrap();
        let out_b = normalize(&b, M, &source, &normals).unwrap();
        assert_eq!(
            bits(&out_a),
            bits(&[
                0.0,
                GRID_NODATA,
                GRID_NODATA,
                3.0,
                1.0,
                4.0,
                1.0,
                2.0f32 / 3.0 * 2.0
            ])
        );
        assert_eq!(
            bits(&out_b),
            bits(&[0.0, 2.0, GRID_NODATA, 3.0, 1.0, 4.0, 1.0, 2.0f32 / 3.0])
        );
    }

    #[test]
    fn normalizer_keeps_a_missing_cell_whose_total_is_zero() {
        let a = [GRID_NODATA];
        let b = [0.0];
        let mut source = zero_totals(1).unwrap();
        add_source(&mut source, &a, M).unwrap();
        add_source(&mut source, &b, M).unwrap();
        let normals = [5.0];
        assert_eq!(
            bits(&normalize(&a, M, &source, &normals).unwrap()),
            bits(&[GRID_NODATA])
        );
        assert_eq!(normalize(&b, M, &source, &normals).unwrap(), [0.0]);
    }

    #[test]
    fn normalizer_writes_a_cell_with_no_normal_as_missing_and_counts_it() {
        let a = [3.0, GRID_NODATA, 0.0, 2.0];
        let source = [3.0, 5.0, 0.0, 2.0];
        let normals = [f32::NAN, f32::NAN, f32::NAN, 4.0];
        let (out, made_missing) = normalize_counted(&a, M, &source, &normals).unwrap();
        // The first and the third, and a total of 0 does not save the third. The second was missing
        // already, so it is not counted.
        assert_eq!(
            bits(&out),
            bits(&[GRID_NODATA, GRID_NODATA, GRID_NODATA, 4.0])
        );
        assert_eq!(made_missing, 2);
    }

    #[test]
    fn normalizer_with_no_normal_anywhere_writes_every_cell_missing() {
        let a = [1.0, 0.0, GRID_NODATA];
        let source = [1.0, 0.0, 0.0];
        let normals = [f32::NAN; 3];
        let (out, made_missing) = normalize_counted(&a, M, &source, &normals).unwrap();
        assert_eq!(bits(&out), bits(&[GRID_NODATA; 3]));
        assert_eq!(made_missing, 2);
    }

    #[test]
    fn normalizer_takes_one_missing_normal_in_an_interval_as_no_normal() {
        let mut normals = zero_totals(2).unwrap();
        add_normal(&mut normals, &[GRID_NODATA, 1.0], M).unwrap();
        add_normal(&mut normals, &[4.0, 1.0], M).unwrap();
        let (out, made_missing) = normalize_counted(&[2.0, 2.0], M, &[2.0, 2.0], &normals).unwrap();
        assert_eq!(bits(&out), bits(&[GRID_NODATA, 2.0]));
        assert_eq!(made_missing, 1);
    }

    #[test]
    fn normalizer_refuses_what_no_grid_can_hold() {
        let e = normalize(&[1e30], M, &[1e-30], &[1e30]).unwrap_err();
        assert!(e.to_string().contains("cannot hold"), "{e}");
        assert!(normalize(&[1.0, 2.0], M, &[1.0], &[1.0]).is_err());
    }

    #[test]
    fn the_order_is_left_to_right_in_single_precision() {
        // normal / source * value, not normal * value / source.
        let (n, s, v) = (1.0f32, 41.0f32, 41.0f32);
        assert_ne!((n / s * v).to_bits(), (n * v / s).to_bits());
        let out = normalize(&[v], M, &[s], &[n]).unwrap();
        assert_eq!(out[0].to_bits(), (n / s * v).to_bits());
    }
}

//! Grid tools: calculator, sanitizer, normalizer and clipper, replacing the Vortex tools of those
//! names (`math/Calculator`, `math/GridCalculator`, `math/Sanitizer`, `math/Normalizer`,
//! `geo/BatchSubsetter`). No Java runs here; Vortex is used only as a test oracle.
//!
//! This module is called `grid_tools` and not `math`, Vortex's package name, so that
//! it cannot be confused with [`crate::maths`], the portable floating-point functions. Everything
//! here is IEEE `+ - * /` on `f32`, which is exact on every platform and needs no portable
//! function; the warp behind the clipper and the raster calculator uses `maths` already.
//!
//! A run follows the importer's phases. **Planning** decides every record, then computes every
//! frame once and discards it, so a refused value leaves the output untouched and does not create
//! it. **Writing** computes each frame again and stores it through the P24 writer, one at a time.
//! A failure there keeps the finished records, and the writer guarantees no partial record. The
//! error's `completed` lists the writes that were acknowledged, and `in_doubt` the one that was not:
//! it may or may not be in the file, since a failure late in a write can follow its publication.
//!
//! Observed behaviour we did not implement, on Vortex 1.1.0 and 0.13.3. Each is a test in
//! `tests/grid_tools.rs`:
//!
//! - **A missing cell as an operand.** `GridCalculator` applies the operation to every cell, so HEC's
//!   marker (-FLT_MAX) in the input and the raster's marker are both used as numbers. Here a cell
//!   missing on either side is missing in the result.
//! - **The normalizer's marker.** `Normalizer` tests only NaN, so -FLT_MAX is summed into the totals
//!   and scaled. Here a missing source cell is left out of the total and stays missing. A missing
//!   normal cell means no normal for that cell, and the cell is written as missing, by maintainer
//!   decision, so a result never mixes scaled and unscaled cells. That also differs from what Vortex
//!   does for NaN, which passes the source through. A cell with no normal is missing even where the
//!   source's total is 0, and a missing source cell whose total is 0 stays missing; Vortex gives each 0.
//! - **The sanitizer on a missing cell.** `Sanitizer` compares -FLT_MAX and infinity with the
//!   thresholds, so a stated replacement turns them into values. Here they stay missing.
//! - **-901, -902 and a specified grid's null value** in a DSS input are missing here, as in this
//!   library's reader (`Grid::is_missing`). Vortex computes on them as numbers.
//! - **A result a DSS grid cannot hold.** A valid cell that divides by zero, overflows, or lands on
//!   the marker is stored as missing by `DssDataWriter.write`. Here the run refuses, before writing.
//!   So does one landing on -901 or -902: Vortex stores the number, and this library's reader would
//!   take it as missing, so a successful write would lose it on the next read.
//! - **A DSS raster.** `BatchGridCalculator.getRaster` builds its `DataReader` with no variable name, so
//!   Vortex fails on a DSS raster before computing. This library reads one, as its own extension.
//!   A timed one and an input grid stating different times are refused, unless
//!   [`RasterRequest::static_raster`] states that one grid is a factor for all of them.
//! - **A cell zero away from the CRS's origin.** `dssToDto` ignores it, moving the grid. Refused.
//! - **A marker other than HEC's.** A computed cell equal to a non-DSS input's marker is stored as
//!   missing by `DssDataWriter.write`. Here it stays a value.
//! - **Grids that do not agree.** `Normalizer.validate` compares only `nx`, `ny`, `dx` and `dy`, and
//!   skips the interval on a mismatch; `BatchGridCalculator` resamples for the first grid only. Here
//!   a different origin, CRS, shape or unit is refused.
//! - **A record's meaning.** `DssDataWriter.write` gives every record the type its C part and window
//!   imply, whatever it was. Here, a different type must be stated. `getCPart` also searches the
//!   input's whole path for a variable name; here a C part comes from the record.
//! - **Skipped requests.** Vortex logs and carries on for a name the file does not hold, a geographic
//!   grid to clip, and an interval with sources and no normals. Here each is refused.

pub mod cells;
pub mod dss_source;
pub mod java_order;
pub mod run;

pub use cells::{Operation, Thresholds};
pub use dss_source::DssSource;
pub use run::{
    calculate, calculate_raster, clip, normalize, sanitize, ClipBounds, ClipRequest, IntervalNote,
    NormalizeOutcome, NormalizeRequest, OutputOptions, RasterRequest, ToolRecord,
};

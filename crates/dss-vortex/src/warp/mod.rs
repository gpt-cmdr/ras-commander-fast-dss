//! Warping one grid to another: `near`, `bilinear` and `average`, reproducing GDAL 3.2.1 as HEC
//! Vortex drives it from `geo/Resampler.java`.
//!
//! The per-case checks against GDAL output are in `tests/p41_warp_cases.rs`.
//! All maths goes through [`crs::maths`](crate::crs::maths). Only exact operations (`floor`,
//! `ceil`, `abs`, `sqrt`, `trunc`) come from `std`; `tests/warp_maths_gate.rs` enforces that.
//!
//! Observed behaviour we did not implement: with no envelope, or with no cell size, Vortex 1.1.0
//! and 0.13.3 use GDAL's suggested output extent, whose origin is not on a whole multiple of the
//! cell size, and the DSS write then rounds the origin to whole cells. This library refuses such
//! a grid by default, and [`Align::Vortex`] returns the extent as Vortex's warp computes it.
//!
//! Streaming: a [`WarpPlan`] holds the destination grid and chunk list, which depend only on the
//! georeference, and no cell data. Each frame costs one output buffer and one chunk at a time.
//!
//! Resources: [`WarpLimits`] bounds destination cells, the chunk list and the memory a warp may
//! hold. Workers count towards that memory and are capped, so a large `threads` costs no more than
//! the budget allows; their number never changes a result. Failing to allocate or to start a
//! worker is a typed refusal.
//!
//! Sources must be axis-aligned. A rotated geotransform is refused, because `VortexGrid` cannot
//! express one, so no comparison with Vortex covers this warp on such a grid.

mod extent;
mod kernel;
mod transformer;
mod window;

use crate::crs::{reproject_envelope, CrsDef, DatumPolicy, Envelope};
use crate::error::VortexError;
use crate::frame::{Crs, GeoTransform, GridFrame, GridFrameParts, NamedCrs, RowOrder};
use transformer::{gt_array, GenImg};
use window::{Chunk, ChunkBuilder};

/// Resampling methods Vortex's importer offers (`ResamplingMethod.java`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Near,
    Bilinear,
    Average,
}

impl Method {
    // GWKGetFilterRadius.
    fn filter_radius(self) -> i32 {
        match self {
            Method::Near | Method::Average => 0,
            Method::Bilinear => 1,
        }
    }
}

/// GDAL's working data type, which `GDALWarpResolveWorkingDataType` (`alg/gdalwarper.cpp`) unions
/// from the band type and the nodata value. It sets the memory cost, and so the chunks and the
/// values; and it sets the precision of the nodata comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkingType {
    Float32,
    Float64,
}

impl WorkingType {
    /// `GDALDataTypeUnionWithValue(GDT_Float32, nodata, FALSE)`: `GetMinBitsForValue`,
    /// `GDALFindDataType` and `GetMinBitsForPair` (`gcore/gdal_misc.cpp`). NaN and infinities take
    /// 64 bits, and a whole number that needs 32 bits takes 2 * 32 next to a float.
    pub(crate) fn for_nodata(nodata: f32) -> Self {
        let v = f64::from(nodata);
        if v.is_nan() || v.is_infinite() {
            return WorkingType::Float64;
        }
        if v.trunc() != v {
            return WorkingType::Float32;
        }
        let fits_16 = (-32768.0..=65535.0).contains(&v);
        if fits_16 {
            WorkingType::Float32
        } else {
            WorkingType::Float64
        }
    }

    fn pixel_cost_bits(self) -> f64 {
        match self {
            WorkingType::Float32 => 33.0,
            WorkingType::Float64 => 65.0,
        }
    }
}

/// What to do when the destination origin is not on whole cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// Refuse. This is the default: see the module's note on observed behaviour.
    #[default]
    Refuse,
    /// Accept the extent, as Vortex's warp computes it.
    Vortex,
}

/// The default bound on destination cells: 2^28, about 1 GiB of cells, the same as the GRIB2
/// reader's default. Every real product in the P41 and P44 matrices is well under it.
pub const DEFAULT_MAX_CELLS: u64 = 1 << 28;

/// A DSS grid stores its data size in an `i32`, so no plan may ask for more cells than this.
pub const FORMAT_MAX_CELLS: u64 = i32::MAX as u64;

/// The most worker threads a warp starts for a chunk, however many it is asked for.
pub const MAX_WARP_THREADS: usize = 64;

/// Bounds on what a plan may allocate. A larger request is a typed refusal. The default suits
/// untrusted georeferences; [`WarpLimits::format_maximum`] is the explicit opt-in for larger grids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarpLimits {
    /// Destination cells. Above [`FORMAT_MAX_CELLS`], that bound applies instead.
    pub max_cells: u64,
    /// Entries in the chunk list.
    pub max_chunks: usize,
    /// What one warp may hold at once, in bytes: the destination, the chunk lists, one chunk's
    /// source window, mask and row references, and every worker's scratch and stack. The caller's
    /// input and the frame it came in are not counted.
    pub max_memory_bytes: u64,
}

impl Default for WarpLimits {
    fn default() -> Self {
        WarpLimits {
            max_cells: DEFAULT_MAX_CELLS,
            max_chunks: 1 << 20,
            // The 1 GiB of DEFAULT_MAX_CELLS, and as much again for everything else.
            max_memory_bytes: 1 << 31,
        }
    }
}

impl WarpLimits {
    /// Allow grids up to the format's maximum, about 8 GiB of cells for each frame.
    pub fn format_maximum() -> Self {
        WarpLimits {
            max_cells: FORMAT_MAX_CELLS,
            max_chunks: 1 << 20,
            max_memory_bytes: 1 << 35,
        }
    }
}

/// An envelope, and the CRS it is stated in; `None` means the grid's own (`Resampler.build`).
#[derive(Debug, Clone, PartialEq)]
pub struct WarpEnvelope {
    pub envelope: Envelope,
    pub crs: Option<CrsDef>,
}

/// A cell size is a length in metres, as Vortex's is (`GeographicProcessor`, "Cell size units
/// defaults to meters"), and becomes the target's unit here. Only a Transverse Mercator can be in
/// feet, so for every other target this returns its argument, untouched.
///
/// - **Default ([`Align::Refuse`]): the CRS's own foot, at the spacing a DSS record stores.** A US
///   survey foot is 1200/3937 m and an international foot 0.3048 m, each applied as an exact ratio,
///   and the result is rounded to the nearest `f32`, which is what a grid record holds. The
///   destination lattice is then built, snapped and warped on that value, so the record's whole
///   cells are the lattice's by construction. 2,000 m is 6561.6665 US survey feet, 49.6 micrometres
///   short of 2 km; 10,000 m is 32,808.332, 0.4 mm short of 10 km. [`CellSize`] reports both.
/// - **[`Align::Vortex`]: Vortex's rule** (`Resampler.getTargetCellSize`), unrounded, as Vortex
///   plans with it. Its `ReferenceUtils.getLinearUnits` maps every foot, the US survey foot
///   included, to `USCustomary.FOOT`, the international one. So 10,000 m on a zone in US survey
///   feet is 32,808.399 of its units, which is 10,000.0198 m, and the snapped lattice moves with it
///   (measured on Vortex 1.1.0 and 0.13.3, `target.tm_feet`).
///
/// Each factor is a ratio of small integers: the product is exact for any cell size of 40 bits or
/// fewer, and the quotient then rounds once.
pub fn cell_size_in_target_units(metres: f64, target: &CrsDef, align: Align) -> f64 {
    use crate::crs::LinearUnit;
    match (LinearUnit::of_metres(target.metres_per_unit()), align) {
        (Some(LinearUnit::UsSurveyFoot), Align::Refuse) => {
            f64::from((metres * 3937.0 / 1200.0) as f32)
        }
        (Some(LinearUnit::InternationalFoot), Align::Refuse) => {
            f64::from((metres * 1250.0 / 381.0) as f32)
        }
        (Some(LinearUnit::UsSurveyFoot | LinearUnit::InternationalFoot), Align::Vortex) => {
            metres * 1250.0 / 381.0
        }
        // The metre, and a unit `CrsDef::build` refuses before any cell is planned.
        (Some(LinearUnit::Metre) | None, _) => metres,
    }
}

/// What a requested cell size becomes on a target: [`cell_size_in_target_units`], with the size
/// in metres that a record holding it has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSize {
    /// Asked for, in metres.
    pub requested_metres: f64,
    /// In the target's unit, as the lattice is built.
    pub in_target_units: f64,
    /// What a DSS record stores: the `f32` of that.
    pub stored: f32,
    /// The stored size in metres, by the CRS's own unit.
    pub stored_metres: f64,
}

impl CellSize {
    pub fn of(metres: f64, target: &CrsDef, align: Align) -> CellSize {
        use crate::crs::LinearUnit;
        let in_target_units = cell_size_in_target_units(metres, target, align);
        let stored = in_target_units as f32;
        let stored_metres = match LinearUnit::of_metres(target.metres_per_unit()) {
            Some(LinearUnit::UsSurveyFoot) => f64::from(stored) * 1200.0 / 3937.0,
            Some(LinearUnit::InternationalFoot) => f64::from(stored) * 381.0 / 1250.0,
            Some(LinearUnit::Metre) | None => f64::from(stored),
        };
        CellSize {
            requested_metres: metres,
            in_target_units,
            stored,
            stored_metres,
        }
    }

    /// Stored less requested, in metres. Zero wherever the record holds the size exactly.
    pub fn difference_metres(&self) -> f64 {
        self.stored_metres - self.requested_metres
    }
}

/// What `Resampler` is given.
#[derive(Debug, Clone, PartialEq)]
pub struct WarpSpec {
    /// `None` keeps the source's CRS.
    pub target: Option<CrsDef>,
    /// In the target's units where those are metres or degrees, which is every target but a
    /// Transverse Mercator in feet. There it is metres, converted to the CRS's own foot by default
    /// and by Vortex's rule under [`Align::Vortex`]: see [`cell_size_in_target_units`].
    /// `None` follows `Resampler.getTargetCellSize`.
    pub cell_size: Option<f64>,
    pub envelope: Option<WarpEnvelope>,
    pub method: Method,
    pub align: Align,
    pub datum_policy: DatumPolicy,
    /// Worker threads for each chunk, at most this many. The result does not depend on this; 0
    /// and 1 are one. Capped by [`MAX_WARP_THREADS`], the host's parallelism, the chunk's rows
    /// and [`WarpLimits::max_memory_bytes`].
    pub threads: usize,
    pub limits: WarpLimits,
}

impl WarpSpec {
    pub fn new(method: Method) -> Self {
        WarpSpec {
            target: None,
            cell_size: None,
            envelope: None,
            method,
            align: Align::default(),
            datum_policy: DatumPolicy::default(),
            threads: 1,
            limits: WarpLimits::default(),
        }
    }
}

/// The source's georeference, which a plan is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceGrid {
    pub nx: usize,
    pub ny: usize,
    pub transform: GeoTransform,
    pub crs: CrsDef,
}

fn refused(what: &str, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

const CELL_BYTES: u64 = std::mem::size_of::<f32>() as u64;

// A `std` lock can be a heap allocation: on macOS each `Mutex` boxes a 64-byte `pthread_mutex_t`.
const SYNC_BYTES: u64 = 256;

fn over_budget(bytes: u64, limit: u64) -> VortexError {
    refused(
        "warp",
        format!("it would hold {bytes} bytes, over the limit of {limit}"),
    )
}

// More workers than the host can run add cost and no speed.
fn useful_threads(requested: usize) -> usize {
    let host = std::thread::available_parallelism().map_or(1, usize::from);
    requested.clamp(1, host.clamp(1, MAX_WARP_THREADS))
}

/// A buffer, or a typed refusal if the allocation cannot be made.
pub(crate) fn alloc_filled<T: Clone>(len: usize, value: T) -> Result<Vec<T>, VortexError> {
    let mut v = Vec::new();
    v.try_reserve_exact(len)
        .map_err(|_| refused("warp", format!("cannot allocate {len} elements")))?;
    v.resize(len, value);
    Ok(v)
}

// InsertCenterLong (alg/gdaltransformer.cpp), which writes the centre with "%g" and reads it back,
// so only six significant digits reach the wrap.
fn center_long(crs: &CrsDef, gt: &[f64; 6], nx: f64, ny: f64) -> Option<f64> {
    if !crs.is_geographic() {
        return None;
    }
    let corners = [
        gt[0] + 0.0 * gt[1] + 0.0 * gt[2],
        gt[0] + nx * gt[1] + 0.0 * gt[2],
        gt[0] + 0.0 * gt[1] + ny * gt[2],
        gt[0] + nx * gt[1] + ny * gt[2],
    ];
    let min = transformer::cpp_min(
        transformer::cpp_min(corners[0], corners[1]),
        transformer::cpp_min(corners[2], corners[3]),
    );
    let max = transformer::cpp_max(
        transformer::cpp_max(corners[0], corners[1]),
        transformer::cpp_max(corners[2], corners[3]),
    );
    if max - min > 360.0 {
        return None;
    }
    format!("{:.5e}", (max + min) / 2.0).parse().ok()
}

fn whole_cells(value: f64, cell: f64) -> bool {
    (value / cell).round() * cell == value
}

/// A destination grid and its chunks, reusable for every frame on the same source grid.
#[derive(Debug, Clone)]
pub struct WarpPlan {
    source: SourceGrid,
    target: CrsDef,
    xf: GenImg,
    gt: [f64; 6],
    nx: usize,
    ny: usize,
    /// Chunk list and final check state, per working type.
    chunks: [(Vec<Chunk>, bool); 2],
    method: Method,
    threads: usize,
    max_memory_bytes: u64,
}

impl WarpPlan {
    /// Fails with a typed refusal; any input gives an error, not a panic.
    pub fn new(source: &SourceGrid, spec: &WarpSpec) -> Result<Self, VortexError> {
        source
            .nx
            .checked_mul(source.ny)
            .filter(|n| *n > 0 && i32::try_from(*n).is_ok())
            .ok_or_else(|| refused("source grid", "its size does not fit a warp"))?;
        let (src_nx, src_ny) = (source.nx as i32, source.ny as i32);
        let src_gt = gt_array(&source.transform);
        if src_gt.iter().any(|v| !v.is_finite()) {
            return Err(refused("source geotransform", "it is not finite"));
        }
        if src_gt[2] != 0.0 || src_gt[4] != 0.0 {
            return Err(refused(
                "source geotransform",
                "it is rotated, and this warp is not qualified on rotated sources: Vortex's grid \
                 model cannot express one, so no comparison with Vortex covers them",
            ));
        }
        let target = spec.target.clone().unwrap_or_else(|| source.crs.clone());

        // Resampler.getTargetCellSize.
        let cell = match spec.cell_size {
            Some(c) if !c.is_finite() || c <= 0.0 => {
                return Err(refused("cell size", "it must be finite and positive"))
            }
            Some(c) => Some(cell_size_in_target_units(c, &target, spec.align)),
            None if source.transform.dx == source.transform.dy
                && source.crs.is_geographic() == target.is_geographic() =>
            {
                Some(source.transform.dx).filter(|c| *c > 0.0)
            }
            None => None,
        };

        // Resampler.resample: reproject the envelope's corners, then snap to whole cells.
        let te = match &spec.envelope {
            None => None,
            Some(e) => {
                let from = e.crs.as_ref().unwrap_or(&source.crs);
                let r = reproject_envelope(e.envelope, from, &target, spec.datum_policy)?;
                let bounds = [r.min_x, r.min_y, r.max_x, r.max_y];
                if bounds.iter().any(|v| !v.is_finite()) {
                    return Err(refused("envelope", "it is not finite"));
                }
                Some(match cell {
                    Some(c) => [
                        (r.min_x / c).floor() * c,
                        (r.min_y / c).floor() * c,
                        (r.max_x / c).ceil() * c,
                        (r.max_y / c).ceil() * c,
                    ],
                    None => bounds,
                })
            }
        };

        let center = center_long(&source.crs, &src_gt, f64::from(src_nx), f64::from(src_ny));
        let xf = GenImg::new(&source.crs, &target, spec.datum_policy, src_gt, center)?;
        let output = extent::create_output(&xf, src_nx, src_ny, te, cell)?;
        if output.gt.iter().any(|v| !v.is_finite()) {
            return Err(refused("warp output extent", "it is not finite"));
        }
        let max_side = f64::from(i32::MAX);
        if !(output.nx >= 1.0 && output.ny >= 1.0 && output.nx <= max_side && output.ny <= max_side)
        {
            return Err(refused(
                "warp output extent",
                format!("the grid would be {} x {}", output.nx, output.ny),
            ));
        }
        let (nx, ny) = (output.nx as i32, output.ny as i32);
        let total = u64::from(nx as u32) * u64::from(ny as u32);
        let max_cells = spec.limits.max_cells.min(FORMAT_MAX_CELLS);
        if total > max_cells {
            return Err(refused(
                "warp output extent",
                format!(
                    "{nx} x {ny} is over the limit of {max_cells} cells. \
                     WarpLimits::format_maximum() allows larger grids"
                ),
            ));
        }
        let destination_bytes = total * CELL_BYTES;
        if destination_bytes > spec.limits.max_memory_bytes {
            return Err(over_budget(destination_bytes, spec.limits.max_memory_bytes));
        }
        if spec.align == Align::Refuse {
            let snapped = output.gt[1] == output.gt[5].abs()
                && whole_cells(output.gt[0], output.gt[1])
                && whole_cells(output.gt[3], output.gt[1]);
            if !snapped {
                return Err(refused(
                    "warp output extent",
                    "the origin is not on whole cells. Observed behaviour we did not implement: \
                     with no envelope or no cell size, Vortex 1.1.0 and 0.13.3 use GDAL's \
                     suggested extent, and the DSS write rounds its origin to whole cells. State \
                     an envelope and a cell size, or ask for align=\"vortex\"",
                ));
            }
        }
        let xf = xf.with_destination(output.gt)?;
        let chunks_for = |working| {
            ChunkBuilder::new(
                &xf,
                src_nx,
                src_ny,
                spec.method,
                spec.limits.max_chunks,
                output.check,
                working,
            )
            .collect(nx, ny)
            .ok_or_else(|| {
                refused(
                    "warp",
                    "the chunk list is over its limit or could not be allocated",
                )
            })
        };
        let chunks = [
            chunks_for(WorkingType::Float32)?,
            chunks_for(WorkingType::Float64)?,
        ];
        Ok(WarpPlan {
            source: source.clone(),
            target,
            xf,
            gt: output.gt,
            nx: nx as usize,
            ny: ny as usize,
            chunks,
            method: spec.method,
            threads: useful_threads(spec.threads),
            max_memory_bytes: spec.limits.max_memory_bytes,
        })
    }

    /// A plan for frames on this frame's grid. The frame's stated CRS must parse.
    pub fn for_frame(frame: &GridFrame, spec: &WarpSpec) -> Result<Self, VortexError> {
        let p = frame.parts();
        WarpPlan::new(
            &SourceGrid {
                nx: p.nx,
                ny: p.ny,
                transform: p.transform,
                crs: CrsDef::from_frame(&p.crs)?,
            },
            spec,
        )
    }

    /// Destination columns.
    pub fn nx(&self) -> usize {
        self.nx
    }

    /// Destination rows, north to south.
    pub fn ny(&self) -> usize {
        self.ny
    }

    /// The destination's geotransform, in GDAL's order.
    pub fn geotransform(&self) -> [f64; 6] {
        self.gt
    }

    /// How many chunks GDAL would split this warp into under this nodata marker, which sets the
    /// working type. Chunks change values, through the scale and the scanline approximation.
    pub fn chunk_count(&self, nodata: f32) -> usize {
        self.chunks_for(nodata).0.len()
    }

    fn chunks_for(&self, nodata: f32) -> &(Vec<Chunk>, bool) {
        match WorkingType::for_nodata(nodata) {
            WorkingType::Float32 => &self.chunks[0],
            WorkingType::Float64 => &self.chunks[1],
        }
    }

    /// The most a warp under this nodata marker holds at once, in bytes, counted as
    /// [`WarpLimits::max_memory_bytes`] counts it. A warp whose estimate is over the limit is
    /// refused before it allocates.
    pub fn memory_estimate(&self, nodata: f32) -> u64 {
        let (chunks, _) = self.chunks_for(nodata);
        let held = self.held_bytes();
        chunks
            .iter()
            .fold(held, |peak, c| peak.max(self.workers_for(c, held).1))
    }

    // The destination and both chunk lists, which stay for the whole warp.
    fn held_bytes(&self) -> u64 {
        // Capacity, not length: the lists keep what geometric growth reserved.
        let entries = (self.chunks[0].0.capacity() + self.chunks[1].0.capacity()) as u64;
        let cells = self.nx as u64 * self.ny as u64;
        (cells * CELL_BYTES)
            .saturating_add(entries * std::mem::size_of::<Chunk>() as u64)
            .saturating_add(SYNC_BYTES)
    }

    // How many workers this chunk gets, and the bytes held while they run. A single worker runs on
    // the caller's thread, so it has no stack to count. Workers do not change the result, so the
    // budget lowers their number before it refuses anything.
    fn workers_for(&self, c: &Chunk, held: u64) -> (usize, u64) {
        let cost = kernel::chunk_cost(c, self.method);
        let fixed = held.saturating_add(cost.fixed);
        let alone = fixed.saturating_add(cost.scratch);
        let wanted = self.threads.min(c.dsy.max(1) as usize);
        let each = cost.scratch.saturating_add(kernel::WORKER_OVERHEAD_BYTES);
        let room = self.max_memory_bytes.saturating_sub(fixed) / each;
        let workers = (wanted as u64).min(room).max(1) as usize;
        if workers == 1 {
            (1, alone)
        } else {
            (
                workers,
                fixed.saturating_add(each.saturating_mul(workers as u64)),
            )
        }
    }

    /// Warp cells in the source's row order. `nodata` is `-srcnodata` and the destination's
    /// nodata: an array holding NaN under another marker keeps NaN as data, and NaN spreads.
    pub fn warp_cells(&self, cells: &[f32], nodata: f32) -> Result<Vec<f32>, VortexError> {
        Ok(self.warp_cells_counted(cells, nodata)?.0)
    }

    // The cells, and how many were computed from valid data and equal the marker.
    fn warp_cells_counted(
        &self,
        cells: &[f32],
        nodata: f32,
    ) -> Result<(Vec<f32>, u64), VortexError> {
        if cells.len() != self.source.nx * self.source.ny {
            return Err(refused(
                "warp",
                format!(
                    "expected {} source cells, got {}",
                    self.source.nx * self.source.ny,
                    cells.len()
                ),
            ));
        }
        let estimate = self.memory_estimate(nodata);
        if estimate > self.max_memory_bytes {
            return Err(over_budget(estimate, self.max_memory_bytes));
        }
        // A MEM dataset starts as zeros, and INIT_DEST=NO_DATA applies per chunk (WarpRegion).
        // Where ComputeSourceWindow fails, GDAL warns and skips the window, so its cells stay 0.
        let mut out = alloc_filled(self.nx * self.ny, 0.0f32)?;
        let (chunks, check) = self.chunks_for(nodata);
        let held = self.held_bytes();
        let collisions = std::sync::atomic::AtomicU64::new(0);
        for c in chunks {
            let (dx, dsx) = (c.dx as usize, c.dsx as usize);
            let mut rows: Vec<&mut [f32]> = Vec::new();
            rows.try_reserve_exact(c.dsy as usize).map_err(|_| {
                refused("warp", format!("cannot allocate {} row references", c.dsy))
            })?;
            rows.extend(
                out.chunks_exact_mut(self.nx)
                    .skip(c.dy as usize)
                    .take(c.dsy as usize)
                    .map(|row| &mut row[dx..dx + dsx]),
            );
            rows.iter_mut().for_each(|row| row.fill(nodata));
            kernel::warp_chunk(
                &self.xf,
                cells,
                self.source.nx as i32,
                self.method,
                c,
                nodata,
                &mut rows,
                self.workers_for(c, held).0,
                *check,
                &collisions,
            )?;
        }
        Ok((out, collisions.into_inner()))
    }

    /// Warp one frame. Its grid must be the plan's, and it must state a nodata marker, because
    /// `Resampler` always passes one and GDAL's kernels differ without it.
    pub fn warp_frame(&self, frame: &GridFrame) -> Result<GridFrame, VortexError> {
        Ok(self.warp_frame_counted(frame)?.0)
    }

    /// [`Self::warp_frame`], and how many cells were computed from valid data and came out equal to
    /// the marker, as bits. GDAL 3.2.1 writes these with no nudge, so a reader takes them as missing;
    /// the count lets a caller refuse rather than lose them. A NaN marker counts none.
    pub fn warp_frame_counted(&self, frame: &GridFrame) -> Result<(GridFrame, u64), VortexError> {
        let p = frame.parts();
        if p.nx != self.source.nx
            || p.ny != self.source.ny
            || p.transform != self.source.transform
            || CrsDef::from_frame(&p.crs)? != self.source.crs
        {
            return Err(refused(
                "warp",
                "the frame is not on the plan's source grid",
            ));
        }
        let nodata = p
            .nodata
            .ok_or_else(|| refused("warp", "the frame states no nodata marker"))?;
        let (cells, collisions) = self.warp_cells_counted(&p.cells, nodata)?;
        let crs = if self.target == CrsDef::shg() {
            Crs::Named(NamedCrs::Shg)
        } else {
            Crs::Wkt(self.target.to_wkt())
        };
        let g = self.gt;
        let frame = GridFrame::new(GridFrameParts {
            cells,
            nx: self.nx,
            ny: self.ny,
            row_order: RowOrder::NorthToSouth,
            transform: GeoTransform {
                origin_x: g[0],
                dx: g[1],
                row_rotation: g[2],
                origin_y: g[3],
                column_rotation: g[4],
                dy: g[5],
            },
            crs,
            nodata: Some(nodata),
            time: p.time,
            data_type: p.data_type,
            units: p.units.clone(),
            variable: p.variable.clone(),
            description: p.description.clone(),
        })?;
        Ok((frame, collisions))
    }

    /// Warp frames lazily, holding one at a time.
    pub fn warp_frames<'a, I>(
        &'a self,
        frames: I,
    ) -> impl Iterator<Item = Result<GridFrame, VortexError>> + 'a
    where
        I: IntoIterator<Item = GridFrame> + 'a,
    {
        frames.into_iter().map(move |f| self.warp_frame(&f))
    }
}

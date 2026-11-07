//! The warp kernels of GDAL 3.2.1 `alg/gdalwarpkernel.cpp`, for a Float32 band with nodata set:
//! `GWKNearestThread`, `GWKRealCaseThread` with `GWKBilinearResample4Sample` or `GWKResample`, and
//! `GWKAverageOrModeThread`. The Float64 working type runs the same arithmetic: accumulators are
//! `f64` in both, and each output is cast to `f32` once. `Resampler` always passes `-srcnodata`, and `gdalwarp` copies it to
//! the destination, so `panDstValid` exists and GDAL's no-mask fast paths are never taken.
//!
//! Values accumulate in `f64` and are cast to `f32` once, in GDAL's order. Do not reorder.
//!
//! Each destination row depends only on its own coordinates and read-only source data, so rows
//! can run on any number of threads with the same result. The caller decides how many, from
//! [`chunk_cost`] and its memory budget; a thread the OS will not start is a typed refusal.

use super::transformer::{cpp_max, cpp_min, Direction, GenImg};
use super::window::Chunk;
use super::{alloc_filled, Method, WorkingType};
use crate::error::VortexError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// `SRC_DENSITY_THRESHOLD`, `BAND_DENSITY_THRESHOLD` and the `f32` literal of `GWKGetPixelRow`.
const BAND_DENSITY_THRESHOLD: f64 = 0.0000000001;
const ROW_DENSITY_THRESHOLD: f64 = 0.000000001f32 as f64;
/// Upper limit of the X scale ratio for `FROM_GRID_SAMPLING` in `PerformWarp`.
const SCALE_RATIO_LIMIT: f64 = 100.0;

/// Each worker's stack, set explicitly so that the budget counts what is really reserved. It is
/// std's default; the only recursion here is the transformer's halving, which is shallow.
const WORKER_STACK_BYTES: usize = 2 << 20;

/// What a started worker costs beyond its scratch: its stack, and std's bookkeeping for it.
pub(crate) const WORKER_OVERHEAD_BYTES: u64 = WORKER_STACK_BYTES as u64 + 4096;

/// What warping one chunk holds in bytes, besides the destination.
pub(crate) struct ChunkCost {
    /// The source window, its validity mask and the row references, once.
    pub fixed: u64,
    /// One worker's [`Scratch`], at most.
    pub scratch: u64,
}

pub(crate) fn chunk_cost(c: &Chunk, method: Method) -> ChunkCost {
    let rows = c.dsy.max(0) as u64 * std::mem::size_of::<&mut [f32]>() as u64;
    if c.ssx <= 0 || c.ssy <= 0 {
        return ChunkCost {
            fixed: rows,
            scratch: 0,
        };
    }
    let (f32_size, f64_size) = (std::mem::size_of::<f32>() as u64, 8u64);
    let source = c.ssx as u64 * c.ssy as u64;
    let n = c.dsx.max(0) as u64;
    let coordinates = 2 * f64_size + 1;
    let per_cell = if method == Method::Average {
        2 * coordinates
    } else {
        coordinates
    };
    // The bound in `Kernel::scratch`, without the chunk's scale: only bilinear's radius grows.
    let width = match method.filter_radius() {
        0 => 4,
        _ => c.ssx as u64 + 4,
    };
    ChunkCost {
        fixed: source * (f32_size + 1) + rows,
        scratch: n * per_cell + width * (3 * f64_size + 1),
    }
}

struct Kernel<'a> {
    src: Vec<f32>,
    valid: Option<Vec<bool>>,
    nsx: i32,
    nsy: i32,
    xoff: i32,
    yoff: i32,
    x_scale: f64,
    y_scale: f64,
    x_radius: i32,
    y_radius: i32,
    filt_x: i32,
    filt_y: i32,
    four_samples: bool,
    method: Method,
    xf: &'a GenImg,
    chunk: Chunk,
    /// `CHECK_WITH_INVERT_PROJ` as the transformer holds it during the warp.
    check: bool,
    /// The marker's bits, when it is a number a valid result could equal.
    marker: Option<u32>,
    /// Counts cells computed from valid data whose result equals the marker. GDAL 3.2.1 writes them,
    /// and they then read as missing.
    collisions: &'a AtomicU64,
}

#[derive(Default)]
struct Scratch {
    x: Vec<f64>,
    y: Vec<f64>,
    ok: Vec<bool>,
    x2: Vec<f64>,
    y2: Vec<f64>,
    ok2: Vec<bool>,
    weights: Vec<f64>,
    calculated: Vec<bool>,
    row_density: Vec<f64>,
    row_real: Vec<f64>,
}

// gdal_priv.h ARE_REAL_EQUAL, ulp = 2, on the working type: float epsilon in both instantiations.
#[inline]
fn are_real_equal(a: f32, b: f32, working: WorkingType) -> bool {
    match working {
        WorkingType::Float32 => a == b || (a - b).abs() < f32::EPSILON * (a + b).abs() * 2.0,
        WorkingType::Float64 => {
            let (a, b) = (f64::from(a), f64::from(b));
            a == b || (a - b).abs() < f64::from(f32::EPSILON) * (a + b).abs() * 2.0
        }
    }
}

/// Warp one chunk into `out_rows`, the destination rows it covers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn warp_chunk(
    xf: &GenImg,
    source: &[f32],
    src_nx: i32,
    method: Method,
    chunk: &Chunk,
    nodata: f32,
    out_rows: &mut [&mut [f32]],
    threads: usize,
    check: bool,
    collisions: &AtomicU64,
) -> Result<(), VortexError> {
    // PerformWarp: "See #2445 and #3079".
    if chunk.ssx <= 0 || chunk.ssy <= 0 {
        return Ok(());
    }
    let kernel = Kernel::new(xf, source, src_nx, method, chunk, nodata, check, collisions)?;
    let queue = Mutex::new(out_rows.iter_mut().enumerate());
    let workers = threads.clamp(1, out_rows_len(&queue).max(1));
    let stop = AtomicBool::new(false);
    let run = || -> Result<(), VortexError> {
        let mut scratch = kernel.scratch()?;
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let item = match queue.lock() {
                Ok(mut q) => q.next(),
                Err(_) => None,
            };
            let Some((row, cells)) = item else {
                return Ok(());
            };
            kernel.warp_row(row as i32, cells, &mut scratch);
        }
    };
    if workers == 1 {
        return run();
    }
    run_workers(workers, &stop, &run, |builder, scope, run| {
        builder.spawn_scoped(scope, run)
    })
}

fn worker_refusal(reason: String) -> VortexError {
    VortexError::Refused {
        what: "warp".into(),
        reason,
    }
}

/// Start `workers` copies of `run` and wait for them. If one cannot start, the ones already
/// running are told to stop and joined, and the result is a typed refusal. The rows are then
/// partly written, so the caller must not use them; `warp_cells` drops them with its buffer.
fn run_workers<'env, F, S>(
    workers: usize,
    stop: &AtomicBool,
    run: &'env F,
    spawn: S,
) -> Result<(), VortexError>
where
    F: Fn() -> Result<(), VortexError> + Sync,
    S: for<'scope> Fn(
        std::thread::Builder,
        &'scope std::thread::Scope<'scope, 'env>,
        &'env F,
    ) -> std::io::Result<
        std::thread::ScopedJoinHandle<'scope, Result<(), VortexError>>,
    >,
{
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(workers)
            .map_err(|_| worker_refusal(format!("cannot allocate {workers} thread handles")))?;
        let mut result = Ok(());
        for started in 0..workers {
            let builder = std::thread::Builder::new().stack_size(WORKER_STACK_BYTES);
            match spawn(builder, scope, run) {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    stop.store(true, Ordering::Relaxed);
                    result = Err(worker_refusal(format!(
                        "the OS refused worker {} of {workers}: {e}",
                        started + 1
                    )));
                    break;
                }
            }
        }
        for h in handles.drain(..) {
            let joined = match h.join() {
                Ok(r) => r,
                Err(_) => Err(worker_refusal("a worker thread stopped".into())),
            };
            if result.is_ok() {
                result = joined;
            }
        }
        result
    })
}

fn out_rows_len<I: ExactSizeIterator>(queue: &Mutex<I>) -> usize {
    queue.lock().map(|q| q.len()).unwrap_or(1)
}

impl<'a> Kernel<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        xf: &'a GenImg,
        source: &[f32],
        src_nx: i32,
        method: Method,
        c: &Chunk,
        nodata: f32,
        check: bool,
        collisions: &'a AtomicU64,
    ) -> Result<Self, VortexError> {
        // WarpRegionToBuffer: the source subwindow, then GDALWarpNoDataMasker.
        let (ssx, ssy) = (c.ssx as usize, c.ssy as usize);
        let mut src = alloc_filled(ssx * ssy, 0.0f32)?;
        for r in 0..ssy {
            let from = (c.sy as usize + r) * src_nx as usize + c.sx as usize;
            src[r * ssx..(r + 1) * ssx].copy_from_slice(&source[from..from + ssx]);
        }
        let nodata_nan = nodata.is_nan();
        let working = WorkingType::for_nodata(nodata);
        let any_invalid = src.iter().any(|v| {
            if nodata_nan {
                v.is_nan()
            } else {
                are_real_equal(*v, nodata, working)
            }
        });
        let valid = if any_invalid {
            let mut mask = alloc_filled(src.len(), true)?;
            for (m, v) in mask.iter_mut().zip(&src) {
                *m = if nodata_nan {
                    !v.is_nan()
                } else {
                    !are_real_equal(*v, nodata, working)
                };
            }
            Some(mask)
        } else {
            None
        };

        // PerformWarp, "Pre-calculate resampling scales and window sizes for filtering."
        let (nsx, nsy) = (f64::from(c.ssx), f64::from(c.ssy));
        let (ndx, ndy) = (f64::from(c.dsx), f64::from(c.dsy));
        let mut x_scale = ndx / (nsx - c.extra_x);
        let mut y_scale = ndy / (nsy - c.extra_y);
        if nsx >= ndx && nsx <= ndx + c.extra_x {
            x_scale = 1.0;
        }
        if nsy >= ndy && nsy <= ndy + c.extra_y {
            y_scale = 1.0;
        }
        x_scale = snap_reciprocal(x_scale);
        y_scale = snap_reciprocal(y_scale);
        if y_scale / x_scale > SCALE_RATIO_LIMIT {
            x_scale = sampled_x_scale(xf, c, check).unwrap_or(x_scale);
        }
        let four_samples = x_scale >= 0.95 && y_scale >= 0.95;
        let filter = method.filter_radius();
        let radius = |scale: f64| -> i32 {
            if scale < 1.0 {
                (f64::from(filter) / scale).ceil() as i32
            } else {
                filter
            }
        };
        let (x_radius, y_radius) = (radius(x_scale), radius(y_scale));
        Ok(Kernel {
            src,
            valid,
            nsx: c.ssx,
            nsy: c.ssy,
            xoff: c.sx,
            yoff: c.sy,
            x_scale,
            y_scale,
            x_radius,
            y_radius,
            filt_x: ((filter + 1) % 2).saturating_sub(x_radius),
            filt_y: ((filter + 1) % 2).saturating_sub(y_radius),
            four_samples,
            method,
            xf,
            chunk: *c,
            check,
            marker: (!nodata.is_nan()).then(|| nodata.to_bits()),
            collisions,
        })
    }

    #[inline]
    fn note_written(&self, written: bool, cell: f32) {
        if written && self.marker == Some(cell.to_bits()) {
            self.collisions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn scratch(&self) -> Result<Scratch, VortexError> {
        let n = self.chunk.dsx as usize;
        // GWKResampleCreateWrkStruct sizes these from the radius. The loops clamp to the source
        // width, so this bound holds the same values without trusting a huge radius.
        let width = (i64::from(self.x_radius) + 1)
            .saturating_mul(2)
            .saturating_add(2)
            .min(i64::from(self.nsx) + 4) as usize;
        let average = self.method == Method::Average;
        let extra = if average { n } else { 0 };
        Ok(Scratch {
            x: alloc_filled(n, 0.0)?,
            y: alloc_filled(n, 0.0)?,
            ok: alloc_filled(n, false)?,
            x2: alloc_filled(extra, 0.0)?,
            y2: alloc_filled(extra, 0.0)?,
            ok2: alloc_filled(extra, false)?,
            weights: alloc_filled(width, 0.0)?,
            calculated: alloc_filled(width, false)?,
            row_density: alloc_filled(width, 0.0)?,
            row_real: alloc_filled(width, 0.0)?,
        })
    }

    #[inline]
    fn is_valid(&self, off: i64) -> bool {
        match &self.valid {
            None => true,
            // Masks carry WARP_EXTRA_ELTS trailing bits, created set (CreateKernelMask).
            Some(v) => off < 0 || off as usize >= v.len() || v[off as usize],
        }
    }

    #[inline]
    fn val(&self, off: i64) -> f64 {
        if off < 0 || off as usize >= self.src.len() {
            0.0
        } else {
            f64::from(self.src[off as usize])
        }
    }

    fn warp_row(&self, row: i32, cells: &mut [f32], s: &mut Scratch) {
        match self.method {
            Method::Average => self.average_row(row, cells, s),
            _ => self.point_row(row, cells, s),
        }
    }

    // GWKNearestThread and GWKRealCaseThread share this shape.
    fn point_row(&self, row: i32, cells: &mut [f32], s: &mut Scratch) {
        let c = &self.chunk;
        let dy = f64::from(row) + 0.5 + f64::from(c.dy);
        for i in 0..cells.len() {
            s.x[i] = i as f64 + 0.5 + f64::from(c.dx);
            s.y[i] = dy;
        }
        self.xf.approx(
            Direction::DstToSrc,
            self.check,
            &mut s.x,
            &mut s.y,
            &mut s.ok,
        );
        let (fxoff, fyoff) = (f64::from(self.xoff), f64::from(self.yoff));
        for (i, cell) in cells.iter_mut().enumerate() {
            let (px, py) = (s.x[i], s.y[i]);
            if !s.ok[i] {
                continue;
            }
            let Some(off) = self.offset(px, py) else {
                continue;
            };
            // "This currently ignores the multi-pixel input of bilinear and cubic resamples."
            if !self.is_valid(off) {
                continue;
            }
            if self.method == Method::Near {
                *cell = self.src[off as usize];
                continue;
            }
            let (value, density) = if self.nsx == 1 || self.nsy == 1 {
                (self.val(off), 1.0)
            } else if self.four_samples {
                self.bilinear4(px - fxoff, py - fyoff)
            } else {
                self.resample(px - fxoff, py - fyoff, s)
            };
            if density < BAND_DENSITY_THRESHOLD {
                continue;
            }
            let written = set_pixel(cell, density, value);
            self.note_written(written, *cell);
        }
    }

    // GWKCheckAndComputeSrcOffsets, after its success test.
    #[inline]
    fn offset(&self, x: f64, y: f64) -> Option<i64> {
        if x.is_nan() || y.is_nan() {
            return None;
        }
        if x < f64::from(self.xoff) || y < f64::from(self.yoff) {
            return None;
        }
        if x + 1e-10 > f64::from(self.nsx) + f64::from(self.xoff)
            || y + 1e-10 > f64::from(self.nsy) + f64::from(self.yoff)
        {
            return None;
        }
        let mut ix = (x + 1.0e-10) as i32 - self.xoff;
        let mut iy = (y + 1.0e-10) as i32 - self.yoff;
        if ix == self.nsx {
            ix -= 1;
        }
        if iy == self.nsy {
            iy -= 1;
        }
        Some(i64::from(ix) + i64::from(iy) * i64::from(self.nsx))
    }

    // GWKGetPixelRow with a density array of two, which the 4-sample path always passes.
    #[inline]
    fn row2(&self, off: i64, d: &mut [f64; 2], r: &mut [f64; 2]) -> bool {
        *d = [1.0, 1.0];
        let mut has = false;
        if self.valid.is_some() {
            if self.is_valid(off) {
                has = true;
            } else {
                d[0] = 0.0;
            }
            if self.is_valid(off + 1) {
                has = true;
            } else {
                d[1] = 0.0;
            }
            if !has {
                return false;
            }
        }
        *r = [self.val(off), self.val(off + 1)];
        has = false;
        if d[0] > ROW_DENSITY_THRESHOLD {
            d[0] = 1.0;
            has = true;
        }
        if d[1] > ROW_DENSITY_THRESHOLD {
            d[1] = 1.0;
            has = true;
        }
        has
    }

    // GWKBilinearResample4Sample. A NaN among the four samples spreads through the sum: this is
    // how NaN source cells behave when nodata is another value, as in MRMS.
    fn bilinear4(&self, src_x: f64, src_y: f64) -> (f64, f64) {
        let (nsx, nsy) = (self.nsx, self.nsy);
        let mut isx = (src_x - 0.5).floor() as i32;
        let mut isy = (src_y - 0.5).floor() as i32;
        let mut rx = 1.5 - (src_x - f64::from(isx));
        let mut ry = 1.5 - (src_y - f64::from(isy));
        if isx == -1 {
            isx = 0;
            rx = 1.0;
        }
        if isy == -1 {
            isy = 0;
            ry = 1.0;
        }
        let stride = i64::from(nsx);
        let mut off = i64::from(isx) + i64::from(isy) * stride;
        let pixels = stride * i64::from(nsy);
        let mut shifted = false;
        if pixels == off + 1 || pixels == off + stride + 1 {
            shifted = true;
            off -= 1;
        }
        let (mut d, mut r) = ([0.0f64; 2], [0.0f64; 2]);
        let (mut acc_r, mut acc_d, mut acc_w) = (0.0f64, 0.0f64, 0.0f64);
        let mut add = |d: &[f64; 2], r: &[f64; 2], m1: f64, m2: f64| {
            if isx >= 0 && isx < nsx && d[0] > ROW_DENSITY_THRESHOLD {
                acc_w += m1;
                acc_r += r[0] * m1;
                acc_d += d[0] * m1;
            }
            if isx + 1 >= 0 && isx + 1 < nsx && d[1] > ROW_DENSITY_THRESHOLD {
                acc_w += m2;
                acc_r += r[1] * m2;
                acc_d += d[1] * m2;
            }
        };
        if isy >= 0 && isy < nsy && off >= 0 && off < pixels && self.row2(off, &mut d, &mut r) {
            if shifted {
                r[0] = r[1];
                d[0] = d[1];
            }
            add(&d, &r, rx * ry, (1.0 - rx) * ry);
        }
        if isy + 1 >= 0
            && isy + 1 < nsy
            && off + stride >= 0
            && off + stride < pixels
            && self.row2(off + stride, &mut d, &mut r)
        {
            if shifted {
                r[0] = r[1];
                d[0] = d[1];
            }
            add(&d, &r, rx * (1.0 - ry), (1.0 - rx) * (1.0 - ry));
        }
        if acc_w == 1.0 {
            (acc_r, acc_d)
        } else if acc_w < 0.00001 {
            (0.0, 0.0)
        } else {
            (acc_r / acc_w, acc_d / acc_w)
        }
    }

    // GWKGetPixelRow.
    fn row(&self, off: i64, half: i64, dens: &mut [f64], real: &mut [f64]) -> bool {
        let len = (half * 2) as usize;
        let mut has = false;
        if self.valid.is_some() {
            for (i, d) in dens.iter_mut().enumerate().take(len) {
                if self.is_valid(off + i as i64) {
                    *d = 1.0;
                    has = true;
                } else {
                    *d = 0.0;
                }
            }
            if !has {
                return false;
            }
        }
        for (i, r) in real.iter_mut().enumerate().take(len) {
            *r = self.val(off + i as i64);
        }
        if self.valid.is_none() {
            return true;
        }
        has = false;
        for d in dens.iter_mut().take(len) {
            if *d > ROW_DENSITY_THRESHOLD {
                *d = 1.0;
                has = true;
            }
        }
        has
    }

    // GWKResample with GWKBilinear weights; used when either scale is below 0.95.
    fn resample(&self, src_x: f64, src_y: f64, s: &mut Scratch) -> (f64, f64) {
        let (nsx, nsy) = (i64::from(self.nsx), i64::from(self.nsy));
        let (mut acc_r, mut acc_d, mut acc_w) = (0.0f64, 0.0f64, 0.0f64);
        let isx = i64::from((src_x - 0.5).floor() as i32);
        let isy = i64::from((src_y - 0.5).floor() as i32);
        let off = isx + isy * nsx;
        let delta_x = src_x - 0.5 - isx as f64;
        let delta_y = src_y - 0.5 - isy as f64;
        s.calculated.iter_mut().for_each(|c| *c = false);
        let tent = |v: f64| -> f64 {
            let a = v.abs();
            if a <= 1.0 {
                1.0 - a
            } else {
                0.0
            }
        };
        let mut j = i64::from(self.filt_y);
        let mut j_max = i64::from(self.y_radius);
        if isy + j < 0 {
            j = -isy;
        }
        if isy + j_max >= nsy {
            j_max = nsy - isy - 1;
        }
        let mut i_min = i64::from(self.filt_x);
        let mut i_max = i64::from(self.x_radius);
        if isx + i_min < 0 {
            i_min = -isx;
        }
        if isx + i_max >= nsx {
            i_max = nsx - isx - 1;
        }
        let x_scaled = self.x_scale < 1.0;
        let y_scaled = self.y_scale < 1.0;
        let has_density = self.valid.is_some();
        let mut row_off = off + (j - 1) * nsx + i_min;
        while j <= j_max {
            row_off += nsx;
            if !self.row(
                row_off,
                (i_max - i_min + 2) / 2,
                &mut s.row_density,
                &mut s.row_real,
            ) {
                j += 1;
                continue;
            }
            let w1 = if y_scaled {
                tent((j as f64 - delta_y) * self.y_scale)
            } else {
                tent(j as f64 - delta_y)
            };
            let (mut line_r, mut line_d, mut line_w) = (0.0f64, 0.0f64, 0.0f64);
            let mut i = i_min;
            while i <= i_max {
                let k = (i - i_min) as usize;
                if has_density && s.row_density[k] < ROW_DENSITY_THRESHOLD {
                    i += 1;
                    continue;
                }
                let w2 = if s.calculated[k] {
                    s.weights[k]
                } else {
                    let w = if x_scaled {
                        tent((i as f64 - delta_x) * self.x_scale)
                    } else {
                        tent(i as f64 - delta_x)
                    };
                    s.weights[k] = w;
                    s.calculated[k] = true;
                    w
                };
                line_r += s.row_real[k] * w2;
                if has_density {
                    line_d += s.row_density[k] * w2;
                }
                line_w += w2;
                i += 1;
            }
            acc_r += line_r * w1;
            acc_d += line_d * w1;
            acc_w += line_w * w1;
            j += 1;
        }
        if acc_w < 0.000001 || (has_density && acc_d < 0.000001) {
            return (0.0, 0.0);
        }
        if !(0.99999..=1.00001).contains(&acc_w) {
            (acc_r / acc_w, if has_density { acc_d / acc_w } else { 1.0 })
        } else {
            (acc_r, if has_density { acc_d } else { 1.0 })
        }
    }

    // GWKAverageOrModeThread, GRA_Average. No centre test: any positive valid weight counts.
    fn average_row(&self, row: i32, cells: &mut [f32], s: &mut Scratch) {
        let c = &self.chunk;
        for i in 0..cells.len() {
            s.x[i] = f64::from(i as i32 + c.dx);
            s.y[i] = f64::from(row + c.dy);
            s.x2[i] = i as f64 + 1.0 + f64::from(c.dx);
            s.y2[i] = f64::from(row) + 1.0 + f64::from(c.dy);
        }
        self.xf.approx(
            Direction::DstToSrc,
            self.check,
            &mut s.x,
            &mut s.y,
            &mut s.ok,
        );
        self.xf.approx(
            Direction::DstToSrc,
            self.check,
            &mut s.x2,
            &mut s.y2,
            &mut s.ok2,
        );
        let (fxoff, fyoff) = (f64::from(self.xoff), f64::from(self.yoff));
        let (low_x, low_y) = (fxoff - 1.0, fyoff - 1.0);
        let high_x = f64::from(self.nsx) + fxoff + 1.0;
        let high_y = f64::from(self.nsy) + fyoff + 1.0;
        let (nsx, nsy) = (self.nsx, self.nsy);
        for (i, cell) in cells.iter_mut().enumerate() {
            if !s.ok[i] || !s.ok2[i] {
                continue;
            }
            let (x1, y1, x2, y2) = (s.x[i], s.y[i], s.x2[i], s.y2[i]);
            if x1 < low_x
                || x2 < low_x
                || y1 < low_y
                || y2 < low_y
                || x1 > high_x
                || x2 > high_x
                || y1 > high_y
                || y2 > high_y
            {
                continue;
            }
            let x_min = cpp_min(x1, x2) - fxoff;
            let is_x_min = ((x_min + 1e-10).floor() as i32).max(0);
            let x_max = cpp_max(x1, x2) - fxoff;
            let mut is_x_max = ((x_max - 1e-10).ceil() as i32).min(nsx);
            let y_min = cpp_min(y1, y2) - fyoff;
            let is_y_min = ((y_min + 1e-10).floor() as i32).max(0);
            let y_max = cpp_max(y1, y2) - fyoff;
            let mut is_y_max = ((y_max - 1e-10).ceil() as i32).min(nsy);
            if is_x_min == is_x_max && is_x_max < nsx {
                is_x_max += 1;
            }
            if is_y_min == is_y_max && is_y_max < nsy {
                is_y_max += 1;
            }
            let (mut total, mut weight) = (0.0f64, 0.0f64);
            let mut sy = is_y_min;
            while sy < is_y_max {
                let wy = if sy == is_y_min {
                    if is_y_min + 1 == is_y_max {
                        1.0
                    } else {
                        1.0 - (y_min - f64::from(is_y_min))
                    }
                } else if sy + 1 == is_y_max {
                    1.0 - (f64::from(is_y_max) - y_max)
                } else {
                    1.0
                };
                let mut off = i64::from(is_x_min) + i64::from(sy) * i64::from(nsx);
                let mut sx = is_x_min;
                while sx < is_x_max {
                    if self.is_valid(off) {
                        let w = if sx == is_x_min {
                            if is_x_min + 1 == is_x_max {
                                wy
                            } else {
                                wy * (1.0 - (x_min - f64::from(is_x_min)))
                            }
                        } else if sx + 1 == is_x_max {
                            wy * (1.0 - (f64::from(is_x_max) - x_max))
                        } else {
                            wy
                        };
                        weight += w;
                        total += self.val(off) * w;
                    }
                    sx += 1;
                    off += 1;
                }
                sy += 1;
            }
            if weight > 0.0 {
                let written = set_pixel(cell, 1.0, total / weight);
                self.note_written(written, *cell);
            }
        }
    }
}

fn snap_reciprocal(scale: f64) -> f64 {
    if scale < 1.0 {
        let reciprocal = 1.0 / scale;
        let nearest = (reciprocal + 0.5) as i32;
        if (reciprocal - f64::from(nearest)).abs() < 0.05 {
            return 1.0 / f64::from(nearest);
        }
    }
    scale
}

// PerformWarp: "Prefer doing an average of a few sample points instead". The sample positions
// have no destination offset in GDAL, so they have none here.
fn sampled_x_scale(xf: &GenImg, c: &Chunk, check: bool) -> Option<f64> {
    let nx = c.dsx.min(10);
    let ny = c.dsy.min(10);
    let (mut px, mut py) = (Vec::new(), Vec::new());
    for iy in 0..ny {
        for ix in 0..nx {
            let x = if nx == 1 {
                0.0
            } else {
                f64::from(ix) * f64::from(c.dsx) / f64::from(nx - 1)
            };
            let y = if ny == 1 {
                0.0
            } else {
                f64::from(iy) * f64::from(c.dsy) / f64::from(ny - 1)
            };
            px.push(x);
            py.push(y);
            px.push(if ix == nx - 1 { x - 1.0 } else { x + 1.0 });
            py.push(y);
            px.push(x);
            py.push(if iy == ny - 1 { y - 1.0 } else { y + 1.0 });
        }
    }
    let mut ok = vec![false; px.len()];
    xf.approx(Direction::DstToSrc, check, &mut px, &mut py, &mut ok);
    let mut scales = Vec::new();
    for i in (0..px.len()).step_by(3) {
        if ok[i] && ok[i + 1] && ok[i + 2] {
            scales.push(1.0 / cpp_max((px[i + 1] - px[i]).abs(), (px[i + 2] - px[i]).abs()));
        }
    }
    scales.sort_by(f64::total_cmp);
    let max_scale = *scales.last()?;
    let (mut sum, mut count) = (0.0f64, 0u32);
    for s in &scales {
        if *s > max_scale / 10.0 {
            sum += *s;
            count += 1;
        }
    }
    (count > 0).then(|| sum / f64::from(count))
}

// GWKSetPixelValueReal. The destination is never valid before its one write, so its influence
// is zero, but the arithmetic stays: `value * density / density` can change the last bit.
#[inline]
fn set_pixel(cell: &mut f32, density: f64, value: f64) -> bool {
    let mut v = value;
    if density < 0.9999 {
        if density < 0.0001 {
            return false;
        }
        let dst_density = 0.0;
        let influence = (1.0 - density) * dst_density;
        v = (v * density + f64::from(*cell) * influence) / (density + influence);
    }
    *cell = v as f32;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // The OS refusing a thread cannot be arranged on demand, so this stands in for it. The whole
    // route is held by the Job Object script in `evidence/P44/scripts/thread_limit.py`.
    #[test]
    fn a_refused_worker_stops_the_rest_and_is_a_typed_error() {
        let started = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let run = || -> Result<(), VortexError> {
            started.fetch_add(1, Ordering::SeqCst);
            // Spins for ever unless the failure below tells it to stop.
            while !stop.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            Ok(())
        };
        let attempts = AtomicUsize::new(0);
        let result = run_workers(4, &stop, &run, |builder, scope, run| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 2 {
                return Err(std::io::Error::other("refused here"));
            }
            builder.spawn_scoped(scope, run)
        });
        match result {
            Err(VortexError::Refused { reason, .. }) => {
                assert!(reason.contains("worker 3 of 4"), "{reason}");
                assert!(reason.contains("refused here"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(started.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn a_workers_failure_comes_back_as_its_own_error() {
        let stop = AtomicBool::new(false);
        let run = || -> Result<(), VortexError> {
            Err(VortexError::Refused {
                what: "warp".into(),
                reason: "no scratch".into(),
            })
        };
        let result = run_workers(3, &stop, &run, |builder, scope, run| {
            builder.spawn_scoped(scope, run)
        });
        assert!(
            matches!(result, Err(VortexError::Refused { reason, .. }) if reason == "no scratch")
        );
    }
}

//! Source windows and chunks: `GDALWarpOperation::ComputeSourceWindow`,
//! `ComputeSourceWindowStartingFromSource` and `CollectChunkListInternal`
//! (`alg/gdalwarpoperation.cpp`, GDAL 3.2.1).

use super::transformer::{cpp_max, cpp_min, Direction, GenImg};
use super::{Method, WorkingType};

/// `GDALWarpOptions::dfWarpMemoryLimit` as `gdalwarp_lib.cpp` leaves it.
const WARP_MEMORY_LIMIT: f64 = 64.0 * 1024.0 * 1024.0;
const STEP_COUNT: usize = 21;

/// One `GDALWarpChunk`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Chunk {
    pub dx: i32,
    pub dy: i32,
    pub dsx: i32,
    pub dsy: i32,
    pub sx: i32,
    pub sy: i32,
    pub ssx: i32,
    pub ssy: i32,
    pub extra_x: f64,
    pub extra_y: f64,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    sx: i32,
    sy: i32,
    ssx: i32,
    ssy: i32,
    extra_x: f64,
    extra_y: f64,
    fill: f64,
}

type Bounds = (f64, f64, f64, f64);

pub(crate) struct ChunkBuilder<'a> {
    xf: &'a GenImg,
    src_nx: i32,
    src_ny: i32,
    radius: i32,
    /// `GDALWarpPrivateData`: the source grid's image, computed once.
    forward_grid: Option<(Vec<f64>, Vec<f64>, Vec<bool>)>,
    chunks: Vec<Chunk>,
    /// The working type's size and one validity bit per pixel, on both sides: `Resampler` always
    /// sets nodata. Chunking therefore depends on the working type.
    pixel_cost_bits: f64,
    /// Bound on the chunk list, so that hostile geometry cannot grow it without limit.
    max_chunks: usize,
    overflowed: bool,
    /// The transformer's `CHECK_WITH_INVERT_PROJ` state. The retry in `source_window` ends with a
    /// refresh after the option is cleared, which turns it off from then on.
    check: bool,
}

impl<'a> ChunkBuilder<'a> {
    pub(crate) fn new(
        xf: &'a GenImg,
        src_nx: i32,
        src_ny: i32,
        method: Method,
        max_chunks: usize,
        check: bool,
        working: WorkingType,
    ) -> Self {
        ChunkBuilder {
            xf,
            src_nx,
            src_ny,
            radius: method.filter_radius(),
            forward_grid: None,
            pixel_cost_bits: working.pixel_cost_bits(),
            chunks: Vec::new(),
            max_chunks,
            overflowed: false,
            check,
        }
    }

    /// `GDALWarpOperation::CollectChunkList`, with its sort by row and then column. `None`
    /// means the chunk list passed its bound or could not be allocated. Also returns the
    /// transformer's check state.
    pub(crate) fn collect(mut self, nx: i32, ny: i32) -> Option<(Vec<Chunk>, bool)> {
        self.collect_internal(0, 0, nx, ny);
        if self.overflowed {
            return None;
        }
        let mut chunks = std::mem::take(&mut self.chunks);
        // Each chunk has its own position, so an unstable sort gives the same order, and it does
        // not allocate.
        chunks.sort_unstable_by_key(|c| (c.dy, c.dx));
        Some((chunks, self.check))
    }

    fn collect_internal(&mut self, dx: i32, dy: i32, dsx: i32, dsy: i32) {
        if self.overflowed {
            return;
        }
        // A failure here is CE_Failure from ComputeSourceWindow: GDAL warns, skips the output
        // window, and its cells keep the initial nodata.
        let Some(w) = self.source_window(dx, dy, dsx, dsy) else {
            return;
        };
        let total = (self.pixel_cost_bits * f64::from(w.ssx) * f64::from(w.ssy)
            + self.pixel_cost_bits * f64::from(dsx) * f64::from(dsy))
            / 8.0;
        if (total > WARP_MEMORY_LIMIT && (dsx > 2 || dsy > 2))
            || (w.fill > 0.0 && w.fill < 0.5 && (dsx > 100 || dsy > 100))
        {
            if dsx > dsy {
                let first = dsx / 2;
                self.collect_internal(dx, dy, first, dsy);
                self.collect_internal(dx + first, dy, dsx - first, dsy);
            } else {
                let first = dsy / 2;
                self.collect_internal(dx, dy, dsx, first);
                self.collect_internal(dx, dy + first, dsx, dsy - first);
            }
            return;
        }
        if self.chunks.len() >= self.max_chunks || self.chunks.try_reserve(1).is_err() {
            self.overflowed = true;
            return;
        }
        self.chunks.push(Chunk {
            dx,
            dy,
            dsx,
            dsy,
            sx: w.sx,
            sy: w.sy,
            ssx: w.ssx,
            ssy: w.ssy,
            extra_x: w.extra_x,
            extra_y: w.extra_y,
        });
    }

    fn source_window(&mut self, dx: i32, dy: i32, dsx: i32, dsy: i32) -> Option<Window> {
        let (fdx, fdy, fdsx, fdsy) = (f64::from(dx), f64::from(dy), f64::from(dsx), f64::from(dsy));
        let (rx, ry) = (f64::from(self.src_nx), f64::from(self.src_ny));
        let mut use_grid = false;
        let mut retrying = false;
        let (mut px, mut py, mut ok) = (Vec::new(), Vec::new(), Vec::new());
        let (bounds, failed) = loop {
            sample_points(use_grid, fdx, fdy, fdsx, fdsy, &mut px, &mut py);
            ok.clear();
            ok.resize(px.len(), false);
            let check = retrying || self.check;
            self.xf
                .approx(Direction::DstToSrc, check, &mut px, &mut py, &mut ok);
            if retrying {
                self.check = false;
            }
            let (mut a, mut b, mut c, mut d) = (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            );
            let mut failed = 0usize;
            for i in 0..px.len() {
                if !ok[i] || px[i].is_nan() || py[i].is_nan() {
                    failed += 1;
                    continue;
                }
                a = cpp_min(a, px[i]);
                b = cpp_min(b, py[i]);
                c = cpp_max(c, px[i]);
                d = cpp_max(d, py[i]);
            }
            // "Try to detect crazy values coming from reprojection".
            if failed == 0 && !retrying && (a < -1e6 || b < -1e6 || c > rx + 1e6 || d > ry + 1e6) {
                retrying = true;
                continue;
            }
            if !use_grid && failed > 0 {
                use_grid = true;
                continue;
            }
            break ((a, b, c, d), failed);
        };
        if failed + 5 > px.len() {
            return None;
        }
        let mut bounds = bounds;
        if use_grid {
            self.extend_from_source(dx, dy, dsx, dsy, &mut bounds);
        }
        let (min_x, min_y, max_x, max_y) = bounds;
        if min_x > rx || max_x < 0.0 || min_y > ry || max_y < 0.0 {
            return Some(Window {
                sx: 0,
                sy: 0,
                ssx: 0,
                ssy: 0,
                extra_x: 0.0,
                extra_y: 0.0,
                fill: 0.0,
            });
        }
        let res = f64::from(self.radius);
        let x_scale = cpp_max(1e-3, fdsx / (max_x - min_x));
        let y_scale = cpp_max(1e-3, fdsy / (max_y - min_y));
        let mut x_radius = if x_scale < 0.95 {
            (res / x_scale).ceil() as i32
        } else {
            self.radius
        };
        let mut y_radius = if y_scale < 0.95 {
            (res / y_scale).ceil() as i32
        } else {
            self.radius
        };
        if failed > 0 {
            x_radius += 10;
            y_radius += 10;
        }
        let min_xc = cpp_max(0.0, min_x) as i32;
        let min_yc = cpp_max(0.0, min_y) as i32;
        let max_xc = cpp_min(max_x.ceil(), rx) as i32;
        let max_yc = cpp_min(max_y.ceil(), ry) as i32;
        let raw_x = cpp_max(0.0, cpp_min(f64::from(self.src_nx - min_xc), max_x - min_x));
        let raw_y = cpp_max(0.0, cpp_min(f64::from(self.src_ny - min_yc), max_y - min_y));
        let (sx, ssx) = axis_window(min_xc, max_xc, x_radius, self.src_nx);
        let (sy, ssy) = axis_window(min_yc, max_yc, y_radius, self.src_ny);
        Some(Window {
            sx,
            sy,
            ssx,
            ssy,
            extra_x: f64::from(ssx) - raw_x,
            extra_y: f64::from(ssy) - raw_y,
            fill: f64::from(ssx) * f64::from(ssy)
                / cpp_max(
                    1.0,
                    (max_x - min_x + 2.0 * f64::from(x_radius))
                        * (max_y - min_y + 2.0 * f64::from(y_radius)),
                ),
        })
    }

    fn extend_from_source(&mut self, dx: i32, dy: i32, dsx: i32, dsy: i32, bounds: &mut Bounds) {
        let step = 1.0 / (STEP_COUNT as f64 - 1.0);
        let (fnx, fny) = (f64::from(self.src_nx), f64::from(self.src_ny));
        let ratio = |i: usize, size: f64| -> f64 {
            if i == 0 {
                0.5 / size
            } else if i <= STEP_COUNT {
                (i as f64 - 1.0) * step
            } else {
                1.0 - 0.5 / size
            }
        };
        if self.forward_grid.is_none() {
            let mut gx = Vec::with_capacity((STEP_COUNT + 2) * (STEP_COUNT + 2));
            let mut gy = Vec::with_capacity((STEP_COUNT + 2) * (STEP_COUNT + 2));
            for iy in 0..STEP_COUNT + 2 {
                for ix in 0..STEP_COUNT + 2 {
                    gx.push(ratio(ix, fnx) * fnx);
                    gy.push(ratio(iy, fny) * fny);
                }
            }
            let mut ok = vec![false; gx.len()];
            self.xf
                .approx(Direction::SrcToDst, self.check, &mut gx, &mut gy, &mut ok);
            self.forward_grid = Some((gx, gy, ok));
        }
        let Some((gx, gy, ok)) = &self.forward_grid else {
            return;
        };
        let (l, t) = (f64::from(dx), f64::from(dy));
        let (r, b) = (l + f64::from(dsx), t + f64::from(dsy));
        let mut i = 0;
        for iy in 0..STEP_COUNT + 2 {
            for ix in 0..STEP_COUNT + 2 {
                if ok[i] && gx[i] >= l && gx[i] <= r && gy[i] >= t && gy[i] <= b {
                    let sx = ratio(ix, fnx) * fnx;
                    let sy = ratio(iy, fny) * fny;
                    bounds.0 = cpp_min(bounds.0, sx);
                    bounds.1 = cpp_min(bounds.1, sy);
                    bounds.2 = cpp_max(bounds.2, sx);
                    bounds.3 = cpp_max(bounds.3, sy);
                }
                i += 1;
            }
        }
    }
}

fn sample_points(
    use_grid: bool,
    fdx: f64,
    fdy: f64,
    fdsx: f64,
    fdsy: f64,
    px: &mut Vec<f64>,
    py: &mut Vec<f64>,
) {
    let step = 1.0 / (STEP_COUNT as f64 - 1.0);
    px.clear();
    py.clear();
    if use_grid {
        // GDAL divides by nDstXSize on both axes here.
        let ratio = |i: usize| -> f64 {
            if i == 0 {
                0.5 / fdsx
            } else if i <= STEP_COUNT {
                (i as f64 - 1.0) * step
            } else {
                1.0 - 0.5 / fdsx
            }
        };
        for iy in 0..STEP_COUNT + 2 {
            for ix in 0..STEP_COUNT + 2 {
                px.push(ratio(ix) * fdsx + fdx);
                py.push(ratio(iy) * fdsy + fdy);
            }
        }
    } else {
        // The ratio accumulates, as in GDAL.
        let mut ratio = 0.0f64;
        while ratio <= 1.0 + step * 0.5 {
            px.push(ratio * fdsx + fdx);
            py.push(fdy);
            px.push(ratio * fdsx + fdx);
            py.push(fdy + fdsy);
            px.push(fdx);
            py.push(ratio * fdsy + fdy);
            px.push(fdsx + fdx);
            py.push(ratio * fdsy + fdy);
            ratio += step;
        }
    }
}

// "If we cover more than 90% of the width, then use it fully".
fn axis_window(min_c: i32, max_c: i32, radius: i32, size: i32) -> (i32, i32) {
    if f64::from(max_c - min_c) > 0.9 * f64::from(size) {
        return (0, size);
    }
    let off = (i64::from(min_c) - i64::from(radius))
        .min(i64::from(size))
        .max(0);
    let len = (i64::from(size) - off)
        .min(i64::from(max_c) - off + i64::from(radius))
        .max(0);
    (off as i32, len as i32)
}

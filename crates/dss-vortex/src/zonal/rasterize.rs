//! Burning zone geometry into a mask, as GDAL 3.2.1 does it for Vortex.
//!
//! Sources: `alg/gdalrasterize.cpp` `GDALCollectRingsFromGeometry`, `gv_rasterize_one_shape`,
//! `gvBurnScanline`, `GDALRasterizeLayers`; `alg/llrasterize.cpp` `GDALdllImageFilledPolygon`,
//! `GDALdllImagePoint`, `GDALdllImageLine`; `alg/gdaltransformer.cpp` `GDALGenImgProjTransform`,
//! `GDALInvGeoTransform`. Vortex passes no `ALL_TOUCHED`: its `-te`, `-tr` and `-of` strings go to
//! `GDALRasterizeOptions`, which ignores them, so this is GDAL's default, the cell-centre rule.
//!
//! In GDAL's replace mode a collection's parts are burned one after another. A feature's polygons
//! and a zone's features are therefore a union, and only the rings of one polygon fill by even-odd.
//!
//! **Chunking.** GDAL splits a raster into swaths when one exceeds its block cache (5% of RAM by
//! default), and then subtracts a swath's offset from pixel coordinates. That can only move a
//! coordinate by its last bits, and no grid checked reaches it: 2^26 cells is 64 MiB.

use super::geometry::{is_clockwise, Part, Pt};
use super::{refused, ZoneLimits};
use crate::error::VortexError;

/// `static_cast<int>` as x64 compiled code does it (`cvttsd2si`): the "integer indefinite" for NaN
/// and anything out of range. Rust's saturating `as` would fill to the far edge instead.
fn c_int(v: f64) -> i32 {
    if v.is_nan() || v >= 2_147_483_648.0 || v <= -2_147_483_649.0 {
        i32::MIN
    } else {
        v as i32
    }
}

/// `GDALInvGeoTransform`'s special case for no rotation.
pub(super) fn invert(gt: &[f64; 6]) -> [f64; 6] {
    [
        -gt[0] / gt[1],
        1.0 / gt[1],
        0.0,
        -gt[3] / gt[5],
        0.0,
        1.0 / gt[5],
    ]
}

/// Where a mask goes while a zone is burned. Only the rows touched are cleared and scanned, so many
/// small zones on a large grid do not each pay for the whole grid.
pub(super) struct Burn<'a> {
    pub nx: i32,
    pub ny: i32,
    pub cells: &'a mut [u8],
    pub first_row: i32,
    pub last_row: i32,
    pub work: &'a mut u64,
}

impl Burn<'_> {
    fn touch(&mut self, y: i32) {
        self.first_row = self.first_row.min(y);
        self.last_row = self.last_row.max(y);
    }

    // gvBurnScanline.
    fn scanline(&mut self, y: i32, start: i32, end: i32) {
        if start > end {
            return;
        }
        let start = start.max(0);
        let end = end.min(self.nx - 1);
        if start > end {
            return;
        }
        self.touch(y);
        let row = y as usize * self.nx as usize;
        self.cells[row + start as usize..=row + end as usize].fill(1);
    }

    fn point(&mut self, y: i32, x: i32) {
        self.touch(y);
        self.cells[y as usize * self.nx as usize + x as usize] = 1;
    }
}

// GDALGenImgProjTransform with no source dataset and no reprojection: an identity geotransform on
// the source side, then the inverse on the destination side. The two pairs of products are kept as
// written, so a -0 or infinity behaves as it does there.
fn to_pixel(p: Pt, inv: &[f64; 6]) -> Pt {
    let x = 0.0 + p.x * 1.0 + p.y * 0.0;
    let y = 0.0 + p.x * 0.0 + p.y * 1.0;
    Pt {
        x: inv[0] + x * inv[1] + y * inv[2],
        y: inv[3] + x * inv[4] + y * inv[5],
    }
}

fn fill_polygon(burn: &mut Burn<'_>, parts: &[usize], px: &[f64], py: &[f64], ints: &mut Vec<i32>) {
    let n = px.len();
    if parts.is_empty() || n == 0 {
        return;
    }
    let mut dminy = py[0];
    let mut dmaxy = py[0];
    for &y in &py[1..] {
        if y < dminy {
            dminy = y;
        }
        if y > dmaxy {
            dmaxy = y;
        }
    }
    let miny = c_int(dminy).max(0);
    let maxy = c_int(dmaxy).min(burn.ny - 1);
    let (minx, maxx) = (0, burn.nx - 1);
    let mut y = miny;
    while y <= maxy {
        let dy = f64::from(y) + 0.5;
        ints.clear();
        let mut part = 0;
        let mut offset = 0;
        for i in 0..n {
            if i == offset + parts[part] {
                offset += parts[part];
                part += 1;
            }
            let (ind1, ind2) = if i == offset {
                (offset + parts[part] - 1, offset)
            } else {
                (i - 1, i)
            };
            let (mut dy1, mut dy2) = (py[ind1], py[ind2]);
            if (dy1 < dy && dy2 < dy) || (dy1 > dy && dy2 > dy) {
                continue;
            }
            let (dx1, dx2);
            if dy1 < dy2 {
                dx1 = px[ind1];
                dx2 = px[ind2];
            } else if dy1 > dy2 {
                std::mem::swap(&mut dy1, &mut dy2);
                dx2 = px[ind1];
                dx1 = px[ind2];
            } else {
                // Bottom horizontal segments are filled here; top ones are skipped.
                if px[ind1] > px[ind2] {
                    let x1 = c_int((px[ind2] + 0.5).floor());
                    let x2 = c_int((px[ind1] + 0.5).floor());
                    if x1 > maxx || x2 <= minx {
                        continue;
                    }
                    burn.scanline(y, x1, x2.wrapping_sub(1));
                }
                continue;
            }
            if dy < dy2 && dy >= dy1 {
                let at = (dy - dy1) * (dx2 - dx1) / (dy2 - dy1) + dx1;
                ints.push(c_int((at + 0.5).floor()));
            }
        }
        ints.sort_unstable();
        let mut i = 0;
        while i + 1 < ints.len() {
            if ints[i] <= maxx && ints[i + 1] > minx {
                burn.scanline(y, ints[i], ints[i + 1].wrapping_sub(1));
            }
            i += 2;
        }
        y += 1;
    }
}

// GDALdllImageLine, with no variant. Points arrive in GDAL's reversed order.
fn burn_line(
    burn: &mut Burn<'_>,
    px: &[f64],
    py: &[f64],
    limits: &ZoneLimits,
) -> Result<(), VortexError> {
    let n = px.len();
    // Bresenham visits every cell along the way, inside the grid or not, so far-off coordinates are
    // a cost a small file could impose. Beyond this, they are refused rather than walked.
    let reach = 1u64 << 24;
    for j in 1..n {
        // NaN counts as far.
        let far = |v: f64, size: i32| v.is_nan() || v.abs() >= (reach + size as u64) as f64;
        if far(px[j - 1], burn.nx)
            || far(px[j], burn.nx)
            || far(py[j - 1], burn.ny)
            || far(py[j], burn.ny)
        {
            return Err(refused(
                "zones",
                "a line runs over 2^24 cells beyond the grid",
            ));
        }
        let mut ix = px[j - 1].floor() as i32;
        let mut iy = py[j - 1].floor() as i32;
        let ix1 = px[j].floor() as i32;
        let iy1 = py[j].floor() as i32;
        let mut dx = (ix1 - ix).abs();
        let mut dy = (iy1 - iy).abs();
        let xs = if ix > ix1 { -1 } else { 1 };
        let ys = if iy > iy1 { -1 } else { 1 };
        *burn.work += u64::from(dx.max(dy) as u32) + 1;
        if *burn.work > limits.max_scan_work {
            return Err(refused(
                "zones",
                "rasterizing them would take over the work limit",
            ));
        }
        let inside = |b: &Burn<'_>, x: i32, y: i32| 0 <= x && x < b.nx && 0 <= y && y < b.ny;
        if dx >= dy {
            let x_err = dy << 1;
            let y_err = x_err - (dx << 1);
            let mut err = x_err - dx;
            if j != n - 1 {
                dx -= 1;
            }
            while dx >= 0 {
                if inside(burn, ix, iy) {
                    burn.point(iy, ix);
                }
                ix += xs;
                if err > 0 {
                    iy += ys;
                    err += y_err;
                } else {
                    err += x_err;
                }
                dx -= 1;
            }
        } else {
            let x_err = dx << 1;
            let y_err = x_err - (dy << 1);
            let mut err = x_err - dy;
            if j != n - 1 {
                dy -= 1;
            }
            while dy >= 0 {
                if inside(burn, ix, iy) {
                    burn.point(iy, ix);
                }
                iy += ys;
                if err > 0 {
                    ix += xs;
                    err += y_err;
                } else {
                    err += x_err;
                }
                dy -= 1;
            }
        }
    }
    Ok(())
}

/// Burn one part. Scratch vectors are reused between parts and grown fallibly.
#[allow(clippy::too_many_arguments)]
pub(super) fn burn_part(
    burn: &mut Burn<'_>,
    part: &Part,
    inv: &[f64; 6],
    limits: &ZoneLimits,
    px: &mut Vec<f64>,
    py: &mut Vec<f64>,
    sizes: &mut Vec<usize>,
    ints: &mut Vec<i32>,
) -> Result<(), VortexError> {
    px.clear();
    py.clear();
    sizes.clear();
    let oom = || refused("zones", "a part's points could not be allocated");
    let push = |p: Pt, px: &mut Vec<f64>, py: &mut Vec<f64>| {
        let q = to_pixel(p, inv);
        px.push(q.x);
        py.push(q.y);
    };
    match part {
        Part::Point(p) => {
            let q = to_pixel(*p, inv);
            // GDALdllImagePoint.
            let x = c_int(q.x.floor());
            let y = c_int(q.y.floor());
            if 0 <= x && x < burn.nx && 0 <= y && y < burn.ny {
                burn.point(y, x);
            }
        }
        Part::Line(line) => {
            px.try_reserve(line.len()).map_err(|_| oom())?;
            py.try_reserve(line.len()).map_err(|_| oom())?;
            // GDALCollectRingsFromGeometry walks a line string from its end.
            for p in line.iter().rev() {
                push(*p, px, py);
            }
            burn_line(burn, px, py, limits)?;
        }
        Part::Polygon(rings) => {
            let total: usize = rings.iter().map(Vec::len).sum();
            px.try_reserve(total).map_err(|_| oom())?;
            py.try_reserve(total).map_err(|_| oom())?;
            ints.try_reserve(total).map_err(|_| oom())?;
            sizes.try_reserve(rings.len()).map_err(|_| oom())?;
            // Rings are walked clockwise: an interior ring's sense decides only which horizontal
            // segments count as bottom ones, as isClockwise decides it for every ring alike.
            for ring in rings {
                if ring.is_empty() {
                    continue;
                }
                if is_clockwise(ring) {
                    for p in ring {
                        push(*p, px, py);
                    }
                } else {
                    for p in ring.iter().rev() {
                        push(*p, px, py);
                    }
                }
                sizes.push(ring.len());
            }
            *burn.work = burn
                .work
                .saturating_add((total as u64).saturating_mul(burn.ny as u64));
            if *burn.work > limits.max_scan_work {
                return Err(refused(
                    "zones",
                    "rasterizing them would take over the work limit",
                ));
            }
            fill_polygon(burn, sizes, px, py, ints);
        }
    }
    Ok(())
}

/// `ZonalStatisticsCalculator.createZoneMasks`' fallback: the cell holding the middle of the extent.
/// `GridCell.contains` is strict on all four sides, so a point exactly on a cell's edge is in no
/// cell, and the zone has no cells. Cell edges are computed as `Grid.getGridCells` computes them.
pub(super) fn centroid_cell(gt: &[f64; 6], nx: usize, ny: usize, x: f64, y: f64) -> Option<usize> {
    let (ox, dx, oy, dy) = (gt[0], gt[1], gt[3], gt[5]);
    let inside = |origin: f64, i: usize, step: f64, v: f64| {
        let start = origin + i as f64 * step;
        let end = start + step;
        v > start.min(end) && v < start.max(end)
    };
    let row = (0..ny).find(|&r| inside(oy, r, dy, y))?;
    let column = (0..nx).find(|&c| inside(ox, c, dx, x))?;
    Some(row * nx + column)
}

/// Part of each cell's area a polygon covers, in the grid's own coordinates. This is the optional
/// area-weighted mode, which answers a different question from the cell-centre rule. Rings are
/// clipped to each row, then to each cell (Sutherland-Hodgman, which keeps the signed area of a
/// concave ring). `add` receives (cell index, signed fraction) with exterior rings positive.
pub(super) fn area_fractions(
    rings: &[Vec<Pt>],
    inv: &[f64; 6],
    nx: usize,
    ny: usize,
    mut add: impl FnMut(usize, f64),
) {
    fn clip(input: &[(f64, f64)], out: &mut Vec<(f64, f64)>, axis_y: bool, low: f64, high: f64) {
        let coordinate = |p: (f64, f64)| if axis_y { p.1 } else { p.0 };
        let pass = |input: &[(f64, f64)], keep_above: bool, at: f64| -> Vec<(f64, f64)> {
            let inside = |v: f64| if keep_above { v >= at } else { v <= at };
            let mut kept = Vec::with_capacity(input.len() + 2);
            let n = input.len();
            for i in 0..n {
                let a = input[i];
                let b = input[(i + 1) % n];
                let (ca, cb) = (coordinate(a), coordinate(b));
                if inside(ca) {
                    kept.push(a);
                }
                if inside(ca) != inside(cb) {
                    let t = (at - ca) / (cb - ca);
                    kept.push((a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)));
                }
            }
            kept
        };
        let above = pass(input, true, low);
        *out = pass(&above, false, high);
    }
    fn shoelace(points: &[(f64, f64)]) -> f64 {
        let n = points.len();
        let mut sum = 0.0;
        for i in 0..n {
            let (a, b) = (points[i], points[(i + 1) % n]);
            sum += a.0 * b.1 - b.0 * a.1;
        }
        0.5 * sum
    }
    for (index, ring) in rings.iter().enumerate() {
        if ring.len() < 3 {
            continue;
        }
        let pixels: Vec<(f64, f64)> = ring
            .iter()
            .map(|p| {
                let q = to_pixel(*p, inv);
                (q.x, q.y)
            })
            .collect();
        let total = shoelace(&pixels);
        if total == 0.0 {
            continue;
        }
        let exterior = index == 0;
        let sign = if (total > 0.0) == exterior { 1.0 } else { -1.0 };
        let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
        for p in &pixels {
            min_y = min_y.min(p.1);
            max_y = max_y.max(p.1);
        }
        let first = min_y.floor().max(0.0) as usize;
        let last = (max_y.ceil().min(ny as f64)).max(0.0) as usize;
        let mut strip = Vec::new();
        let mut cell = Vec::new();
        for row in first..last {
            clip(&pixels, &mut strip, true, row as f64, row as f64 + 1.0);
            if strip.is_empty() {
                continue;
            }
            let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
            for p in &strip {
                min_x = min_x.min(p.0);
                max_x = max_x.max(p.0);
            }
            let from = min_x.floor().max(0.0) as usize;
            let to = (max_x.ceil().min(nx as f64)).max(0.0) as usize;
            for column in from..to {
                clip(&strip, &mut cell, false, column as f64, column as f64 + 1.0);
                if cell.is_empty() {
                    continue;
                }
                let area = shoelace(&cell) * sign;
                if area != 0.0 {
                    add(row * nx + column, area);
                }
            }
        }
    }
}

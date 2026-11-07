//! GDAL 3.2.1's coordinate path for a warp, in its order of operations:
//! `GDALGenImgProjTransform` (`alg/gdaltransformer.cpp`) over `OGRProjCT`
//! (`ogr/ogrct.cpp`), and `GDALApproxTransform` over that.
//!
//! A point that fails holds `HUGE_VAL`, as PROJ reports it, and its flag is false.

use crate::crs::{CrsDef, DatumPolicy, Transformer};
use crate::error::VortexError;
use crate::frame::GeoTransform;

const HUGE_VAL: f64 = f64::INFINITY;

/// `gdalwarp_lib.cpp`: with no `-et`, `dfErrorThreshold = 0.125` source pixels.
pub(crate) const APPROX_ERROR_THRESHOLD: f64 = 0.125;

pub(crate) type Gt = [f64; 6];

pub(crate) fn gt_array(t: &GeoTransform) -> Gt {
    [
        t.origin_x,
        t.dx,
        t.row_rotation,
        t.origin_y,
        t.column_rotation,
        t.dy,
    ]
}

/// `GDALInvGeoTransform` (`alg/gdaltransformer.cpp`).
pub(crate) fn invert_gt(gt: &Gt) -> Option<Gt> {
    if gt[2] == 0.0 && gt[4] == 0.0 && gt[1] != 0.0 && gt[5] != 0.0 {
        return Some([
            -gt[0] / gt[1],
            1.0 / gt[1],
            0.0,
            -gt[3] / gt[5],
            0.0,
            1.0 / gt[5],
        ]);
    }
    let det = gt[1] * gt[5] - gt[2] * gt[4];
    let magnitude = cpp_max(
        cpp_max(gt[1].abs(), gt[2].abs()),
        cpp_max(gt[4].abs(), gt[5].abs()),
    );
    if det.abs() <= 1e-10 * magnitude * magnitude {
        return None;
    }
    let inv_det = 1.0 / det;
    Some([
        (gt[2] * gt[3] - gt[0] * gt[5]) * inv_det,
        gt[5] * inv_det,
        -gt[2] * inv_det,
        (-gt[1] * gt[3] + gt[0] * gt[4]) * inv_det,
        -gt[4] * inv_det,
        gt[1] * inv_det,
    ])
}

/// `std::min`: `b < a ? b : a`, which differs from `f64::min` on NaN and signed zero.
#[inline]
pub(crate) fn cpp_min(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// `std::max`: `a < b ? b : a`.
#[inline]
pub(crate) fn cpp_max(a: f64, b: f64) -> f64 {
    if a < b {
        b
    } else {
        a
    }
}

// ogrct.cpp, "Potentially do longitude wrapping".
fn wrap(x: f64, y: f64, center: Option<f64>) -> f64 {
    match center {
        Some(c) if x != HUGE_VAL && y != HUGE_VAL => {
            if x < c - 180.0 {
                x + 360.0
            } else if x > c + 180.0 {
                x - 360.0
            } else {
                x
            }
        }
        _ => x,
    }
}

/// One direction of `OGRProjCT`, with its `CHECK_WITH_INVERT_PROJ` variant.
#[derive(Debug, Clone)]
struct Reprojection {
    wrapped: Transformer,
    raw: Transformer,
    raw_back: Transformer,
    src_wrap: Option<f64>,
    dst_wrap: Option<f64>,
    /// `ogrct.cpp`: `THRESHOLD`, .1 from a geographic source and 10000 otherwise.
    threshold: f64,
}

impl Reprojection {
    fn new(
        src: &CrsDef,
        dst: &CrsDef,
        policy: DatumPolicy,
        src_wrap: Option<f64>,
        dst_wrap: Option<f64>,
    ) -> Result<Self, VortexError> {
        let raw = Transformer::new(src, dst, policy)?;
        Ok(Reprojection {
            wrapped: raw.clone().with_center_long(src_wrap, dst_wrap),
            raw_back: Transformer::new(dst, src, policy)?,
            raw,
            src_wrap: src_wrap.filter(|_| src.is_geographic()),
            dst_wrap: dst_wrap.filter(|_| dst.is_geographic()),
            threshold: if src.is_geographic() { 0.1 } else { 10000.0 },
        })
    }

    #[inline]
    fn apply(&self, x: f64, y: f64, check: bool) -> Option<(f64, f64)> {
        if !check {
            return self.wrapped.transform(x, y).ok();
        }
        // ogrct.cpp, the bCheckWithInvertProj branch: compare the round trip with the input.
        let ox = wrap(x, y, self.src_wrap);
        let (tx, ty) = self.raw.transform(ox, y).ok()?;
        let (bx, by) = self
            .raw_back
            .transform(tx, ty)
            .unwrap_or((HUGE_VAL, HUGE_VAL));
        if (bx - ox).abs() > self.threshold || (by - y).abs() > self.threshold {
            return None;
        }
        Some((wrap(tx, ty, self.dst_wrap), ty))
    }
}

/// `GDALGenImgProjTransform` between source pixels and destination pixels.
#[derive(Debug, Clone)]
pub(crate) struct GenImg {
    forward: Reprojection,
    reverse: Reprojection,
    src_gt: Gt,
    src_inv: Gt,
    dst_gt: Gt,
    dst_inv: Gt,
    /// `GDALCreateApproxTransformer`, both directions.
    max_error: f64,
}

/// `bDstToSrc` in GDAL's argument list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    DstToSrc,
    SrcToDst,
}

impl GenImg {
    /// Starts with GDAL's identity destination, as `GDALCreateGenImgProjTransformer2` does
    /// when `hDstDS` is null: destination pixels are then target coordinates.
    pub(crate) fn new(
        src: &CrsDef,
        dst: &CrsDef,
        policy: DatumPolicy,
        src_gt: Gt,
        center_long: Option<f64>,
    ) -> Result<Self, VortexError> {
        let src_inv = invert_gt(&src_gt).ok_or_else(|| VortexError::Refused {
            what: "source geotransform".into(),
            reason: "it cannot be inverted".into(),
        })?;
        Ok(GenImg {
            forward: Reprojection::new(src, dst, policy, center_long, None)?,
            reverse: Reprojection::new(dst, src, policy, None, center_long)?,
            src_gt,
            src_inv,
            dst_gt: [0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            dst_inv: [0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            max_error: APPROX_ERROR_THRESHOLD,
        })
    }

    pub(crate) fn with_destination(mut self, dst_gt: Gt) -> Result<Self, VortexError> {
        self.dst_inv = invert_gt(&dst_gt).ok_or_else(|| VortexError::Refused {
            what: "destination geotransform".into(),
            reason: "it cannot be inverted".into(),
        })?;
        self.dst_gt = dst_gt;
        Ok(self)
    }

    /// The base transformer. `check` is `CHECK_WITH_INVERT_PROJ`.
    pub(crate) fn base(
        &self,
        direction: Direction,
        check: bool,
        x: &mut [f64],
        y: &mut [f64],
        ok: &mut [bool],
    ) {
        let (first, reproject, last) = match direction {
            Direction::DstToSrc => (&self.dst_gt, &self.reverse, &self.src_inv),
            Direction::SrcToDst => (&self.src_gt, &self.forward, &self.dst_inv),
        };
        for i in 0..x.len() {
            if x[i] == HUGE_VAL || y[i] == HUGE_VAL {
                ok[i] = false;
                continue;
            }
            let gx = first[0] + x[i] * first[1] + y[i] * first[2];
            let gy = first[3] + x[i] * first[4] + y[i] * first[5];
            match reproject.apply(gx, gy, check) {
                Some((ox, oy)) if ox != HUGE_VAL && oy != HUGE_VAL => {
                    x[i] = last[0] + ox * last[1] + oy * last[2];
                    y[i] = last[3] + ox * last[4] + oy * last[5];
                    ok[i] = true;
                }
                _ => {
                    x[i] = HUGE_VAL;
                    y[i] = HUGE_VAL;
                    ok[i] = false;
                }
            }
        }
    }

    /// `GDALApproxTransform`.
    pub(crate) fn approx(
        &self,
        direction: Direction,
        check: bool,
        x: &mut [f64],
        y: &mut [f64],
        ok: &mut [bool],
    ) {
        let n = x.len();
        if n == 0 {
            return;
        }
        let mid = (n - 1) / 2;
        if y[0] != y[n - 1]
            || y[0] != y[mid]
            || x[0] == x[n - 1]
            || x[0] == x[mid]
            || self.max_error == 0.0
            || n <= 5
        {
            self.base(direction, check, x, y, ok);
            return;
        }
        let mut x2 = [x[0], x[mid], x[n - 1]];
        let mut y2 = [y[0], y[mid], y[n - 1]];
        let mut ok2 = [false; 3];
        self.base(direction, check, &mut x2, &mut y2, &mut ok2);
        if !ok2[0] || !ok2[1] || !ok2[2] {
            self.base(direction, check, x, y, ok);
            return;
        }
        self.approx_internal(direction, check, x, y, ok, x2, y2);
    }

    // GDALApproxTransformInternal. The depth is the log of the scanline length.
    #[allow(clippy::too_many_arguments)]
    fn approx_internal(
        &self,
        direction: Direction,
        check: bool,
        x: &mut [f64],
        y: &mut [f64],
        ok: &mut [bool],
        xs: [f64; 3],
        ys: [f64; 3],
    ) {
        let n = x.len();
        let mid = (n - 1) / 2;
        let delta_x = (xs[2] - xs[0]) / (x[n - 1] - x[0]);
        let delta_y = (ys[2] - ys[0]) / (x[n - 1] - x[0]);
        let error = ((xs[0] + delta_x * (x[mid] - x[0])) - xs[1]).abs()
            + ((ys[0] + delta_y * (x[mid] - x[0])) - ys[1]).abs();
        if error > self.max_error {
            let (q1, q3) = ((mid - 1) / 2, mid + (n - mid - 1) / 2);
            let mut x_mid = [x[q1], x[mid - 1], x[q3]];
            let mut y_mid = [y[q1], y[mid - 1], y[q3]];
            let base_half1 = mid <= 5
                || y[0] != y[mid - 1]
                || y[0] != y[q1]
                || x[0] == x[mid - 1]
                || x[0] == x[q1];
            let base_half2 = n - mid <= 5
                || y[mid] != y[n - 1]
                || y[mid] != y[q3]
                || x[mid] == x[n - 1]
                || x[mid] == x[q3];
            let mut ok2 = [true; 3];
            // bSuccess stays FALSE when both halves use the base transformer.
            let mut success = true;
            if !base_half1 && !base_half2 {
                self.base(direction, check, &mut x_mid, &mut y_mid, &mut ok2);
            } else if !base_half1 {
                self.base(
                    direction,
                    check,
                    &mut x_mid[..2],
                    &mut y_mid[..2],
                    &mut ok2[..2],
                );
            } else if !base_half2 {
                self.base(
                    direction,
                    check,
                    &mut x_mid[2..],
                    &mut y_mid[2..],
                    &mut ok2[2..],
                );
            } else {
                success = false;
            }
            if !success || !ok2[0] || !ok2[1] || !ok2[2] {
                self.base_range(direction, check, x, y, ok, 1, mid);
                self.base_range(direction, check, x, y, ok, mid + 1, n - 1);
                set_point(x, y, ok, 0, xs[0], ys[0]);
                set_point(x, y, ok, mid, xs[1], ys[1]);
                set_point(x, y, ok, n - 1, xs[2], ys[2]);
                return;
            }
            if !base_half1 {
                self.approx_internal(
                    direction,
                    check,
                    &mut x[..mid],
                    &mut y[..mid],
                    &mut ok[..mid],
                    [xs[0], x_mid[0], x_mid[1]],
                    [ys[0], y_mid[0], y_mid[1]],
                );
            } else {
                self.base_range(direction, check, x, y, ok, 1, mid);
                set_point(x, y, ok, 0, xs[0], ys[0]);
            }
            if !base_half2 {
                self.approx_internal(
                    direction,
                    check,
                    &mut x[mid..],
                    &mut y[mid..],
                    &mut ok[mid..],
                    [xs[1], x_mid[2], xs[2]],
                    [ys[1], y_mid[2], ys[2]],
                );
            } else {
                self.base_range(direction, check, x, y, ok, mid + 1, n - 1);
                set_point(x, y, ok, mid, xs[1], ys[1]);
                set_point(x, y, ok, n - 1, xs[2], ys[2]);
            }
            return;
        }
        // Backwards, so x[0] stays the origin of the distance until the last step.
        for i in (0..n).rev() {
            let dist = x[i] - x[0];
            x[i] = xs[0] + delta_x * dist;
            y[i] = ys[0] + delta_y * dist;
            ok[i] = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn base_range(
        &self,
        direction: Direction,
        check: bool,
        x: &mut [f64],
        y: &mut [f64],
        ok: &mut [bool],
        from: usize,
        to: usize,
    ) {
        self.base(
            direction,
            check,
            &mut x[from..to],
            &mut y[from..to],
            &mut ok[from..to],
        );
    }
}

fn set_point(x: &mut [f64], y: &mut [f64], ok: &mut [bool], i: usize, px: f64, py: f64) {
    x[i] = px;
    y[i] = py;
    ok[i] = true;
}

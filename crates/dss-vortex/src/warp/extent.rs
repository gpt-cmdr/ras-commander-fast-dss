//! The destination grid when GDAL has to choose it: `GDALWarpCreateOutput`
//! (`apps/gdalwarp_lib.cpp`) and `GDALSuggestedWarpOutput2` (`alg/gdaltransformer.cpp`),
//! GDAL 3.2.1. Snapping of a stated envelope is `Resampler.java`'s and lives in `mod.rs`.

use super::transformer::{cpp_max, cpp_min, Direction, GenImg, Gt};
use crate::error::VortexError;

fn refused(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "warp output extent".into(),
        reason: reason.into(),
    }
}

/// The result of `GDALWarpCreateOutput`: geotransform and size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Output {
    pub gt: Gt,
    pub nx: f64,
    pub ny: f64,
    /// `CHECK_WITH_INVERT_PROJ` stays on in the transformer `gdalwarp` goes on to warp with, when
    /// the extent needed the retry: the option is cleared, but the transformer is not refreshed.
    pub check: bool,
}

fn one(xf: &GenImg, direction: Direction, check: bool, x: &mut f64, y: &mut f64) -> bool {
    let (mut xs, mut ys, mut ok) = ([*x], [*y], [false]);
    xf.base(direction, check, &mut xs, &mut ys, &mut ok);
    *x = xs[0];
    *y = ys[0];
    ok[0]
}

/// `te` is `[min_x, min_y, max_x, max_y]`, `tr` the cell size.
pub(crate) fn create_output(
    xf: &GenImg,
    src_nx: i32,
    src_ny: i32,
    te: Option<[f64; 4]>,
    tr: Option<f64>,
) -> Result<Output, VortexError> {
    // GDAL reads an extent of four zeros as no extent.
    let te = te.filter(|e| e.iter().any(|v| *v != 0.0));
    if let (Some(e), Some(res)) = (te, tr) {
        return Ok(from_extent_and_res(e, res, false));
    }
    let res_from_extent = te.map_or(f64::INFINITY, |e| {
        resolution_from_extent(xf, src_nx, src_ny, e)
    });

    let mut suggested = suggested_output(xf, src_nx, src_ny, false)?;
    let mut check = false;
    if !extent_round_trips(xf, &suggested) {
        // "retry with CHECK_WITH_INVERT_PROJ=TRUE".
        suggested = suggested_output(xf, src_nx, src_ny, true)?;
        check = true;
    }
    let (mut res_x, mut res_y) = (suggested.gt[1], suggested.gt[5].abs());
    if res_from_extent.is_finite() {
        res_x = res_from_extent;
        res_y = res_from_extent;
    }
    let [min_x, min_y, max_x, max_y] = suggested.extent;
    let gt = [min_x, res_x, 0.0, max_y, 0.0, -res_y];
    let pixels = ((max_x - min_x) / res_x + 0.5).trunc();
    let lines = ((max_y - min_y) / res_y + 0.5).trunc();

    if let Some(res) = tr {
        // No -te here: the extent comes from the suggested grid, at its own resolution.
        let e = [gt[0], gt[3] + gt[5] * lines, gt[0] + gt[1] * pixels, gt[3]];
        return Ok(from_extent_and_res(e, res, check));
    }
    if let Some(e) = te {
        // The -te branch with no -tr or -ts.
        let nx = ((e[2] - e[0] + res_x / 2.0) / res_x).trunc();
        let ny = (((e[3] - e[1]).abs() + res_y / 2.0) / res_y).trunc();
        let x_res = (e[2] - e[0]) / nx;
        let y_res = (e[3] - e[1]) / ny;
        return Ok(Output {
            gt: [e[0], x_res, 0.0, e[3], 0.0, -y_res],
            nx,
            ny,
            check,
        });
    }
    Ok(Output {
        gt,
        nx: pixels,
        ny: lines,
        check,
    })
}

fn from_extent_and_res(e: [f64; 4], res: f64, check: bool) -> Output {
    let [min_x, min_y, max_x, max_y] = e;
    Output {
        gt: [
            min_x,
            res,
            0.0,
            max_y,
            0.0,
            if max_y > min_y { -res } else { res },
        ],
        nx: ((max_x - min_x + res / 2.0) / res).trunc(),
        ny: (((max_y - min_y).abs() + res / 2.0) / res).trunc(),
        check,
    }
}

// gdalwarp_lib.cpp, bKnownTargetExtentButNotResolution.
fn resolution_from_extent(xf: &GenImg, src_nx: i32, src_ny: i32, e: [f64; 4]) -> f64 {
    const N: usize = 10;
    let [min_x, min_y, max_x, max_y] = e;
    let eps = cpp_min(max_x - min_x, (max_y - min_y).abs()) / 1000.0;
    let (mut px, mut py) = (Vec::with_capacity(3 * N * N), Vec::with_capacity(3 * N * N));
    for iy in 0..N {
        for ix in 0..N {
            let x = min_x + ix as f64 * (max_x - min_x) / (N as f64 - 1.0);
            let y = min_y + iy as f64 * (max_y - min_y) / (N as f64 - 1.0);
            px.push(x);
            py.push(y);
            px.push(if ix == N - 1 { x - eps } else { x + eps });
            py.push(y);
            px.push(x);
            py.push(if iy == N - 1 { y - eps } else { y + eps });
        }
    }
    let mut ok = vec![false; px.len()];
    xf.base(Direction::DstToSrc, false, &mut px, &mut py, &mut ok);
    let (fnx, fny) = (f64::from(src_nx), f64::from(src_ny));
    let distance = |x: f64, y: f64| (x * x + y * y).sqrt();
    let mut res = Vec::new();
    for i in (0..px.len()).step_by(3) {
        if ok[i]
            && ok[i + 1]
            && ok[i + 2]
            && px[i] >= 0.0
            && px[i] <= fnx
            && py[i] >= 0.0
            && py[i] <= fny
        {
            let r1 = eps.abs() / distance(px[i + 1] - px[i], py[i + 1] - py[i]);
            let r2 = eps.abs() / distance(px[i + 2] - px[i], py[i + 2] - py[i]);
            if r1.is_finite() && r2.is_finite() {
                res.push((r1 + r2) / 2.0);
            }
        }
    }
    res.sort_by(f64::total_cmp);
    let mut found = f64::INFINITY;
    if !res.is_empty() {
        let median = res[res.len() / 2];
        for r in &res {
            if *r > median / 10.0 {
                found = cpp_min(found, *r);
                break;
            }
        }
    }
    found
}

#[derive(Debug, Clone, Copy)]
struct Suggested {
    gt: Gt,
    extent: [f64; 4],
    pixels: i32,
    lines: i32,
}

// gdalwarp_lib.cpp: "Check that the edges of the target image are in the validity area".
fn extent_round_trips(xf: &GenImg, s: &Suggested) -> bool {
    const N_STEPS: i32 = 20;
    let [min_x, min_y, max_x, max_y] = s.extent;
    let mut success = true;
    let mut i = 0;
    while i <= N_STEPS && success {
        let mut j = 0;
        while j <= N_STEPS && success {
            let ratio_i = f64::from(i) * 1.0 / f64::from(N_STEPS);
            let ratio_j = f64::from(j) * 1.0 / f64::from(N_STEPS);
            let expected_x = (1.0 - ratio_i) * min_x + ratio_i * max_x;
            let expected_y = (1.0 - ratio_j) * min_y + ratio_j * max_y;
            let (mut x, mut y) = (expected_x, expected_y);
            if !one(xf, Direction::DstToSrc, false, &mut x, &mut y) {
                success = false;
            }
            if !one(xf, Direction::SrcToDst, false, &mut x, &mut y) {
                success = false;
            }
            if (x - expected_x).abs() > (max_x - min_x) / f64::from(s.pixels)
                || (y - expected_y).abs() > (max_y - min_y) / f64::from(s.lines)
            {
                success = false;
            }
            j += 1;
        }
        i += 1;
    }
    success
}

// GDALSuggestedWarpOutput2_MustAdjustForRightBorder and ...ForBottomBorder: the same test on two
// edges. The sampled ratio snaps to 1 only in the first loop, as in GDAL.
fn must_adjust(
    xf: &GenImg,
    check: bool,
    point: impl Fn(f64) -> (f64, f64),
    pixel_x: f64,
    pixel_y: f64,
) -> bool {
    let (mut ax, mut ay) = (Vec::with_capacity(21), Vec::with_capacity(21));
    let mut ratio = 0.0f64;
    while ratio <= 1.01 {
        if ratio > 0.99 {
            ratio = 1.0;
        }
        let (x, y) = point(ratio);
        ax.push(x);
        ay.push(y);
        ratio += 0.05;
    }
    let mut ok = vec![false; ax.len()];
    xf.base(Direction::DstToSrc, check, &mut ax, &mut ay, &mut ok);
    xf.base(Direction::SrcToDst, check, &mut ax, &mut ay, &mut ok);
    let (mut samples, mut bad) = (0usize, 0usize);
    let mut ratio = 0.0f64;
    while ratio <= 1.01 && samples < ax.len() {
        let (ex, ey) = point(ratio);
        if (ax[samples] - ex).abs() > pixel_x || (ay[samples] - ey).abs() > pixel_y {
            bad += 1;
        }
        samples += 1;
        ratio += 0.05;
    }
    bad == samples
}

fn suggested_output(
    xf: &GenImg,
    in_x: i32,
    in_y: i32,
    check: bool,
) -> Result<Suggested, VortexError> {
    const N_PIXELSTEP: i32 = 50;
    let mut steps = (f64::from(in_y.min(in_x)) / f64::from(N_PIXELSTEP) + 0.5) as i32;
    if steps < 20 {
        steps = 20;
    }
    let steps = steps.min(100) as usize;
    let step = 1.0 / steps as f64;
    let (fx, fy) = (f64::from(in_x), f64::from(in_y));
    let sample_max = (steps + 1) * (steps + 1);
    let edge = 4 * (steps + 1);
    let (mut px, mut py) = (
        Vec::with_capacity(sample_max),
        Vec::with_capacity(sample_max),
    );
    px.resize(edge, 0.0);
    py.resize(edge, 0.0);
    for i in 0..=steps {
        let ratio = if i == steps { 1.0 } else { i as f64 * step };
        px[i] = ratio * fx;
        py[i] = 0.0;
        px[steps + 1 + i] = ratio * fx;
        py[steps + 1 + i] = fy;
        px[2 * (steps + 1) + i] = 0.0;
        py[2 * (steps + 1) + i] = ratio * fy;
        px[3 * (steps + 1) + i] = fx;
        py[3 * (steps + 1) + i] = ratio * fy;
    }
    let mut ok = vec![true; edge];
    xf.base(Direction::SrcToDst, check, &mut px, &mut py, &mut ok);

    // Sign discontinuity: Trac #7243.
    const UNINIT: i32 = -2;
    const INVALID: i32 = 0;
    let in_range = |sign: f64, x: f64| {
        (sign * x > 0.0 && sign * x <= 180.0) || (x - sign * -180.0).abs() < 1e-8
    };
    let mut failed = 0usize;
    let mut sign = UNINIT;
    for i in 0..edge {
        if !ok[i] {
            failed += 1;
        } else if sign == 1 || sign == -1 {
            if !in_range(f64::from(sign), px[i]) {
                sign = INVALID;
            }
        } else if sign == UNINIT {
            for candidate in [-1, 1] {
                if in_range(f64::from(candidate), px[i]) {
                    sign = candidate;
                    break;
                }
            }
            if sign == UNINIT {
                sign = INVALID;
            }
        }
    }
    if sign == 1 || sign == -1 {
        let s = f64::from(sign);
        for i in 0..edge {
            if ok[i] && (px[i] - s * -180.0).abs() < 1e-8 {
                let mut ax = [s * -180.0, s * 180.0];
                let mut ay = [py[i], py[i]];
                let mut aok = [false; 2];
                xf.base(Direction::DstToSrc, check, &mut ax, &mut ay, &mut aok);
                if (ax[0] - ax[1]).abs() < 1e-8 && (ay[0] - ay[1]).abs() < 1e-8 {
                    px[i] = s * 180.0;
                }
            }
        }
    }

    // "Check if the computed target coordinates are revertable."
    if failed == 0 {
        let (mut rx, mut ry) = (px.clone(), py.clone());
        xf.base(Direction::DstToSrc, check, &mut rx, &mut ry, &mut ok);
        for i in 0..edge {
            if failed != 0 {
                break;
            }
            if !ok[i] {
                failed += 1;
                break;
            }
            let mut ratio = (i % (steps + 1)) as f64 * step;
            if ratio > 0.99 {
                ratio = 1.0;
            }
            let (ex, ey) = if i < steps + 1 {
                (ratio * fx, 0.0)
            } else if i < 2 * (steps + 1) {
                (ratio * fx, fy)
            } else if i < 3 * (steps + 1) {
                (0.0, ratio * fy)
            } else {
                (fx, ratio * fy)
            };
            if (rx[i] - ex).abs() > fx / steps as f64 || (ry[i] - ey).abs() > fy / steps as f64 {
                failed += 1;
            }
        }
    }

    // "build a fairly detailed internal grid of points".
    if failed > 0 {
        px.clear();
        py.clear();
        for i in 0..=steps {
            let ratio = if i == steps { 1.0 } else { i as f64 * step };
            for j in 0..=steps {
                let ratio2 = if j == steps { 1.0 } else { j as f64 * step };
                px.push(ratio2 * fx);
                py.push(ratio * fy);
            }
        }
        ok.clear();
        ok.resize(sample_max, true);
        xf.base(Direction::SrcToDst, check, &mut px, &mut py, &mut ok);
    }
    let samples = px.len();

    // "Collect the bounds, ignoring any failed points."
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut got = false;
    let mut include = |x: f64, y: f64| {
        if got {
            min_x = cpp_min(min_x, x);
            min_y = cpp_min(min_y, y);
            max_x = cpp_max(max_x, x);
            max_y = cpp_max(max_y, y);
        } else {
            got = true;
            min_x = x;
            max_x = x;
            min_y = y;
            max_y = y;
        }
    };
    failed = 0;
    for i in 0..samples {
        let (x_i, y_i) = if samples == sample_max {
            (i % (steps + 1), i / (steps + 1))
        } else if i < 2 * (steps + 1) {
            (i % (steps + 1), if i < steps + 1 { 0 } else { steps })
        } else {
            (0, 0)
        };
        if x_i > 0 && (ok[i - 1] || ok[i]) {
            let (mut out_before, mut out_after) = (px[i - 1], px[i]);
            let mut in_before = (x_i - 1) as f64 * fx / steps as f64;
            let mut in_after = x_i as f64 * fx / steps as f64;
            let (mut valid_before, mut valid_after) = (ok[i - 1], ok[i]);
            let mut iter = 0;
            // Dichotomy towards a discontinuity where the target x changes sign.
            while (!valid_before || !valid_after || out_before * out_after < 0.0) && iter < 16 {
                let mut x = (in_before + in_after) / 2.0;
                let mut y = y_i as f64 * fy / steps as f64;
                if !one(xf, Direction::SrcToDst, check, &mut x, &mut y) {
                    if !valid_before {
                        in_before = (in_before + in_after) / 2.0;
                    } else if !valid_after {
                        in_after = (in_before + in_after) / 2.0;
                    } else {
                        break;
                    }
                } else {
                    include(x, y);
                    if !valid_before || out_before * x < 0.0 {
                        valid_after = true;
                        in_after = (in_before + in_after) / 2.0;
                        out_after = x;
                    } else {
                        valid_before = true;
                        out_before = x;
                        in_before = (in_before + in_after) / 2.0;
                    }
                }
                iter += 1;
            }
        }
        if !ok[i] {
            failed += 1;
            continue;
        }
        include(px[i], py[i]);
    }
    if failed + 10 > samples {
        return Err(refused(format!(
            "{failed} of {samples} edge points failed to transform"
        )));
    }

    // The diagonal gives the pixel size.
    let (mut delta_x, mut delta_y) = (0.0, 0.0);
    if ok[0] && ok[samples - 1] {
        delta_x = px[samples - 1] - px[0];
        delta_y = py[samples - 1] - py[0];
    }
    if delta_x == 0.0 || delta_y == 0.0 {
        delta_x = max_x - min_x;
        delta_y = max_y - min_y;
    }
    let diagonal = (delta_x * delta_x + delta_y * delta_y).sqrt();
    let pixel = diagonal / (fx * fx + fy * fy).sqrt();
    let d_pixels = (max_x - min_x) / pixel;
    let d_lines = (max_y - min_y) / pixel;
    let limit = f64::from(i32::MAX - 1);
    if d_pixels > limit || d_lines > limit {
        return Err(refused("the computed dimensions are too big"));
    }
    let pixels = (d_pixels + 0.5) as i32;
    let lines = (d_lines + 0.5) as i32;
    let (fp, fl) = (f64::from(pixels), f64::from(lines));

    let (mut pixel_x, mut pixel_y) = (pixel, pixel);
    for ratio in [0.000, 0.001, 0.010, 0.100, 1.000] {
        let try_x = pixel_x - pixel_x * ratio / fp;
        let right = min_x + fp * try_x;
        if !must_adjust(
            xf,
            check,
            |r| (right, max_y - pixel_y * r * fl),
            try_x,
            pixel_y,
        ) {
            pixel_x = try_x;
            break;
        }
    }
    for ratio in [0.000, 0.001, 0.010, 0.100, 1.000] {
        let try_y = pixel_y - pixel_y * ratio / fl;
        let bottom = max_y - fl * try_y;
        if !must_adjust(
            xf,
            check,
            |r| (min_x + pixel_x * r * fp, bottom),
            pixel_x,
            try_y,
        ) {
            pixel_y = try_y;
            break;
        }
    }
    let max_x_out = min_x + fp * pixel_x;
    let min_y_out = max_y - fl * pixel_y;
    Ok(Suggested {
        gt: [min_x, pixel_x, 0.0, max_y, 0.0, -pixel_y],
        extent: [min_x, min_y_out, max_x_out, max_y],
        pixels,
        lines,
    })
}

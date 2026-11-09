//! Test-only port of the P41 prototype: GDAL 3.2.1 warp and transformer logic and PROJ 7.2.0 `aea`,
//! `lcc`, `stere`, kept to prove cross-platform bits. It is not the production warp. Licences:
//! see `NOTICE`.

use std::cell::Cell;

/// `dss_vortex::maths`, with a digest of every call: name, input bits, result bits.
mod m {
    use super::Cell;
    use dss_vortex::maths;
    thread_local! {
        pub static STREAM: Cell<u64> = const { Cell::new(0xcbf2_9ce4_8422_2325) };
        pub static CALLS: Cell<u64> = const { Cell::new(0) };
    }
    fn mix(id: u64, x: f64, y: f64, r: f64) -> f64 {
        // A NaN's sign from an invalid operation differs between x86 and ARM, and NaN is the mask.
        let canon = |v: f64| {
            if v.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                v.to_bits()
            }
        };
        STREAM.with(|s| {
            let mut h = s.get();
            for word in [id, canon(x), canon(y), canon(r)] {
                h = (h ^ word).wrapping_mul(0x0000_0100_0000_01b3);
                h ^= h >> 29;
            }
            s.set(h);
        });
        CALLS.with(|c| c.set(c.get() + 1));
        r
    }
    pub fn sin(x: f64) -> f64 {
        mix(0, x, 0.0, maths::sin(x))
    }
    pub fn cos(x: f64) -> f64 {
        mix(1, x, 0.0, maths::cos(x))
    }
    pub fn tan(x: f64) -> f64 {
        mix(2, x, 0.0, maths::tan(x))
    }
    pub fn asin(x: f64) -> f64 {
        mix(3, x, 0.0, maths::asin(x))
    }
    pub fn atan2(y: f64, x: f64) -> f64 {
        mix(5, y, x, maths::atan2(y, x))
    }
    pub fn log(x: f64) -> f64 {
        mix(6, x, 0.0, maths::log(x))
    }
    pub fn pow(x: f64, y: f64) -> f64 {
        mix(7, x, y, maths::pow(x, y))
    }
    pub fn hypot(x: f64, y: f64) -> f64 {
        mix(8, x, y, maths::hypot(x, y))
    }
}

/// One case's parameters, as raw values.
pub struct Case<'a> {
    pub method: &'a str,
    pub proj: &'a str,
    pub nx: i32,
    pub ny: i32,
    pub ox: f64,
    pub dx: f64,
    pub oy: f64,
    pub dy: f64,
    pub nodata: f64,
    pub te: [f64; 4],
    pub tr: f64,
    pub r: f64,
    pub lat1: f64,
    pub lat2: f64,
    pub lat0: f64,
    pub lon0: f64,
    pub k: f64,
}

const DEG_TO_RAD: f64 = 0.017453292519943296;
const HALFPI: f64 = std::f64::consts::FRAC_PI_2;
const FORTPI: f64 = std::f64::consts::FRAC_PI_4;
const PI: f64 = std::f64::consts::PI;
const TWOPI: f64 = 6.28318530717958647693;

// ---- PROJ 7.2.0 adjlon.cpp
fn adjlon(mut lon: f64) -> f64 {
    if lon.abs() < PI + 1e-12 {
        return lon;
    }
    lon += PI;
    lon -= TWOPI * (lon / TWOPI).floor();
    lon -= PI;
    lon
}

// ---- PROJ 7.2.0 qsfn.cpp, msfn.cpp
fn qsfn(sinphi: f64, e: f64, one_es: f64) -> f64 {
    let con = e * sinphi;
    let div1 = 1.0 - con * con;
    let div2 = 1.0 + con;
    one_es * (sinphi / div1 - (0.5 / e) * m::log((1.0 - con) / div2))
}
fn msfn(sinphi: f64, cosphi: f64, es: f64) -> f64 {
    cosphi / (1.0 - es * sinphi * sinphi).sqrt()
}

// ---- PROJ 7.2.0 aea.cpp, ellipsoidal inverse only; +ellps=GRS80
struct Aea {
    a: f64,
    ra: f64,
    e: f64,
    one_es: f64,
    lam0: f64,
    n: f64,
    c: f64,
    dd: f64,
    rho0: f64,
    ec: f64,
}
impl Aea {
    fn new(lat1: f64, lat2: f64, lat0: f64, lon0: f64) -> Aea {
        let a = 6378137.0f64;
        let rf = 298.257222101f64;
        let f = 1.0 / rf;
        let es = 2.0 * f - f * f; // ell_set.cpp ellps_shape
        let e = es.sqrt();
        let one_es = 1.0 - es;
        let ra = 1.0 / a;
        let phi1 = lat1 * DEG_TO_RAD;
        let phi2 = lat2 * DEG_TO_RAD;
        let phi0 = lat0 * DEG_TO_RAD;
        let lam0 = lon0 * DEG_TO_RAD;
        let mut sinphi = m::sin(phi1);
        let mut n = sinphi;
        let mut cosphi = m::cos(phi1);
        let m1 = msfn(sinphi, cosphi, es);
        let ml1 = qsfn(sinphi, e, one_es);
        if (phi1 - phi2).abs() >= 1e-10 {
            sinphi = m::sin(phi2);
            cosphi = m::cos(phi2);
            let m2 = msfn(sinphi, cosphi, es);
            let ml2 = qsfn(sinphi, e, one_es);
            n = (m1 * m1 - m2 * m2) / (ml2 - ml1);
        }
        let ec = 1.0 - 0.5 * one_es * m::log((1.0 - e) / (1.0 + e)) / e;
        let c = m1 * m1 + n * ml1;
        let dd = 1.0 / n;
        let rho0 = dd * (c - n * qsfn(m::sin(phi0), e, one_es)).sqrt();
        Aea {
            a,
            ra,
            e,
            one_es,
            lam0,
            n,
            c,
            dd,
            rho0,
            ec,
        }
    }
    // inv_prepare, aea_e_inverse, inv_finalize
    fn inv(&self, x: f64, y: f64) -> (f64, f64) {
        let _ = self.a;
        let xin = (1.0 * x - 0.0) * self.ra;
        let yin = (1.0 * y - 0.0) * self.ra;
        let xx = xin;
        let yy = self.rho0 - yin;
        let rho = m::hypot(xx, yy);
        let mut phi = rho / self.dd;
        phi = (self.c - phi * phi) / self.n;
        if (self.ec - phi.abs()).abs() > 1e-7 {
            phi = phi1_(phi, self.e, self.one_es);
        } else {
            phi = if phi < 0.0 { -HALFPI } else { HALFPI };
        }
        let lam = m::atan2(xx, yy) / self.n;
        let lam = lam + 0.0 + self.lam0;
        (adjlon(lam), phi)
    }
}
fn phi1_(qs: f64, te: f64, tone_es: f64) -> f64 {
    let mut phi = m::asin(0.5 * qs);
    let mut i = 15;
    loop {
        let sinpi = m::sin(phi);
        let cospi = m::cos(phi);
        let con = te * sinpi;
        let com = 1.0 - con * con;
        let dphi = 0.5 * com * com / cospi
            * (qs / tone_es - sinpi / com + 0.5 / te * m::log((1.0 - con) / (1.0 + con)));
        phi += dphi;
        if !(dphi.abs() > 1.0e-10) {
            return phi;
        }
        i -= 1;
        if i < 0 {
            return f64::INFINITY;
        }
    }
}

// fwd_prepare common part for an angular input
fn fwd_prepare(mut lam: f64, mut phi: f64, lam0: f64) -> (f64, f64) {
    if phi > HALFPI {
        phi = HALFPI;
    }
    if phi < -HALFPI {
        phi = -HALFPI;
    }
    lam = adjlon(lam);
    lam = (lam - 0.0) - lam0;
    lam = adjlon(lam);
    (lam, phi)
}

enum Target {
    // +proj=lcc spherical forward (lcc.cpp)
    Lcc {
        a: f64,
        k0: f64,
        lam0: f64,
        n: f64,
        c: f64,
        rho0: f64,
    },
    // +proj=stere spherical polar forward (stere.cpp)
    Stere {
        a: f64,
        lam0: f64,
        akm1: f64,
        phits_ok: bool,
    },
    // +proj=unitconvert rad to deg (unitconvert.cpp)
    LonLat {
        factor: f64,
    },
}
impl Target {
    fn lcc(lat1: f64, lat2: f64, lat0: f64, lon0: f64, r: f64) -> Target {
        let phi1 = lat1 * DEG_TO_RAD;
        let phi2 = lat2 * DEG_TO_RAD;
        let phi0 = lat0 * DEG_TO_RAD;
        let sinphi = m::sin(phi1);
        let mut n = sinphi;
        let cosphi = m::cos(phi1);
        if (phi1 - phi2).abs() >= 1e-10 {
            n = m::log(cosphi / m::cos(phi2))
                / m::log(m::tan(FORTPI + 0.5 * phi2) / m::tan(FORTPI + 0.5 * phi1));
        }
        let c = cosphi * m::pow(m::tan(FORTPI + 0.5 * phi1), n) / n;
        let rho0 = if ((phi0).abs() - HALFPI).abs() < 1e-10 {
            0.0
        } else {
            c * m::pow(m::tan(FORTPI + 0.5 * phi0), -n)
        };
        Target::Lcc {
            a: r,
            k0: 1.0,
            lam0: lon0 * DEG_TO_RAD,
            n,
            c,
            rho0,
        }
    }
    fn fwd(&self, lam_in: f64, phi_in: f64) -> (f64, f64) {
        match *self {
            Target::Lcc {
                a,
                k0,
                lam0,
                n,
                c,
                rho0,
            } => {
                let (lam, phi) = fwd_prepare(lam_in, phi_in, lam0);
                let rho = c * m::pow(m::tan(FORTPI + 0.5 * phi), -n);
                let lam = lam * n;
                let x = k0 * (rho * m::sin(lam));
                let y = k0 * (rho0 - rho * m::cos(lam));
                (1.0 * (x * a + 0.0), 1.0 * (y * a + 0.0))
            }
            Target::Stere {
                a,
                lam0,
                akm1,
                phits_ok: _,
            } => {
                let (lam, phi) = fwd_prepare(lam_in, phi_in, lam0);
                let sinlam = m::sin(lam);
                let coslam = -m::cos(lam);
                let phi = -phi;
                let yy = akm1 * m::tan(FORTPI + 0.5 * phi);
                let x = sinlam * yy;
                let y = yy * coslam;
                (1.0 * (x * a + 0.0), 1.0 * (y * a + 0.0))
            }
            Target::LonLat { factor } => (lam_in * factor, phi_in * factor),
        }
    }
}

// ---- GDAL gdaltransformer.cpp GDALGenImgProjTransform (bDstToSrc=TRUE) with OGRProjCT
struct Xf {
    aea: Aea,
    tgt: Target,
    dst_gt: [f64; 6],
    src_inv: [f64; 6],
    max_err: f64,
    center_skip: bool,
}
impl Xf {
    fn base(&self, x: &mut [f64], y: &mut [f64]) {
        for i in 0..x.len() {
            let gx = self.dst_gt[0] + x[i] * self.dst_gt[1] + y[i] * self.dst_gt[2];
            let gy = self.dst_gt[3] + x[i] * self.dst_gt[4] + y[i] * self.dst_gt[5];
            let (lam, phi) = self.aea.inv(gx, gy);
            let (ox, oy) = self.tgt.fwd(lam, phi);
            x[i] = self.src_inv[0] + ox * self.src_inv[1] + oy * self.src_inv[2];
            y[i] = self.src_inv[3] + ox * self.src_inv[4] + oy * self.src_inv[5];
        }
    }
    // GDALApproxTransform
    fn approx(&self, x: &mut [f64], y: &mut [f64]) {
        let n = x.len();
        let mid = (n - 1) / 2;
        if y[0] != y[n - 1]
            || y[0] != y[mid]
            || x[0] == x[n - 1]
            || x[0] == x[mid]
            || n <= 5
            || self.max_err == 0.0
        {
            self.base(x, y);
            return;
        }
        let mut x2 = [x[0], x[mid], x[n - 1]];
        let mut y2 = [y[0], y[mid], y[n - 1]];
        self.base(&mut x2, &mut y2);
        self.approx_internal(x, y, x2, y2);
    }
    fn approx_internal(&self, x: &mut [f64], y: &mut [f64], xs: [f64; 3], ys: [f64; 3]) {
        let MAXERR: f64 = self.max_err;
        let n = x.len();
        let mid = (n - 1) / 2;
        let ddx = (xs[2] - xs[0]) / (x[n - 1] - x[0]);
        let ddy = (ys[2] - ys[0]) / (x[n - 1] - x[0]);
        let err = ((xs[0] + ddx * (x[mid] - x[0])) - xs[1]).abs()
            + ((ys[0] + ddy * (x[mid] - x[0])) - ys[1]).abs();
        if err > MAXERR {
            let mut xm = [x[(mid - 1) / 2], x[mid - 1], x[mid + (n - mid - 1) / 2]];
            let mut ym = [y[(mid - 1) / 2], y[mid - 1], y[mid + (n - mid - 1) / 2]];
            let base1 = mid <= 5
                || y[0] != y[mid - 1]
                || y[0] != y[(mid - 1) / 2]
                || x[0] == x[mid - 1]
                || x[0] == x[(mid - 1) / 2];
            let base2 = n - mid <= 5
                || y[mid] != y[n - 1]
                || y[mid] != y[mid + (n - mid - 1) / 2]
                || x[mid] == x[n - 1]
                || x[mid] == x[mid + (n - mid - 1) / 2];
            if !base1 && !base2 {
                self.base(&mut xm, &mut ym);
            } else if !base1 {
                self.base(&mut xm[..2], &mut ym[..2]);
            } else if !base2 {
                self.base(&mut xm[2..3], &mut ym[2..3]);
            } else {
                // bSuccess stays FALSE: both halves through the base transformer
                self.base(&mut x[1..mid], &mut y[1..mid]);
                self.base(&mut x[mid + 1..n - 1], &mut y[mid + 1..n - 1]);
                x[0] = xs[0];
                y[0] = ys[0];
                x[mid] = xs[1];
                y[mid] = ys[1];
                x[n - 1] = xs[2];
                y[n - 1] = ys[2];
                return;
            }
            if !base1 {
                let (xa, _) = x.split_at_mut(mid);
                let (ya, _) = y.split_at_mut(mid);
                self.approx_internal(xa, ya, [xs[0], xm[0], xm[1]], [ys[0], ym[0], ym[1]]);
            } else {
                self.base(&mut x[1..mid], &mut y[1..mid]);
                x[0] = xs[0];
                y[0] = ys[0];
            }
            if !base2 {
                self.approx_internal(
                    &mut x[mid..],
                    &mut y[mid..],
                    [xs[1], xm[2], xs[2]],
                    [ys[1], ym[2], ys[2]],
                );
            } else {
                self.base(&mut x[mid + 1..n - 1], &mut y[mid + 1..n - 1]);
                x[mid] = xs[1];
                y[mid] = ys[1];
                x[n - 1] = xs[2];
                y[n - 1] = ys[2];
            }
            return;
        }
        for i in (0..n).rev() {
            let dist = x[i] - x[0];
            x[i] = xs[0] + ddx * dist;
            y[i] = ys[0] + ddy * dist;
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Alg {
    Near,
    Bilinear,
    Average,
}
impl Alg {
    fn radius(self) -> i32 {
        match self {
            Alg::Near => 0,
            Alg::Bilinear => 1,
            Alg::Average => 0,
        }
    }
}

struct Src {
    nx: i32,
    ny: i32,
    data: Vec<f32>,
}

#[derive(Clone, Copy)]
struct Chunk {
    dx: i32,
    dy: i32,
    dsx: i32,
    dsy: i32,
    sx: i32,
    sy: i32,
    ssx: i32,
    ssy: i32,
    ex: f64,
    ey: f64,
}

// gdalwarpoperation.cpp ComputeSourceWindow (edge sampling path)
#[allow(clippy::too_many_arguments)]
fn compute_source_window(
    xf: &Xf,
    s: &Src,
    alg: Alg,
    dx: i32,
    dy: i32,
    dsx: i32,
    dsy: i32,
) -> (i32, i32, i32, i32, f64, f64, f64) {
    let step_count = 21;
    let step = 1.0 / (step_count as f64 - 1.0);
    let mut px = Vec::with_capacity(84);
    let mut py = Vec::with_capacity(84);
    let mut ratio = 0.0f64;
    while ratio <= 1.0 + step * 0.5 {
        px.push(ratio * dsx as f64 + dx as f64);
        py.push(dy as f64);
        px.push(ratio * dsx as f64 + dx as f64);
        py.push((dy + dsy) as f64);
        px.push(dx as f64);
        py.push(ratio * dsy as f64 + dy as f64);
        px.push((dsx + dx) as f64);
        py.push(ratio * dsy as f64 + dy as f64);
        ratio += step;
    }
    xf.approx(&mut px, &mut py); // y[0] != y[last] so this is the base transformer
    let (mut minx, mut miny, mut maxx, mut maxy) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for i in 0..px.len() {
        if px[i].is_nan() || py[i].is_nan() {
            continue;
        }
        minx = if px[i] < minx { px[i] } else { minx };
        miny = if py[i] < miny { py[i] } else { miny };
        maxx = if maxx < px[i] { px[i] } else { maxx };
        maxy = if maxy < py[i] { py[i] } else { maxy };
    }
    let (rx, ry) = (s.nx, s.ny);
    if minx > rx as f64 || maxx < 0.0 || miny > ry as f64 || maxy < 0.0 {
        return (0, 0, 0, 0, 0.0, 0.0, 0.0);
    }
    let res = alg.radius();
    let xscale = f64::max(1e-3, dsx as f64 / (maxx - minx));
    let yscale = f64::max(1e-3, dsy as f64 / (maxy - miny));
    let xrad = if xscale < 0.95 {
        (res as f64 / xscale).ceil() as i32
    } else {
        res
    };
    let yrad = if yscale < 0.95 {
        (res as f64 / yscale).ceil() as i32
    } else {
        res
    };
    let minxc = f64::max(0.0, minx) as i32;
    let minyc = f64::max(0.0, miny) as i32;
    let maxxc = f64::min(maxx.ceil(), rx as f64) as i32;
    let maxyc = f64::min(maxy.ceil(), ry as f64) as i32;
    let sxraw = f64::max(0.0, f64::min((rx - minxc) as f64, maxx - minx));
    let syraw = f64::max(0.0, f64::min((ry - minyc) as f64, maxy - miny));
    let (sx, ssx);
    if (maxxc - minxc) as f64 > 0.9 * rx as f64 {
        sx = 0;
        ssx = rx;
    } else {
        sx = (minxc - xrad).min(rx).max(0);
        ssx = (rx - sx).min(maxxc - sx + xrad).max(0);
    }
    let (sy, ssy);
    if (maxyc - minyc) as f64 > 0.9 * ry as f64 {
        sy = 0;
        ssy = ry;
    } else {
        sy = (minyc - yrad).min(ry).max(0);
        ssy = (ry - sy).min(maxyc - sy + yrad).max(0);
    }
    let fill = ssx as f64 * ssy as f64
        / f64::max(
            1.0,
            (maxx - minx + 2.0 * xrad as f64) * (maxy - miny + 2.0 * yrad as f64),
        );
    (
        sx,
        sy,
        ssx,
        ssy,
        ssx as f64 - sxraw,
        ssy as f64 - syraw,
        fill,
    )
}

// gdalwarpoperation.cpp CollectChunkListInternal
fn collect(xf: &Xf, s: &Src, alg: Alg, dx: i32, dy: i32, dsx: i32, dsy: i32, out: &mut Vec<Chunk>) {
    let (sx, sy, ssx, ssy, ex, ey, fill) = compute_source_window(xf, s, alg, dx, dy, dsx, dsy);
    // Float32 + one band-mask bit on both sides (padfSrcNoDataReal, padfDstNoDataReal)
    let total = (33.0 * ssx as f64 * ssy as f64 + 33.0 * dsx as f64 * dsy as f64) / 8.0;
    let limit = 64.0 * 1024.0 * 1024.0;
    if (total > limit && (dsx > 2 || dsy > 2))
        || (fill > 0.0 && fill < 0.5 && (dsx > 100 || dsy > 100))
    {
        if dsx > dsy {
            let c1 = dsx / 2;
            collect(xf, s, alg, dx, dy, c1, dsy, out);
            collect(xf, s, alg, dx + c1, dy, dsx - c1, dsy, out);
        } else {
            let c1 = dsy / 2;
            collect(xf, s, alg, dx, dy, dsx, c1, out);
            collect(xf, s, alg, dx, dy + c1, dsx, dsy - c1, out);
        }
        return;
    }
    out.push(Chunk {
        dx,
        dy,
        dsx,
        dsy,
        sx,
        sy,
        ssx,
        ssy,
        ex,
        ey,
    });
}

struct Wk<'a> {
    src: Vec<f32>,
    valid: Option<Vec<bool>>,
    nsx: i32,
    nsy: i32,
    xoff: i32,
    yoff: i32,
    alg: Alg,
    xscale: f64,
    yscale: f64,
    xrad: i32,
    yrad: i32,
    filt_x: i32,
    filt_y: i32,
    xf: &'a Xf,
}
impl Wk<'_> {
    #[inline]
    fn is_valid(&self, off: i64) -> bool {
        match &self.valid {
            None => true,
            // Mask arrays carry WARP_EXTRA_ELTS trailing bits, created set (CreateKernelMask).
            Some(v) => {
                if off < 0 || off as usize >= v.len() {
                    true
                } else {
                    v[off as usize]
                }
            }
        }
    }
    #[inline]
    fn val(&self, off: i64) -> f64 {
        if off < 0 || off as usize >= self.src.len() {
            0.0
        } else {
            self.src[off as usize] as f64
        }
    }
    // GWKGetPixelRow
    fn row(&self, off: i64, half: i32, dens: &mut [f64], real: &mut [f64]) -> bool {
        let len = half * 2;
        let mut has = false;
        if self.valid.is_some() {
            for i in 0..len as usize {
                dens[i] = 1.0;
            }
            for i in 0..len as usize {
                if self.is_valid(off + i as i64) {
                    has = true;
                } else {
                    dens[i] = 0.0;
                }
            }
            if !has {
                return false;
            }
        }
        for i in 0..len as usize {
            real[i] = self.val(off + i as i64);
        }
        if self.valid.is_none() {
            return true;
        }
        has = false;
        for i in 0..len as usize {
            if dens[i] > 0.000000001f32 as f64 {
                dens[i] = 1.0;
                has = true;
            }
        }
        has
    }
    // GWKBilinearResample4Sample. The kernel always passes a density array here.
    fn bilinear4(&self, srcx: f64, srcy: f64) -> (f64, f64) {
        let (nsx, nsy) = (self.nsx, self.nsy);
        let mut isx = (srcx - 0.5).floor() as i32;
        let mut isy = (srcy - 0.5).floor() as i32;
        let mut rx = 1.5 - (srcx - isx as f64);
        let mut ry = 1.5 - (srcy - isy as f64);
        let mut shifted = false;
        if isx == -1 {
            isx = 0;
            rx = 1.0;
        }
        if isy == -1 {
            isy = 0;
            ry = 1.0;
        }
        let mut off = isx as i64 + isy as i64 * nsx as i64;
        let npx = nsx as i64 * nsy as i64;
        if npx == off + 1 || npx == off + nsx as i64 + 1 {
            shifted = true;
            off -= 1;
        }
        let (mut d, mut r) = ([0.0f64; 2], [0.0f64; 2]);
        let (mut ar, mut ad, mut adv) = (0.0f64, 0.0f64, 0.0f64);
        let thr = 0.000000001f32 as f64;
        // GWKGetPixelRow with a density array always fills it, also without a mask.
        let row2 = |o: i64, d: &mut [f64; 2], r: &mut [f64; 2]| -> bool {
            d[0] = 1.0;
            d[1] = 1.0;
            let mut has = false;
            if self.valid.is_some() {
                if self.is_valid(o) {
                    has = true;
                } else {
                    d[0] = 0.0;
                }
                if self.is_valid(o + 1) {
                    has = true;
                } else {
                    d[1] = 0.0;
                }
                if !has {
                    return false;
                }
            }
            r[0] = self.val(o);
            r[1] = self.val(o + 1);
            has = false;
            if d[0] > thr {
                d[0] = 1.0;
                has = true;
            }
            if d[1] > thr {
                d[1] = 1.0;
                has = true;
            }
            has
        };
        if isy >= 0 && isy < nsy && off >= 0 && off < npx && row2(off, &mut d, &mut r) {
            let m1 = rx * ry;
            let m2 = (1.0 - rx) * ry;
            if shifted {
                r[0] = r[1];
                d[0] = d[1];
            }
            if isx >= 0 && isx < nsx && d[0] > thr {
                adv += m1;
                ar += r[0] * m1;
                ad += d[0] * m1;
            }
            if isx + 1 >= 0 && isx + 1 < nsx && d[1] > thr {
                adv += m2;
                ar += r[1] * m2;
                ad += d[1] * m2;
            }
        }
        if isy + 1 >= 0
            && isy + 1 < nsy
            && off + (nsx as i64) >= 0
            && off + (nsx as i64) < npx
            && row2(off + nsx as i64, &mut d, &mut r)
        {
            let m1 = rx * (1.0 - ry);
            let m2 = (1.0 - rx) * (1.0 - ry);
            if shifted {
                r[0] = r[1];
                d[0] = d[1];
            }
            if isx >= 0 && isx < nsx && d[0] > thr {
                adv += m1;
                ar += r[0] * m1;
                ad += d[0] * m1;
            }
            if isx + 1 >= 0 && isx + 1 < nsx && d[1] > thr {
                adv += m2;
                ar += r[1] * m2;
                ad += d[1] * m2;
            }
        }
        if adv == 1.0 {
            (ar, ad)
        } else if adv < 0.00001 {
            (0.0, 0.0)
        } else {
            (ar / adv, ad / adv)
        }
    }
    // GWKResample with GWKBilinear weights; used when either scale is below 0.95.
    fn resample(
        &self,
        srcx: f64,
        srcy: f64,
        wx: &mut [f64],
        calc: &mut [bool],
        rd: &mut [f64],
        rr: &mut [f64],
    ) -> (f64, f64) {
        let (nsx, nsy) = (self.nsx, self.nsy);
        let (mut acc_r, mut acc_d, mut acc_w) = (0.0f64, 0.0f64, 0.0f64);
        let isx = (srcx - 0.5).floor() as i32;
        let isy = (srcy - 0.5).floor() as i32;
        let off = isx as i64 + isy as i64 * nsx as i64;
        let dx = srcx - 0.5 - isx as f64;
        let dy = srcy - 0.5 - isy as f64;
        for c in calc.iter_mut() {
            *c = false;
        }
        let bil = |v: f64| -> f64 {
            let a = v.abs();
            if a <= 1.0 {
                1.0 - a
            } else {
                0.0
            }
        };
        let mut j = self.filt_y;
        let mut jmax = self.yrad;
        if isy + j < 0 {
            j = -isy;
        }
        if isy + jmax >= nsy {
            jmax = nsy - isy - 1;
        }
        let mut imin = self.filt_x;
        let mut imax = self.xrad;
        if isx + imin < 0 {
            imin = -isx;
        }
        if isx + imax >= nsx {
            imax = nsx - isx - 1;
        }
        let xb = self.xscale < 1.0;
        let yb = self.yscale < 1.0;
        let mut roff = off + (j as i64 - 1) * nsx as i64 + imin as i64;
        let has_d = self.valid.is_some();
        while j <= jmax {
            roff += nsx as i64;
            if !self.row(roff, (imax - imin + 2) / 2, rd, rr) {
                j += 1;
                continue;
            }
            let w1 = if yb {
                bil((j as f64 - dy) * self.yscale)
            } else {
                bil(j as f64 - dy)
            };
            let (mut lr, mut ld, mut lw) = (0.0f64, 0.0f64, 0.0f64);
            let mut i = imin;
            while i <= imax {
                let k = (i - imin) as usize;
                if has_d && rd[k] < 0.000000001f32 as f64 {
                    i += 1;
                    continue;
                }
                let w2 = if calc[k] {
                    wx[k]
                } else {
                    let w = if xb {
                        bil((i as f64 - dx) * self.xscale)
                    } else {
                        bil(i as f64 - dx)
                    };
                    wx[k] = w;
                    calc[k] = true;
                    w
                };
                lr += rr[k] * w2;
                if has_d {
                    ld += rd[k] * w2;
                }
                lw += w2;
                i += 1;
            }
            acc_r += lr * w1;
            acc_d += ld * w1;
            acc_w += lw * w1;
            j += 1;
        }
        if acc_w < 0.000001 || (has_d && acc_d < 0.000001) {
            return (0.0, 0.0);
        }
        if acc_w < 0.99999 || acc_w > 1.00001 {
            (acc_r / acc_w, if has_d { acc_d / acc_w } else { 1.0 })
        } else {
            (acc_r, if has_d { acc_d } else { 1.0 })
        }
    }
    // GWKCheckAndComputeSrcOffsets
    #[inline]
    fn offset(&self, x: f64, y: f64) -> Option<i64> {
        if x.is_nan() || y.is_nan() {
            return None;
        }
        if x < self.xoff as f64 || y < self.yoff as f64 {
            return None;
        }
        if x + 1e-10 > (self.nsx + self.xoff) as f64 || y + 1e-10 > (self.nsy + self.yoff) as f64 {
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
        Some(ix as i64 + iy as i64 * self.nsx as i64)
    }
}

fn set_pixel(dst: &mut [f32], valid: &mut [bool], off: usize, density: f64, real: f64) -> bool {
    // GWKSetPixelValueReal with panDstValid set; dst density stays null.
    let mut v = real;
    if density < 0.9999 {
        if density < 0.0001 {
            return true;
        }
        let mut dst_density = 1.0;
        if !valid[off] {
            dst_density = 0.0;
        }
        let dst_real = dst[off] as f64;
        let infl = (1.0 - density) * dst_density;
        v = (v * density + dst_real * infl) / (density + infl);
    }
    dst[off] = v as f32;
    true
}

#[allow(clippy::too_many_arguments)]
fn warp_chunk(xf: &Xf, s: &Src, alg: Alg, c: &Chunk, nodata: f64, out: &mut [f32], full_nx: usize) {
    if c.ssx <= 0 || c.ssy <= 0 {
        return;
    }
    // WarpRegionToBuffer: source subwindow, then GDALWarpNoDataMasker
    let mut src = Vec::with_capacity((c.ssx as usize) * (c.ssy as usize));
    for r in 0..c.ssy {
        let b = ((c.sy + r) as usize) * (s.nx as usize) + c.sx as usize;
        src.extend_from_slice(&s.data[b..b + c.ssx as usize]);
    }
    let nd = nodata as f32;
    let nd_nan = nd.is_nan();
    let mut valid = vec![true; src.len()];
    let mut all = true;
    for i in 0..src.len() {
        let v = src[i];
        // gdal_priv.h ARE_REAL_EQUAL: ulp=2, float epsilon
        let eq = if nd_nan {
            v.is_nan()
        } else {
            v == nd || (v - nd).abs() < f32::EPSILON * (v + nd).abs() * 2.0
        };
        if eq {
            valid[i] = false;
            all = false;
        }
    }
    let valid = if all { None } else { Some(valid) };
    // PerformWarp scale computation
    let (nsx, nsy, ndx, ndy) = (c.ssx, c.ssy, c.dsx, c.dsy);
    let mut xs = ndx as f64 / (nsx as f64 - c.ex);
    let mut ys = ndy as f64 / (nsy as f64 - c.ey);
    if nsx >= ndx && nsx as f64 <= ndx as f64 + c.ex {
        xs = 1.0;
    }
    if nsy >= ndy && nsy as f64 <= ndy as f64 + c.ey {
        ys = 1.0;
    }
    if xs < 1.0 {
        let rc = 1.0 / xs;
        let n = (rc + 0.5) as i32;
        if (rc - n as f64).abs() < 0.05 {
            xs = 1.0 / n as f64;
        }
    }
    if ys < 1.0 {
        let rc = 1.0 / ys;
        let n = (rc + 0.5) as i32;
        if (rc - n as f64).abs() < 0.05 {
            ys = 1.0 / n as f64;
        }
    }
    if ys / xs > 100.0 {
        panic!("FROM_GRID_SAMPLING branch not ported");
    }
    let four = xs >= 0.95 && ys >= 0.95;
    let filt = alg.radius();
    let xrad = if xs < 1.0 {
        (filt as f64 / xs).ceil() as i32
    } else {
        filt
    };
    let yrad = if ys < 1.0 {
        (filt as f64 / ys).ceil() as i32
    } else {
        filt
    };
    let wk = Wk {
        src,
        valid,
        nsx,
        nsy,
        xoff: c.sx,
        yoff: c.sy,
        alg,
        xscale: xs,
        yscale: ys,
        xrad,
        yrad,
        filt_x: ((filt + 1) % 2) - xrad,
        filt_y: ((filt + 1) % 2) - yrad,
        xf,
    };
    let _ = wk.alg;
    let ndxu = ndx as usize;
    let mut dst = vec![nd; ndxu * ndy as usize];
    let mut dvalid = vec![false; dst.len()];
    let nxd = ((wk.xrad + 1) * 2) as usize + 2;
    let (mut wx, mut calc, mut rd, mut rr) = (
        vec![0.0; nxd],
        vec![false; nxd],
        vec![0.0; nxd],
        vec![0.0; nxd],
    );
    let mut x = vec![0.0f64; ndxu];
    let mut y = vec![0.0f64; ndxu];
    let mut x2 = vec![0.0f64; ndxu];
    let mut y2 = vec![0.0f64; ndxu];
    for idy in 0..ndy {
        if alg == Alg::Average {
            // GWKAverageOrModeThread
            for i in 0..ndxu {
                x[i] = (i as i32 + c.dx) as f64;
                y[i] = (idy + c.dy) as f64;
                x2[i] = i as f64 + 1.0 + c.dx as f64;
                y2[i] = idy as f64 + 1.0 + c.dy as f64;
            }
            xf.approx(&mut x, &mut y);
            xf.approx(&mut x2, &mut y2);
            for idx in 0..ndxu {
                if x[idx] < (c.sx - 1) as f64
                    || x2[idx] < (c.sx - 1) as f64
                    || y[idx] < (c.sy - 1) as f64
                    || y2[idx] < (c.sy - 1) as f64
                    || x[idx] > (nsx + c.sx + 1) as f64
                    || x2[idx] > (nsx + c.sx + 1) as f64
                    || y[idx] > (nsy + c.sy + 1) as f64
                    || y2[idx] > (nsy + c.sy + 1) as f64
                {
                    continue;
                }
                let dst_off = idx + idy as usize * ndxu;
                let xmin = f64::min(x[idx], x2[idx]) - c.sx as f64;
                let mut isxmin = i32::max((xmin + 1e-10).floor() as i32, 0);
                let xmax = f64::max(x[idx], x2[idx]) - c.sx as f64;
                let mut isxmax = i32::min((xmax - 1e-10).ceil() as i32, nsx);
                let ymin = f64::min(y[idx], y2[idx]) - c.sy as f64;
                let mut isymin = i32::max((ymin + 1e-10).floor() as i32, 0);
                let ymax = f64::max(y[idx], y2[idx]) - c.sy as f64;
                let mut isymax = i32::min((ymax - 1e-10).ceil() as i32, nsy);
                if isxmin == isxmax && isxmax < nsx {
                    isxmax += 1;
                }
                if isymin == isymax && isymax < nsy {
                    isymax += 1;
                }
                let _ = (&mut isxmin, &mut isymin);
                let (mut tr, mut tw) = (0.0f64, 0.0f64);
                let mut sy = isymin;
                while sy < isymax {
                    let wy = if sy == isymin {
                        if isymin + 1 == isymax {
                            1.0
                        } else {
                            1.0 - (ymin - isymin as f64)
                        }
                    } else if sy + 1 == isymax {
                        1.0 - (isymax as f64 - ymax)
                    } else {
                        1.0
                    };
                    let mut off = isxmin as i64 + sy as i64 * nsx as i64;
                    let mut sx = isxmin;
                    while sx < isxmax {
                        if wk.is_valid(off) {
                            let w = if sx == isxmin {
                                if isxmin + 1 == isxmax {
                                    wy
                                } else {
                                    wy * (1.0 - (xmin - isxmin as f64))
                                }
                            } else if sx + 1 == isxmax {
                                wy * (1.0 - (isxmax as f64 - xmax))
                            } else {
                                wy
                            };
                            tw += w;
                            tr += wk.val(off) * w;
                        }
                        sx += 1;
                        off += 1;
                    }
                    sy += 1;
                }
                if tw > 0.0 {
                    set_pixel(&mut dst, &mut dvalid, dst_off, 1.0, tr / tw);
                    dvalid[dst_off] = true;
                }
            }
            continue;
        }
        for i in 0..ndxu {
            x[i] = i as f64 + 0.5 + c.dx as f64;
            y[i] = idy as f64 + 0.5 + c.dy as f64;
        }
        xf.approx(&mut x, &mut y);
        for idx in 0..ndxu {
            let off = match wk.offset(x[idx], y[idx]) {
                Some(o) => o,
                None => continue,
            };
            if xf.center_skip && !wk.is_valid(off) {
                continue;
            }
            let dst_off = idx + idy as usize * ndxu;
            if alg == Alg::Near {
                // GWKNearestThread: copies the source float
                dst[dst_off] = wk.src[off as usize];
                dvalid[dst_off] = true;
                continue;
            }
            let (val, dens) = if nsx == 1 || nsy == 1 {
                (wk.val(off), 1.0)
            } else if four {
                wk.bilinear4(x[idx] - c.sx as f64, y[idx] - c.sy as f64)
            } else {
                wk.resample(
                    x[idx] - c.sx as f64,
                    y[idx] - c.sy as f64,
                    &mut wx,
                    &mut calc,
                    &mut rd,
                    &mut rr,
                )
            };
            if dens < 0.0000000001 {
                continue;
            }
            set_pixel(&mut dst, &mut dvalid, dst_off, dens, val);
            dvalid[dst_off] = true;
        }
    }
    for r in 0..ndy as usize {
        let b = (c.dy as usize + r) * full_nx + c.dx as usize;
        out[b..b + ndxu].copy_from_slice(&dst[r * ndxu..(r + 1) * ndxu]);
    }
}

/// The warp output, and the count and digest of its maths calls.
pub fn run(case: &Case, data: &[f32]) -> (Vec<f32>, u64, u64) {
    m::STREAM.with(|s| s.set(0xcbf2_9ce4_8422_2325));
    m::CALLS.with(|c| c.set(0));
    let s = Src {
        nx: case.nx,
        ny: case.ny,
        data: data.to_vec(),
    };
    let src_gt = [case.ox, case.dx, 0.0, case.oy, 0.0, case.dy];
    // GDALInvGeoTransform, no-rotation special case
    let src_inv = [
        -src_gt[0] / src_gt[1],
        1.0 / src_gt[1],
        0.0,
        -src_gt[3] / src_gt[5],
        0.0,
        1.0 / src_gt[5],
    ];
    let [minx, miny, maxx, maxy] = case.te;
    let res = case.tr;
    // gdalwarp_lib.cpp GDALWarpCreateOutput
    let onx = ((maxx - minx + res / 2.0) / res) as i32;
    let ony = (((maxy - miny).abs() + res / 2.0) / res) as i32;
    let dst_gt = [minx, res, 0.0, maxy, 0.0, -res];
    let tgt = match case.proj {
        "lcc" => Target::lcc(case.lat1, case.lat2, case.lat0, case.lon0, case.r),
        "stere" => Target::Stere {
            a: case.r,
            lam0: case.lon0 * DEG_TO_RAD,
            akm1: 2.0 * case.k,
            phits_ok: true,
        },
        "lonlat" => Target::LonLat {
            factor: 1.0 / DEG_TO_RAD,
        },
        p => panic!("{p}"),
    };
    let xf = Xf {
        aea: Aea::new(29.5, 45.5, 23.0, -96.0),
        tgt,
        dst_gt,
        src_inv,
        max_err: 0.125,
        center_skip: true,
    };
    let alg = match case.method {
        "near" => Alg::Near,
        "bilinear" => Alg::Bilinear,
        "average" => Alg::Average,
        m => panic!("{m}"),
    };
    let nodata = case.nodata;
    let mut out = vec![nodata as f32; onx as usize * ony as usize];
    let mut chunks = Vec::new();
    collect(&xf, &s, alg, 0, 0, onx, ony, &mut chunks);
    chunks.sort_by_key(|c| (c.dy, c.dx));
    for c in &chunks {
        warp_chunk(&xf, &s, alg, c, nodata, &mut out, onx as usize);
    }
    (out, m::CALLS.with(Cell::get), m::STREAM.with(Cell::get))
}

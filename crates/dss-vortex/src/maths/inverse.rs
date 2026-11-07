#![allow(clippy::excessive_precision)]
//! `asin`, `atan`, `atan2`, `hypot`. Ported from AOCL-LibM
//! `src/optimized/{asin,atan,atan2,hypot}.c`. Those are C, and the reference matches them with
//! their multiply-adds contracted into fused steps, which is what is written here. `hypot` keeps
//! the source's scaling and drops its head-and-tail correction. These forms were identified by
//! black-box comparison; see the P44a report. Only `f64::sqrt`, exactly rounded by IEEE 754, is
//! taken from `std`.

use super::fma;
use super::tables::{ATAN_JBY256_HEAD, ATAN_JBY256_TAIL};

const fn d(bits: u64) -> f64 {
    f64::from_bits(bits)
}

const SIGN: u64 = 0x8000_0000_0000_0000;
const INF: u64 = 0x7ff0_0000_0000_0000;
const EXPBITS: u64 = INF;
const QNAN_BIT: u64 = 0x0008_0000_0000_0000;
const PIBY2: f64 = d(0x3ff921fb54442d18);
const NEG_NAN: f64 = d(0xfff8_0000_0000_0000);

/// Arc sine.
pub fn asin(x: f64) -> f64 {
    let ux = x.to_bits();
    let aux = ux & !SIGN;
    let xneg = ux & SIGN != 0;
    let xexp = ((ux & EXPBITS) >> 52) as i32 - 1023;
    if aux > INF {
        return d(ux | QNAN_BIT);
    }
    if xexp < -28 {
        return x;
    }
    if xexp >= 0 {
        return if x == 1.0 {
            PIBY2
        } else if x == -1.0 {
            -PIBY2
        } else {
            NEG_NAN
        };
    }
    let mut y = if xneg { -x } else { x };
    let transform = xexp >= -1;
    let mut s = 0.0;
    let r;
    if transform {
        r = 0.5 * (1.0 - y);
        s = r.sqrt();
        y = s;
    } else {
        r = y * y;
    }
    let mut num = fma(
        0.0000482901920344786991880522822991,
        r,
        0.00109242697235074662306043804220,
    );
    num = fma(num, r, -0.0549989809235685841612020091328);
    num = fma(num, r, 0.275558175256937652532686256258);
    num = fma(num, r, -0.445017216867635649900123110649);
    num = fma(num, r, 0.227485835556935010735943483075);
    let mut den = fma(
        0.105869422087204370341222318533,
        r,
        -0.943639137032492685763471240072,
    );
    den = fma(den, r, 2.76568859157270989520376345954);
    den = fma(den, r, -3.28431505720958658909889444194);
    den = fma(den, r, 1.36491501334161032038194214209);
    let u = (r * num) / den;
    let v = if transform {
        let s1 = d(s.to_bits() & 0xffff_ffff_0000_0000);
        let c = (r - s1 * s1) / (s + s1);
        let p = fma(2.0 * s, u, -(d(0x3c91a62633145c07) - 2.0 * c));
        let q = d(0x3fe921fb54442d18) - 2.0 * s1;
        d(0x3fe921fb54442d18) - (p - q)
    } else {
        fma(y, u, y)
    };
    if xneg {
        -v
    } else {
        v
    }
}

/// Arc tangent.
pub fn atan(x_in: f64) -> f64 {
    let ux = x_in.to_bits();
    let aux = ux & !SIGN;
    let xneg = ux != aux;
    let v = if xneg { -x_in } else { x_in };
    let (x, chi, clo);
    if aux < 0x3e30_0000_0000_0000 {
        return x_in;
    } else if aux > 0x4003_8000_0000_0000 {
        if aux > INF {
            return d(ux | QNAN_BIT);
        } else if aux > 0x4370_0000_0000_0000 {
            return if xneg { -PIBY2 } else { PIBY2 };
        }
        x = -1.0 / v;
        chi = d(0x3ff921fb54442d18);
        clo = d(0x3c91a62633145c06);
    } else if aux > 0x3ff3_0000_0000_0000 {
        x = (v - 1.5) / fma(1.5, v, 1.0);
        chi = d(0x3fef730bd281f69b);
        clo = d(0x3c7007887af0cbbc);
    } else if aux > 0x3fe6_0000_0000_0000 {
        x = (v - 1.0) / (1.0 + v);
        chi = d(0x3fe921fb54442d18);
        clo = d(0x3c81a62633145c06);
    } else if aux > 0x3fdc_0000_0000_0000 {
        x = fma(2.0, v, -1.0) / (2.0 + v);
        chi = d(0x3fddac670561bb4f);
        clo = d(0x3c7a2b7f222f65e0);
    } else {
        x = v;
        chi = 0.0;
        clo = 0.0;
    }
    let s = x * x;
    let mut num = fma(0.142316903342317766e-3, s, 0.304455919504853031e-1);
    num = fma(num, s, 0.220638780716667420e0);
    num = fma(num, s, 0.447677206805497472e0);
    num = fma(num, s, 0.268297920532545909e0);
    let mut den = fma(0.389525873944742195e-1, s, 0.424602594203847109e0);
    den = fma(den, s, 0.141254259931958921e1);
    den = fma(den, s, 0.182596787737507063e1);
    den = fma(den, s, 0.804893761597637733e0);
    let q = (x * s * num) / den;
    let z = chi - ((q - clo) - x);
    if xneg {
        -z
    } else {
        z
    }
}

/// `scaleDouble_2` from `libm_inlines_amd.h`, for the range the callers reach.
#[inline(always)]
fn scale2(x: f64, n: i32) -> f64 {
    let n1 = n / 2;
    let n2 = n - n1;
    let t1 = d(((i64::from(n1) + 1023) as u64) << 52);
    let t2 = d(((i64::from(n2) + 1023) as u64) << 52);
    (x * t1) * t2
}

/// `scaleUpDouble1024` from `libm_inlines_amd.h`.
#[inline(always)]
fn scale_up_1024(ux: u64) -> u64 {
    if ux & EXPBITS == 0 {
        let y = d(ux | 0x4010_0000_0000_0000);
        let y = if ux & SIGN != 0 { y + 4.0 } else { y - 4.0 };
        y.to_bits()
    } else {
        ux.wrapping_add(0x4000_0000_0000_0000)
    }
}

/// Arc tangent of `y / x`, by quadrant.
pub fn atan2(y_in: f64, x_in: f64) -> f64 {
    const PI: u64 = 0x4009_21fb_5444_2d18;
    const PI2: u64 = 0x3ff9_21fb_5444_2d18;
    const PI4: u64 = 0x3fe9_21fb_5444_2d18;
    const PI34: u64 = 0x4002_d97c_7f33_21d2;
    let (mut x, mut y) = (x_in, y_in);
    let mut ux = x.to_bits();
    let mut uy = y.to_bits();
    let aux = ux & !SIGN;
    let auy = uy & !SIGN;
    let mut xexp = ((ux & EXPBITS) >> 52) as i32;
    let mut yexp = ((uy & EXPBITS) >> 52) as i32;
    let xneg = ux & SIGN != 0;
    let yneg = uy & SIGN;
    let mut diffexp = yexp - xexp;

    if aux >= INF || auy >= INF {
        if aux == INF && auy == INF {
            return d(yneg | if xneg { PI34 } else { PI4 });
        }
        if aux > INF {
            return d(ux | QNAN_BIT);
        }
        if auy > INF {
            return d(uy | QNAN_BIT);
        }
        if ux == INF {
            return d(yneg);
        }
    } else if auy == 0 {
        return if xneg { d(yneg | PI) } else { y };
    } else if aux == 0 {
        return d(yneg | PI2);
    }

    if xexp < 1021 && yexp < 1021 {
        ux = scale_up_1024(ux);
        uy = scale_up_1024(uy);
        x = d(ux);
        y = d(uy);
        xexp = ((ux & EXPBITS) >> 52) as i32;
        yexp = ((uy & EXPBITS) >> 52) as i32;
        diffexp = yexp - xexp;
    }

    if diffexp > 56 {
        return d(yneg | PI2);
    } else if diffexp < -28 && !xneg {
        if diffexp < -1074 {
            return d(yneg);
        }
        return y / x;
    } else if diffexp < -56 && xneg {
        return d(yneg | PI);
    }

    let mut u = if xneg { -x } else { x };
    let mut v = if yneg != 0 { -y } else { y };
    let swap = u < v;
    if swap {
        core::mem::swap(&mut u, &mut v);
    }
    let vbyu = v / u;
    let mut q1;
    let mut q2;
    if vbyu > 0.0625 {
        let index = (256.0 * vbyu + 0.5) as u32;
        q1 = d(ATAN_JBY256_HEAD[index as usize - 16]);
        q2 = d(ATAN_JBY256_TAIL[index as usize - 16]);
        let c = f64::from(index) * 1.0 / 256.0;
        let m = ((u.to_bits() & EXPBITS) >> 52) as i32 - 1023;
        u = scale2(u, -m);
        v = scale2(v, -m);
        let u1 = d(u.to_bits() & 0xffff_ffff_f800_0000);
        let u2 = u - u1;
        let r = fma(-c, u2, fma(-c, u1, v)) / fma(c, v, u);
        let s = r * r;
        let cc = fma(-s, 0.19999918038989143496, 0.33333333333224095522);
        q2 = fma(-s, r * cc, q2 + r);
    } else if vbyu < 1.0e-8 {
        q1 = 0.0;
        q2 = vbyu;
    } else {
        let u1 = d(u.to_bits() & 0xffff_ffff_0000_0000);
        let vu1 = d(vbyu.to_bits() & 0xffff_ffff_0000_0000);
        let u2 = u - u1;
        let vu2 = vbyu - vu1;
        q1 = 0.0;
        let s = vbyu * vbyu;
        let mut p = fma(-s, 0.90029810285449784439E-01, 0.11110736283514525407);
        p = fma(-s, p, 0.14285713561807169030);
        p = fma(-s, p, 0.19999999999393223405);
        p = fma(-s, p, 0.33333333333333170500);
        let w = fma(-u, vu2, fma(-u2, vu1, fma(-u1, vu1, v))) / u;
        q2 = vbyu + fma(-(vbyu * s), p, w);
    }
    if swap {
        q1 = d(PI2) - q1;
        q2 = d(0x3c91a62633145c07) - q2;
    }
    if xneg {
        q1 = d(0x400921fb50000000) - q1;
        q2 = d(0x3e6110b4611a6263) - q2;
    }
    q1 += q2;
    d(yneg | q1.to_bits())
}

/// `sqrt(x*x + y*y)` without undue overflow or underflow.
pub fn hypot(x_in: f64, y_in: f64) -> f64 {
    const SIX_HUNDRED: u64 = 0x2580_0000_0000_0000;
    const U_FIX_BITS: u64 = 0x2590_0000_0000_0000;
    let ux = x_in.to_bits() & !SIGN;
    let uy = y_in.to_bits() & !SIGN;
    let xexp = ux >> 52;
    let yexp = uy >> 52;
    if ux >= INF || uy >= INF {
        if ux == INF || uy == INF {
            return d(INF);
        }
        return d(0x7ff8_0000_0000_0000);
    }
    let mut x = d(ux);
    let mut y = d(uy);
    let dexp = xexp as i32 - yexp as i32;
    let mut expadjust = 0i32;
    if ux == 0 {
        return y;
    } else if uy == 0 {
        return x;
    } else if !(-53..=53).contains(&dexp) {
        return x + y;
    } else if xexp > 1023 + 500 || yexp > 1023 + 500 {
        expadjust = 600;
        x = d(ux - SIX_HUNDRED);
        y = d(uy - SIX_HUNDRED);
    } else if xexp < 1023 - 500 || yexp < 1023 - 500 {
        expadjust = -600;
        if xexp == 0 {
            x = d(ux + U_FIX_BITS);
            x -= d(U_FIX_BITS);
        } else {
            x = d(ux + SIX_HUNDRED);
        }
        if yexp == 0 {
            y = d(uy + U_FIX_BITS);
            y -= d(U_FIX_BITS);
        } else {
            y = d(uy + SIX_HUNDRED);
        }
    }
    let retval = (x * x + y * y).sqrt();
    retval * d(((i64::from(expadjust) + 1023) as u64) << 52)
}

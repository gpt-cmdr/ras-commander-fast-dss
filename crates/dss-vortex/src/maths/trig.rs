//! `sin`, `cos`, `tan`. Ported from AOCL-LibM `src/isa/avx2/gas/{sin,cos,tan}_fma3.S` and the
//! macros in `src/isa/avx2/include/trig_func.h`. Each statement is one instruction or a short
//! run of them, in the source order.

use super::fma;
use super::tables::PIBITS;

const SIGN: u64 = 0x8000_0000_0000_0000;
const ABS: u64 = !SIGN;
const EXP: u64 = 0x7ff0_0000_0000_0000;
const PIBY4_BITS: u64 = 0x3fe9_21fb_5444_2d18; // .L_mask_3fe
const SMALL_3F2: u64 = 0x3f20_0000_0000_0000; // .L_mask_3f2
const SMALL_3E4: u64 = 0x3e40_0000_0000_0000; // .L_mask_3e4
const E_5: u64 = 0x4153_12d0_0000_0000; // .L_e_5, 5.0e6
const HALF_MASK: u64 = 0xffff_ffff_0000_0000;

const fn d(bits: u64) -> f64 {
    f64::from_bits(bits)
}

const C1: f64 = d(0x3fa5555555555555);
const C2: f64 = d(0xbf56c16c16c16967);
const C3: f64 = d(0x3efa01a019f4ec91);
const C4: f64 = d(0xbe927e4fa17f667b);
const C5: f64 = d(0x3e21eeb690382eec);
const C6: f64 = d(0xbda907db47258aa7);
const S1: f64 = d(0xbfc5555555555555);
const S2: f64 = d(0x3f81111111110bb3);
const S3: f64 = d(0xbf2a01a019e83e5c);
const S4: f64 = d(0x3ec71de3796cde01);
const S5: f64 = d(0xbe5ae600b42fdfa7);
const S6: f64 = d(0x3de5e0b2f9a43bb8);
const POINT_166: f64 = d(0x3fc5555555555555);
const TWOBYPI: f64 = d(0x3fe45f306dc9c883);
const PIBY2_1: f64 = d(0x3ff921fb54400000);
const PIBY2_1TAIL: f64 = d(0x3dd0b4611a626331);
const PIBY2_2: f64 = d(0x3dd0b4611a600000);
const PIBY2_2TAIL: f64 = d(0x3ba3198a2e037073);
const PIBY2_3: f64 = d(0x3ba3198a2e000000);
const PIBY2_3TAIL: f64 = d(0x397b839a252049c1);
const PIBY2_LEAD: f64 = d(0x3ff921fb54442d18);
const PIBY2_PART1: f64 = d(0x3ff921fb50000000);
const PIBY2_PART2: f64 = d(0x3e5110b460000000);
const PIBY2_PART3: f64 = d(0x3c91a62633145c06);
const POINT_68: f64 = d(0x3fe5c28f5c28f5c3);
const PIBY4_LEAD: f64 = d(0x3fe921fb54442d18);
const PIBY4_TAIL: f64 = d(0x3c81a62633145c06);
const TAN_C1: f64 = d(0x3fd7d50f6638564a);
const TAN_C2: f64 = d(0xbf977c24c7569abb);
const TAN_C3: f64 = d(0x3f2d5daf289c385a);
const TAN_C4: f64 = d(0x3ff1dfcb8caa40b8);
const TAN_C5: f64 = d(0xbfe08046499eb90f);
const TAN_C6: f64 = d(0x3f9ab0f4f80a0acf);
const TAN_C7: f64 = d(0x3f2e7517ef6d98f8);

/// `sin_piby4_s_xx_zero_fma3`.
#[inline(always)]
fn sin_piby4_zero(x: f64) -> f64 {
    let x2 = x * x;
    let mut p = fma(x2, S6, S5);
    p = fma(x2, p, S4);
    p = fma(x2, p, S3);
    p = fma(x2, p, S2);
    let x3 = x2 * x;
    p = fma(x2, p, S1);
    fma(p, x3, x)
}

/// `sin_piby4_s_fma3`.
#[inline(always)]
fn sin_piby4(x: f64, xx: f64) -> f64 {
    let x2 = x * x;
    let mut p = fma(x2, S6, S5);
    p = fma(x2, p, S4);
    p = fma(x2, p, S3);
    p = fma(x2, p, S2);
    let x3 = x2 * x;
    let mut t = p * x3;
    t = (0.5 * xx) - t;
    t *= x2;
    t -= xx;
    t = fma(-x3, S1, t);
    x - t
}

/// `cos_piby4_s_fma3`.
#[inline(always)]
fn cos_piby4(x: f64, xx: f64) -> f64 {
    let x2 = x * x;
    let r = 0.5 * x2;
    let t = 1.0 - r;
    let mut a = 1.0 - t;
    a -= r;
    a = fma(-x, xx, a);
    let x4 = x2 * x2;
    let mut p = fma(x2, C6, C5);
    p = fma(x2, p, C4);
    p = fma(x2, p, C3);
    p = fma(x2, p, C2);
    p = fma(x2, p, C1);
    p = fma(x4, p, a);
    p + t
}

/// `range_e_5_s_fma3`, for `abs_x` below 5e6. Returns `(r, rr, region)`.
#[inline(always)]
fn reduce_small(abs_x: f64) -> (f64, f64, u64) {
    let n = fma(abs_x, TWOBYPI, 0.5) as i32;
    let region = (i64::from(n) as u64) & 3;
    let npi2 = f64::from(n);
    let mut rhead = fma(-npi2, PIBY2_1, abs_x);
    let mut rtail = npi2 * PIBY2_1TAIL;
    let expdiff =
        ((abs_x.to_bits() >> 52) as u32).wrapping_sub(((rhead.to_bits() & EXP) >> 52) as u32);
    if expdiff >= 15 {
        let t = rhead;
        rtail = npi2 * PIBY2_2;
        rhead = t - rtail;
        let a = -((t - rhead) - rtail);
        rtail = fma(npi2, PIBY2_2TAIL, a);
        if expdiff >= 48 {
            let t = rhead;
            rtail = npi2 * PIBY2_3;
            rhead = t - rtail;
            let a = -((t - rhead) - rtail);
            rtail = fma(npi2, PIBY2_3TAIL, a);
        }
    }
    let r = rhead - rtail;
    let rr = (rhead - r) - rtail;
    (r, rr, region)
}

#[inline(always)]
fn pibits_u64(at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&PIBITS[at..at + 8]);
    u64::from_le_bytes(b)
}

/// `call_remainder_piby2_fma3`, for `abs_x` at or above 5e6. Returns `(r, rr, region)`.
fn reduce_large(ax: u64) -> (f64, f64, u64) {
    let xexp = (ax >> 52).wrapping_sub(1023);
    let last = 134 - (xexp >> 3) as usize;
    let mant = ((ax << 12) >> 12) | (1u64 << 52);
    let a0 = pibits_u64(last);
    let a1 = pibits_u64(last + 8);
    let a2 = pibits_u64(last + 16);

    let w0 = u128::from(a0) * u128::from(mant);
    let mut r8 = w0 as u64;
    let carry = (w0 >> 64) as u64;
    let w1 = u128::from(a1) * u128::from(mant) + u128::from(carry);
    let resexp = xexp & 7;
    let mut r9 = w1 as u64;
    let carry = (w1 >> 64) as u64;
    let mut r10 = a2.wrapping_mul(mant).wrapping_add(carry);

    let shift = 54 - resexp as u32;
    let rax = r10 >> shift;
    let point_five = (r10 >> (shift - 1)) & 1;
    let mut sign = 0u64;
    if point_five != 0 {
        r10 = !r10;
        r9 = !r9;
        r8 = !r8;
        sign = SIGN;
    }
    let region = rax.wrapping_add(point_five) & 3;

    let int_bits = (resexp + 10) as u32;
    r10 = (r10 << int_bits) >> int_bits;
    let mut e = i64::from(int_bits) - 64;
    if r10 == 0 {
        r10 = r9;
        r9 = r8;
        r8 = 0;
        e -= 64;
    }
    let hsb = 63 - i64::from(r10.leading_zeros());
    e += hsb;
    let sh = hsb - 52;
    if sh > 0 {
        let c = sh as u32;
        r8 = r10;
        r10 >>= c;
        r9 >>= c;
        r9 |= r8 << (64 - c);
    } else if sh < 0 {
        let c = (-sh) as u32;
        let t = r9;
        r10 <<= c;
        r9 <<= c;
        r10 |= t >> (64 - c);
        r9 |= r8 >> (64 - c);
    }
    let mut r11 = (e + 1023) as u64;
    r10 &= !(1u64 << 52);
    let x = d(r10 | sign | (r11 << 52));

    let hsb9 = if r9 == 0 {
        0
    } else {
        63 - u64::from(r9.leading_zeros())
    };
    let c = 64 - hsb9;
    r9 = r9.wrapping_shl(c as u32) >> 12;
    r11 = r11.wrapping_sub(c + 52);
    let xx = d(r9 | sign | (r11 << 52));

    let hx = d(x.to_bits() & 0xffff_ffff_f800_0000);
    let tx = x - hx;
    let c = x * PIBY2_LEAD;
    let mut cc = hx * PIBY2_PART1;
    cc -= c;
    cc = fma(tx, PIBY2_PART1, cc);
    cc = fma(hx, PIBY2_PART2, cc);
    cc = fma(tx, PIBY2_PART2, cc);
    let t = fma(x, PIBY2_PART3, xx * PIBY2_LEAD);
    cc += t;
    let r = c + cc;
    let rr = (c - r) + cc;
    (r, rr, region)
}

#[inline(always)]
fn reduce(abs_x: f64) -> (f64, f64, u64) {
    let ax = abs_x.to_bits();
    if ax >= E_5 {
        reduce_large(ax)
    } else {
        reduce_small(abs_x)
    }
}

/// Sine of `x` in radians.
pub fn sin(x: f64) -> f64 {
    let ux = x.to_bits();
    let ax = ux & ABS;
    if ax < PIBY4_BITS {
        if ax >= SMALL_3F2 {
            return sin_piby4_zero(x);
        }
        if ax >= SMALL_3E4 {
            let x3 = (x * x) * x;
            return fma(-x3, POINT_166, x);
        }
        return x;
    }
    if ux & EXP == EXP {
        return nan_of(x);
    }
    let (r, rr, region) = reduce(d(ax));
    let v = if region & 1 == 1 {
        cos_piby4(r, rr)
    } else {
        sin_piby4(r, rr)
    };
    if (region >= 2) != (ux & SIGN != 0) {
        d(v.to_bits() ^ SIGN)
    } else {
        v
    }
}

/// Cosine of `x` in radians.
pub fn cos(x: f64) -> f64 {
    let ux = x.to_bits();
    let ax = ux & ABS;
    if ax <= PIBY4_BITS {
        if ax >= SMALL_3F2 {
            return cos_direct(x);
        }
        if ax >= SMALL_3E4 {
            let h = 0.5 * x;
            return fma(-h, x, 1.0);
        }
        return 1.0;
    }
    if ux & EXP == EXP {
        return nan_of(x);
    }
    let (r, rr, region) = reduce(d(ax));
    let v = if region & 1 == 1 {
        sin_piby4(r, rr)
    } else {
        cos_piby4(r, rr)
    };
    if region == 1 || region == 2 {
        d(v.to_bits() ^ SIGN)
    } else {
        v
    }
}

/// The evaluation for `|x| < pi/4`, `1 + x^2 (x^2 p(x^2) - 1/2)`. The published FMA3 code routes
/// this through `cos_piby4`; this order was identified by black-box comparison against the
/// reference (`tests/maths_parity.rs`).
#[inline(always)]
fn cos_direct(x: f64) -> f64 {
    let x2 = x * x;
    let mut p = fma(x2, C6, C5);
    p = fma(x2, p, C4);
    p = fma(x2, p, C3);
    p = fma(x2, p, C2);
    p = fma(x2, p, C1);
    fma(x2, fma(x2, p, -0.5), 1.0)
}

/// `tan_piby4_s_zero_fma3`, except for the last step: `fma(x, a, x)` in place of `x + x * a`, as
/// identified by black-box comparison against the reference (`tests/maths_parity.rs`).
#[inline(always)]
fn tan_piby4_zero(x0: f64) -> f64 {
    let gt = x0 > POINT_68;
    let lt = x0 < -POINT_68;
    let mut x5 = if gt { d(x0.to_bits() ^ SIGN) } else { x0 };
    let sgn = if lt { SIGN } else { 0 };
    x5 += PIBY4_LEAD;
    x5 += PIBY4_TAIL;
    let transform = gt || lt;
    let x = if transform { x5 } else { x0 };
    let r = x * x;
    let mut a = fma(r, TAN_C3, TAN_C2);
    a = fma(r, a, TAN_C1);
    let mut b = fma(-r, TAN_C7, TAN_C6);
    b = fma(r, b, TAN_C5);
    b = fma(r, b, TAN_C4);
    a /= b;
    a *= r;
    if !transform {
        return fma(x, a, x);
    }
    let t = fma(x, a, x);
    let q = (t * 2.0) / (t + 1.0);
    d((1.0 - q).to_bits() ^ sgn)
}

/// `tan_piby4_s_fma3`. `recip` selects `-1/tan`.
#[inline(always)]
fn tan_piby4(x0: f64, xx0: f64, recip: bool) -> f64 {
    let gt = x0 > POINT_68;
    let lt = x0 < -POINT_68;
    let flip = if gt { SIGN } else { 0 };
    let mut x5 = d(x0.to_bits() ^ flip);
    let xx6 = d(xx0.to_bits() ^ flip) + PIBY4_TAIL;
    let sgn = if lt { SIGN } else { 0 };
    x5 += PIBY4_LEAD;
    x5 += xx6;
    let transform = gt || lt;
    let x = if transform { x5 } else { x0 };
    let xx = if transform { 0.0 } else { xx0 };

    let x2 = x * x;
    // VFMADD132SD .L_two, x2, (x*xx): one fused step on the already rounded product.
    let r = fma(x * xx, 2.0, x2);
    let mut a = fma(r, TAN_C3, TAN_C2);
    a = fma(r, a, TAN_C1);
    let mut b = fma(-r, TAN_C7, TAN_C6);
    b = fma(r, b, TAN_C5);
    b = fma(r, b, TAN_C4);
    a /= b;
    a *= r;
    let t2 = fma(x, a, xx);
    let t = x + t2;

    if transform {
        let two_t = t * 2.0;
        let den = if recip { t - 1.0 } else { t + 1.0 };
        let q = two_t / den;
        let v = if recip { q - 1.0 } else { 1.0 - q };
        return d(v.to_bits() ^ sgn);
    }
    if !recip {
        return t;
    }
    let z1 = d(t.to_bits() & HALF_MASK);
    let z2 = t2 - (z1 - x);
    let trec = -1.0 / t;
    let trec_top = d(trec.to_bits() & HALF_MASK);
    let mut v = fma(trec_top, z1, 1.0);
    v = fma(z2, trec_top, v);
    fma(trec, v, trec_top)
}

/// Tangent of `x` in radians.
pub fn tan(x: f64) -> f64 {
    let ux = x.to_bits();
    let ax = ux & ABS;
    if ax <= PIBY4_BITS {
        if ax >= SMALL_3E4 {
            return tan_piby4_zero(x);
        }
        return x;
    }
    if ux & EXP == EXP {
        return nan_of(x);
    }
    let (r, rr, region) = reduce(d(ax));
    let v = tan_piby4(r, rr, region & 1 == 1);
    d(v.to_bits() ^ (ux & SIGN))
}

/// Infinity gives NaN and NaN stays NaN.
#[inline(always)]
fn nan_of(x: f64) -> f64 {
    if x.is_nan() {
        d(x.to_bits() | 0x0008_0000_0000_0000)
    } else {
        d(0xfff8_0000_0000_0000)
    }
}

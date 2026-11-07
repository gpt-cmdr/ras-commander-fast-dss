//! Fused multiply-add with no platform call.
//!
//! `f64::mul_add` is one instruction where the build has FMA (x86-64 with `+fma`, every AArch64
//! target). Elsewhere it calls the platform's `fma`, which the platform-parity decision does not
//! allow, so those builds use `software`: exact integer arithmetic, rounded once to nearest-even.
//! `tests/maths_parity.rs` compares the two wherever the tests run.

/// One fused multiply-add, `a * b + c`, with a single rounding.
#[cfg(any(target_feature = "fma", target_arch = "aarch64"))]
#[inline(always)]
pub(super) fn fma(a: f64, b: f64, c: f64) -> f64 {
    a.mul_add(b, c)
}

/// One fused multiply-add, `a * b + c`, with a single rounding.
#[cfg(not(any(target_feature = "fma", target_arch = "aarch64")))]
#[inline(always)]
pub(super) fn fma(a: f64, b: f64, c: f64) -> f64 {
    software(a, b, c)
}

const SIGN: u64 = 0x8000_0000_0000_0000;

/// `(significand, exponent)` with value `significand * 2^exponent`, for a finite nonzero input.
#[inline]
fn split(v: f64) -> (u64, i32) {
    let bits = v.to_bits() & !SIGN;
    let field = (bits >> 52) as i32;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    if field == 0 {
        (frac, -1074)
    } else {
        (frac | (1u64 << 52), field - 1075)
    }
}

/// Exact `a * b + c`, rounded to nearest, ties to even. Finite results match IEEE 754
/// fusedMultiplyAdd in the default rounding mode. NaN payloads are not specified.
pub fn software(a: f64, b: f64, c: f64) -> f64 {
    if a.is_finite() && b.is_finite() && !c.is_finite() {
        // The exact product is finite, even if rounding it alone would overflow.
        return c + c;
    }
    if !(a.is_finite() && b.is_finite() && c.is_finite()) || a == 0.0 || b == 0.0 {
        return a * b + c;
    }
    if c == 0.0 {
        // The rounded product is the answer. Adding a zero could lose the sign of a product that
        // rounds to zero.
        return a * b;
    }
    let product_negative = (a.to_bits() ^ b.to_bits()) & SIGN != 0;
    let c_negative = c.to_bits() & SIGN != 0;

    // Both magnitudes are placed with their top bit at bit 126, which leaves room for a carry.
    // The product has at most 106 significant bits and c at most 53, so at least 20 low bits of
    // each are zero before any alignment.
    let (ma, ea) = split(a);
    let (mb, eb) = split(b);
    let product = u128::from(ma) * u128::from(mb);
    let lz = product.leading_zeros() as i32 - 1;
    let p = product << lz;
    let ep = ea + eb - lz;

    let (mc, ec) = split(c);
    let lzc = u128::from(mc).leading_zeros() as i32 - 1;
    let q = u128::from(mc) << lzc;
    let eq = ec - lzc;

    // Shift the one with the smaller exponent right, folding lost bits into a sticky low bit.
    // Bits are lost only when the shift exceeds 20, and then cancellation can remove at most one
    // leading bit, so the sticky bit stays far below the rounding position. A shift small enough
    // for deep cancellation is exact.
    let (hi, hi_neg, lo, lo_neg, e) = if ep >= eq {
        (
            p,
            product_negative,
            sticky_shift(q, ep - eq),
            c_negative,
            ep,
        )
    } else {
        (
            q,
            c_negative,
            sticky_shift(p, eq - ep),
            product_negative,
            eq,
        )
    };

    let (sum, negative) = if hi_neg == lo_neg {
        (hi + lo, hi_neg)
    } else if hi >= lo {
        (hi - lo, hi_neg)
    } else {
        (lo - hi, lo_neg)
    };
    if sum == 0 {
        return 0.0;
    }

    let msb = 127 - sum.leading_zeros() as i32;
    let top = msb + e;
    let sign_bits = if negative { SIGN } else { 0 };
    if top > 1023 {
        return f64::from_bits(sign_bits | 0x7ff0_0000_0000_0000);
    }
    // The result's lowest kept bit: 53 significant bits, or the fixed 2^-1074 for a subnormal.
    let subnormal = top < -1022;
    let shift = if subnormal { -1074 - e } else { msb - 52 };
    let mant = round_shift(sum, shift);
    let magnitude = if subnormal {
        mant // A carry into bit 52 gives the smallest normal number, as it should.
    } else {
        // The hidden bit adds one to the exponent field, and a rounding carry to 2^53 adds one more.
        (((top + 1022) as u64) << 52).wrapping_add(mant)
    };
    f64::from_bits(sign_bits | magnitude)
}

#[inline]
fn sticky_shift(v: u128, by: i32) -> u128 {
    if by == 0 {
        v
    } else if by >= 128 {
        1
    } else {
        (v >> by) | u128::from(v << (128 - by) != 0)
    }
}

#[inline]
fn round_shift(v: u128, shift: i32) -> u64 {
    if shift <= 0 {
        return (v << -shift) as u64;
    }
    if shift >= 128 {
        return 0;
    }
    let kept = (v >> shift) as u64;
    let rest = v << (128 - shift);
    let half = 1u128 << 127;
    if rest > half || (rest == half && kept & 1 == 1) {
        kept + 1
    } else {
        kept
    }
}

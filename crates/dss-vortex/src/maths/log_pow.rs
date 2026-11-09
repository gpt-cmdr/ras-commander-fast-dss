//! `log`, ported from AOCL-LibM `src/isa/avx2/gas/log_fma3.S`. `pow`, ported from ARM
//! optimized-routines `math/pow.c` (FMA branch), with a 256-entry exponential table built from
//! its definition; `tests/maths_parity.rs` checks them.

use super::fma;
use super::tables::{
    EXP_HEAD, EXP_TABLE, EXP_TAIL, LOG_256_LEAD, LOG_256_TAIL, LOG_F_INV, POW_LOG_INVC,
    POW_LOG_LOGC, POW_LOG_TAIL, TWO_TO_JBY256_HEAD, TWO_TO_JBY256_TAIL,
};

const fn d(bits: u64) -> f64 {
    f64::from_bits(bits)
}

const MANT: u64 = 0x000f_ffff_ffff_ffff;
const HALF_BITS: u64 = 0x3fe0_0000_0000_0000;
const ONE_BITS: u64 = 0x3ff0_0000_0000_0000;
const SIGN: u64 = 0x8000_0000_0000_0000;
const EXP: u64 = 0x7ff0_0000_0000_0000;
const INF: u64 = EXP;
const QNAN_BIT: u64 = 0x0008_0000_0000_0000;
const NEG_NAN: f64 = d(0xfff8_0000_0000_0000);
const LOG2_LEAD: f64 = d(0x3fe62e42e0000000);
const LOG2_TAIL: f64 = d(0x3e6efa39ef35793c);
const O3: f64 = d(0x3fd5555555555555);
const O4: f64 = d(0x3fd0000000000000);
const O5: f64 = d(0x3fc999999999999a);
const O6: f64 = d(0x3fc5555555555555);
const CA1: f64 = d(0x3fb55555555554e6);
const CA2: f64 = d(0x3f89999999bac6d4);
const CA3: f64 = d(0x3f62492307f1519f);
const CA4: f64 = d(0x3f3c8034c85dfff0);

/// `(mantissa bits, xexp)`, normalising a subnormal as `.L__denormal_adjust` does.
#[inline(always)]
fn split(ux: u64) -> (u64, f64) {
    let raw = (ux >> 52) as i64 - 1023;
    if raw == -1023 {
        let t = d((ux & MANT) | ONE_BITS) - 1.0;
        let tb = t.to_bits();
        (tb & MANT, ((tb >> 52) as i64 - 2045) as f64)
    } else {
        (ux & MANT, raw as f64)
    }
}

/// Natural logarithm.
pub fn log(x: f64) -> f64 {
    let ux = x.to_bits();
    let top = ux >> 52;
    if top >= 0x7ff || top == 0 {
        if ux & EXP == EXP {
            if ux == INF {
                return x;
            }
            if ux == (INF | SIGN) {
                return NEG_NAN;
            }
            return d(ux | QNAN_BIT);
        }
        if ux & !SIGN == 0 {
            return d(INF | SIGN);
        }
        if ux & SIGN != 0 {
            return NEG_NAN;
        }
    }
    let (mant, xexp) = split(ux);
    if (x - 1.0).abs() < 0.0625 {
        let r = x - 1.0;
        let u2 = r / (2.0 + r);
        let correction = r * u2;
        let u = u2 + u2;
        let v = u * u;
        let a = fma(v, CA2, CA1);
        let b = fma(v, CA4, CA3);
        let u3 = u * v;
        let mut p = a * u3;
        let u7 = (u3 * u3) * u;
        p = fma(b, u7, p);
        p -= correction;
        return r + p;
    }
    let f_bits = (mant & 0x000f_f000_0000_0000) + ((mant & 0x0000_0800_0000_0000) << 1);
    let idx = (f_bits >> 44) as usize;
    let f_cap = d(f_bits | HALF_BITS);
    let y = d(mant | HALF_BITS);
    let r = (f_cap - y) * d(LOG_F_INV[idx]);
    let r2 = r * r;
    let mut a = fma(r, O6, O5);
    let b = fma(r, O3, 0.5);
    a = fma(r, a, O4);
    let r4 = r2 * r2;
    let mut p = fma(b, r2, r);
    p = fma(a, r4, p);
    let t = fma(xexp, LOG2_TAIL, -p);
    let lo = d(LOG_256_TAIL[idx]) + t;
    let hi = fma(xexp, LOG2_LEAD, d(LOG_256_LEAD[idx]));
    hi + lo
}

const N_BY_LOG2: f64 = d(0x40771547652b82fe);
const LOG2_BY_N_HEAD: f64 = d(0x3f662e42fefc0000);
const NEG_LOG2_BY_N_LO: f64 = d(0x3d2c610ca86c3899);
const BIG: f64 = d(0x4338000000000000);
const B120: f64 = d(0x3f81111111111111);
const B24: f64 = d(0x3fa5555555555555);
const B6: f64 = d(0x3fc5555555555555);
// ARM optimized-routines `exp_data.c` (MIT): N == 256, EXP_POLY_ORDER == 5, EXP_POLY_WIDE.
const POW_C2: f64 = d(0x3fdffffffffffdbd);
const POW_C3: f64 = d(0x3fc555555555543c);
const POW_C4: f64 = d(0x3fa55555cf16e1ed);
const POW_C5: f64 = d(0x3f81111167a4b553);
const AY_MAX: u64 = 0x43e0_0000_0000_0000;
const AY_MIN: u64 = 0x3be0_0000_0000_0000;
// ARM optimized-routines math/pow_log_data.c (MIT): the polynomial scaled as `pow.c` uses it.
const POW_LOG_OFF: u64 = 0x3fe6_9555_0000_0000;
const POW_LN2_HI: f64 = d(0x3fe62e42fefa3800);
const POW_LN2_LO: f64 = d(0x3d2ef35793c76730);
const POW_A: [f64; 6] = [
    d(0xbfe5555555555560),
    d(0x3fe0000000000006),
    d(0x3fe999999959554e),
    d(0xbfe555555529a47a),
    d(0xbff2495b9b4845e9),
    d(0x3ff0002b8b263fc3),
];

/// `x` raised to `y`.
pub fn pow(x: f64, y: f64) -> f64 {
    let ux = x.to_bits();
    let uy = y.to_bits();
    if uy & !SIGN == 0 {
        return if is_snan(ux) { d(ux | QNAN_BIT) } else { 1.0 };
    }
    if uy == ONE_BITS {
        return if is_snan(ux) { d(ux | QNAN_BIT) } else { x };
    }
    let mut negate = 0u64;
    let mut ax = x;
    if ux & SIGN != 0 {
        if uy & EXP >= AY_MAX {
            return large_y(ux, uy, negate);
        }
        let ay = uy & !SIGN;
        let yexp = (ay >> 52) as i64 - 1023;
        if yexp < 0 {
            return neg_not_int(ux, uy);
        }
        if yexp <= 53 {
            if (MANT >> yexp) & ay != 0 {
                return neg_not_int(ux, uy);
            }
            if ((1u64 << 52) >> yexp) & ay != 0 {
                negate = SIGN;
            }
        }
        if ux == SIGN {
            return x_zero(uy, negate);
        }
        if ux == (ONE_BITS | SIGN) {
            return if is_nan_bits(uy) {
                d(uy | QNAN_BIT)
            } else {
                d(ONE_BITS | negate)
            };
        }
        if ux & EXP == EXP {
            return x_inf_nan(ux, uy, negate);
        }
        ax = d(ux & !SIGN);
    } else {
        if ux == ONE_BITS {
            return if is_snan(uy) { d(uy | QNAN_BIT) } else { 1.0 };
        }
        if ux == 0 {
            return x_zero(uy, 0);
        }
        if ux & EXP == EXP {
            return x_inf_nan(ux, uy, 0);
        }
        if uy & EXP >= AY_MAX {
            return large_y(ux, uy, 0);
        }
        if uy & EXP < AY_MIN {
            return if ux > ONE_BITS { 1.0 + y } else { 1.0 - y };
        }
    }

    // log(x) as hi + lo: log_inline() in ARM optimized-routines math/pow.c, its FMA branch.
    let ix = if ax.to_bits() >> 52 == 0 {
        (ax * d(0x4330000000000000))
            .to_bits()
            .wrapping_sub(52u64 << 52)
    } else {
        ax.to_bits()
    };
    let tmp = ix.wrapping_sub(POW_LOG_OFF);
    let idx = ((tmp >> 45) % 128) as usize;
    let k = ((tmp as i64) >> 52) as f64;
    let z = d(ix.wrapping_sub(tmp & (0xfffu64 << 52)));
    let invc = d(POW_LOG_INVC[idx]);
    let r = fma(z, invc, -1.0);
    let t1 = fma(k, POW_LN2_HI, d(POW_LOG_LOGC[idx]));
    let t2 = t1 + r;
    let lo1 = fma(k, POW_LN2_LO, d(POW_LOG_TAIL[idx]));
    let lo2 = t1 - t2 + r;
    let ar = -0.5 * r;
    let ar2 = r * ar;
    let ar3 = r * ar2;
    let h = t2 + ar2;
    let lo3 = fma(ar, r, -ar2);
    let lo4 = t2 - h + ar2;
    let inner = fma(ar2, fma(r, POW_A[5], POW_A[4]), fma(r, POW_A[3], POW_A[2]));
    let p = ar3 * fma(ar2, inner, fma(r, POW_A[1], POW_A[0]));
    let lo_sum = lo1 + lo2 + lo3 + lo4 + p;
    let hi = h + lo_sum;
    let lo = h - hi + lo_sum;

    let v = hi * y;
    let vt = fma(y, lo, fma(y, hi, -v));

    pow_exp(v, vt, negate)
}

/// `exp_inline()` in ARM optimized-routines `math/pow.c`, over 256 entries, with `negate` as the
/// result's sign. `v` carries the exponent; `vt` is its low part.
fn pow_exp(v: f64, vt: f64, negate: u64) -> f64 {
    let abstop = (v.to_bits() >> 52) & 0x7ff;
    let mut special = false;
    if !(0x3c9..0x408).contains(&abstop) {
        if abstop < 0x3c9 {
            // |v| < 2^-54: the result rounds to 1 or its neighbour.
            return d((1.0 + v).to_bits() | negate);
        }
        if abstop >= 0x409 {
            // |v| >= 1024: overflow or underflow.
            return d(if v.to_bits() & SIGN != 0 {
                negate
            } else {
                INF | negate
            });
        }
        // 512 <= |v| < 1024 may overflow or underflow only after reconstruction.
        special = true;
    }
    let shifted = v * N_BY_LOG2 + BIG;
    let dn = shifted - BIG;
    let ki = shifted.to_bits();
    let i = (ki & 255) as usize;
    let r = fma(NEG_LOG2_BY_N_LO, dn, fma(-dn, LOG2_BY_N_HEAD, v)) + vt;
    let rr = r * r;
    let t2 = fma(POW_C3, r, POW_C2);
    let t3 = fma(POW_C5, r, POW_C4);
    let tail = d(TWO_TO_JBY256_TAIL[i]);
    // The outer additions stay separate; only the inner pairs are fused.
    let tmp = (tail + r) + rr * t2 + rr * rr * t3;
    // `TWO_TO_JBY256_HEAD` is 2^(i/256), with its exponent intact, so remove the index from `ki`.
    let sbits = TWO_TO_JBY256_HEAD[i]
        .wrapping_sub((i as u64) << 44)
        .wrapping_add(ki << 44);
    if special {
        return d(pow_exp_special(tmp, sbits, ki).to_bits() | negate);
    }
    let scale = d(sbits);
    d((scale + scale * tmp).to_bits() | negate)
}

/// `specialcase()` in the same source: the result may overflow, or be subnormal.
fn pow_exp_special(tmp: f64, sbits: u64, ki: u64) -> f64 {
    if ki & 0x8000_0000 == 0 {
        // k > 0: the scale's exponent might have overflowed by up to 460.
        let scale = d(sbits.wrapping_sub(1009u64 << 52));
        return (scale + scale * tmp) * d((1009u64 + 1023) << 52);
    }
    // k < 0: round in the normal range first, to avoid double rounding in the subnormal range.
    let scale = d(sbits.wrapping_add(1022u64 << 52));
    let mut y = scale + scale * tmp;
    if y < 1.0 {
        let lo = scale - y + scale * tmp;
        let hi = 1.0 + y;
        let lo = 1.0 - hi + y + lo;
        y = (hi + lo) - 1.0;
    }
    y * d(1u64 << 52)
}

/// `e` raised to `x`. Ported from AOCL-LibM `src/isa/avx2/gas/exp_fma3.S`. Not bit-identical to
/// the reference (about 0.4% of inputs differ by 1 ulp), and not reached by the warp cases.
pub fn exp(x: f64) -> f64 {
    let ux = x.to_bits();
    if ux & !SIGN >= 0x4086_2e42_fefa_39ef {
        if x.is_nan() {
            return d(ux | QNAN_BIT);
        }
        if x > d(0x4086_2e42_fefa_39ef) {
            return d(INF);
        }
        if x <= d(0xc087_4046_dfef_d9d0) {
            return if x <= d(0xc087_4910_d52d_3051) {
                0.0
            } else {
                d(1)
            };
        }
    }
    let k = x * d(0x40571547652b82fe);
    let shifted = k + BIG;
    let dn = shifted - BIG;
    let ecx = shifted.to_bits() as u32 as i32;
    let r = fma(dn, d(0xbd1cf79abc9e3b39), fma(dn, d(0xbf862e42fefa0000), x));
    let j = (ecx & 0x3f) as usize;
    let m = ecx >> 6;
    let t = fma(r, B6, 0.5);
    let u = fma(r, B120, B24);
    let rr = r * r;
    let a = fma(rr, t, r);
    let c = fma(rr, d(0x3f56c16c16c16c17), u);
    let q = fma(c, rr * rr, a);
    let res = (q * d(EXP_TABLE[j]) + d(EXP_TAIL[j])) + d(EXP_HEAD[j]);
    if m < -1022 || (m == -1022 && res < 1.0) {
        return res * d(1u64 << ((m + 1074) as u32 & 63));
    }
    d(res.to_bits().wrapping_add((i64::from(m) as u64) << 52))
}

#[inline(always)]
fn is_nan_bits(u: u64) -> bool {
    u & !SIGN > INF
}

#[inline(always)]
fn is_snan(u: u64) -> bool {
    is_nan_bits(u) && u & QNAN_BIT == 0
}

fn neg_not_int(ux: u64, uy: u64) -> f64 {
    if ux & EXP == EXP {
        return x_inf_nan(ux, uy, 0);
    }
    if ux == SIGN {
        return x_zero(uy, 0);
    }
    NEG_NAN
}

fn x_zero(uy: u64, negate: u64) -> f64 {
    if uy & EXP == EXP {
        if uy == (INF | SIGN) {
            return d(INF);
        }
        if uy & MANT != 0 {
            return d(uy | QNAN_BIT);
        }
        return 0.0;
    }
    if uy & SIGN != 0 {
        return d(INF | negate);
    }
    d(negate)
}

fn x_inf_nan(ux: u64, uy: u64, negate: u64) -> f64 {
    if ux & MANT != 0 {
        return d(ux | QNAN_BIT);
    }
    if is_nan_bits(uy) {
        return d(uy | QNAN_BIT);
    }
    if ux & SIGN != 0 && uy & EXP == EXP {
        return if uy & SIGN == 0 { d(INF) } else { 0.0 };
    }
    d(if uy & SIGN == 0 { INF | negate } else { negate })
}

fn large_y(ux: u64, uy: u64, negate: u64) -> f64 {
    if ux & EXP == EXP {
        return x_inf_nan(ux, uy, negate);
    }
    let ax = ux & !SIGN;
    if ax == 0 {
        return x_zero(uy, negate);
    }
    if is_nan_bits(uy) {
        return d(uy | QNAN_BIT);
    }
    if ax == ONE_BITS {
        return 1.0;
    }
    let grow = (ax > ONE_BITS) == (uy & SIGN == 0);
    d(if grow { INF } else { 0 })
}

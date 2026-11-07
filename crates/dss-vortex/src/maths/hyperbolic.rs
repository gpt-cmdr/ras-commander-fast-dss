#![allow(clippy::excessive_precision)]
//! `sinh` and `asinh`, the two functions PROJ's exact Transverse Mercator adds to the value path
//! (`tmerc.cpp` `exact_e_inv` and `exact_e_fwd`). Identified against the reference by black-box
//! comparison, as the rest of this module was; see the P53b report.
//!
//! `sinh` is AOCL-LibM `src/ref/sinh.c` (BSD-3-Clause; see `NOTICE`), with the two polynomials'
//! multiply-adds contracted into fused steps and the reconstruction fused as written below.
//! The reference leaves that table form at a point between 36 and 37 for one through its `exp`,
//! which is not matched here, so arguments from 36 up keep the earlier form on
//! [`super::uncertified::exp`].
//!
//! `asinh` is not AMD's in the reference. Its results are those of
//! `log1p(|x| + x^2 / (1 + sqrt(1 + x^2)))` with `log1p(v) = log(u) - ((u - 1) - v) / u`,
//! `u = 1 + v`, over the reference's own `log`, which [`super::log`] reproduces, for every
//! normal `|x|` below `2^54`, with no cutoff for small arguments. From `2^60` they are those of
//! `log(|x|) + ln 2`; between the two the reference changes form at a point not located, and
//! this is uncertified there. Transverse Mercator's argument stays below `2^54`.

use super::{fma, log};

const fn d(bits: u64) -> f64 {
    f64::from_bits(bits)
}

const SIGN: u64 = 0x8000_0000_0000_0000;
const INF: u64 = 0x7ff0_0000_0000_0000;
const QNAN_BIT: u64 = 0x0008_0000_0000_0000;
/// Below this, `sinh` returns its argument: `2^-28`.
const TINY: u64 = 0x3e30_0000_0000_0000;

/// `2^54`: from here `asinh` is `log(|x|) + ln 2`.
const ASINH_LARGE: u64 = 0x4350_0000_0000_0000;
const LN2: f64 = d(0x3fe6_2e42_fefa_39ef);

// Lead and tail of sinh(i) and cosh(i), i = 0..=36; the lead has 26 bits.
const SINH_LEAD: [u64; 37] = [
    0x0000000000000000,
    0x3ff2cd9fc0000000,
    0x400d03cf60000000,
    0x40240926e0000000,
    0x403b4a3800000000,
    0x40528d0160000000,
    0x406936d228000000,
    0x4081228768000000,
    0x409749ea50000000,
    0x40afa71570000000,
    0x40c5829dc8000000,
    0x40dd3c4488000000,
    0x40f3de1650000000,
    0x410b00b590000000,
    0x412259ac48000000,
    0x4138f0cca8000000,
    0x4150f2ebd0000000,
    0x4167093488000000,
    0x417f4f2208000000,
    0x419546d8f8000000,
    0x41aceb0888000000,
    0x41c3a6e1f8000000,
    0x41dab5adb8000000,
    0x41f226af30000000,
    0x4208ab7fb0000000,
    0x4220c3d390000000,
    0x4236c93268000000,
    0x424ef822f0000000,
    0x42650bba30000000,
    0x427c9aae40000000,
    0x4293704708000000,
    0x42aa6b7658000000,
    0x42c1f43fc8000000,
    0x42d866f348000000,
    0x42f0953e28000000,
    0x430689e220000000,
    0x431ea215a0000000,
];
const SINH_TAIL: [u64; 37] = [
    0x0000000000000000,
    0x3e513ae6096a0092,
    0x3e5db70cfb79a640,
    0x3e8c2526b66dc067,
    0x3e8b81b18647f380,
    0x3ebbc1cdd1e1eb08,
    0x3ecd9f201534fb09,
    0x3edd1c064a4e9954,
    0x3ed4eca65d06ea74,
    0x3f00c259bcc0ecc5,
    0x3f2b5a6647cf9016,
    0x3f09691adefb0870,
    0x3f53410fc29cde38,
    0x3f46a31a50b6fb3c,
    0x3f57defc71805c40,
    0x3f9eb49fd80e0bab,
    0x3f84fffc7bcd5920,
    0x3fc03a93b6c63435,
    0x3fb1940bb255fd1c,
    0x3fded26e14260b50,
    0x3ffb47401fc9f2a2,
    0x40267bb3f55634f1,
    0x401c435ff8194ddc,
    0x404d8fee052ba63a,
    0x40651d7edccde3f6,
    0x40704b1644557d1a,
    0x4076a6b5ca0a9dc4,
    0x40afd9cc72249aba,
    0x40ce58de693edab5,
    0x40d8c70158ac6363,
    0x40e7614764f43e20,
    0x4106337db36fc718,
    0x41212d98b1f611e2,
    0x412392bc108b37cc,
    0x415ce87bdc3473dc,
    0x414bc8d5ae99ad14,
    0x415d20d76744835c,
];
const COSH_LEAD: [u64; 37] = [
    0x3ff0000000000000,
    0x3ff8b07550000000,
    0x400e18fa08000000,
    0x402422a490000000,
    0x403b4ee858000000,
    0x40528d6fc8000000,
    0x406936e678000000,
    0x4081228948000000,
    0x409749eaa8000000,
    0x40afa71580000000,
    0x40c5829dd0000000,
    0x40dd3c4488000000,
    0x40f3de1650000000,
    0x410b00b590000000,
    0x412259ac48000000,
    0x4138f0cca8000000,
    0x4150f2ebd0000000,
    0x4167093488000000,
    0x417f4f2208000000,
    0x419546d8f8000000,
    0x41aceb0888000000,
    0x41c3a6e1f8000000,
    0x41dab5adb8000000,
    0x41f226af30000000,
    0x4208ab7fb0000000,
    0x4220c3d390000000,
    0x4236c93268000000,
    0x424ef822f0000000,
    0x42650bba30000000,
    0x427c9aae40000000,
    0x4293704708000000,
    0x42aa6b7658000000,
    0x42c1f43fc8000000,
    0x42d866f348000000,
    0x42f0953e28000000,
    0x430689e220000000,
    0x431ea215a0000000,
];
const COSH_TAIL: [u64; 37] = [
    0x0000000000000000,
    0x3e3d9f5504c2bd28,
    0x3e67cb66f0a4c9fd,
    0x3e8f58617928e588,
    0x3e6bc7d000c38d48,
    0x3eaf7f9d4e329998,
    0x3ec6e6e464885269,
    0x3ecba3a8b946c154,
    0x3ed3f4e76110d5a4,
    0x3f017622515a3e2b,
    0x3ee4dc4b528af3d0,
    0x3f11156278615e10,
    0x3f535ad50ed821f5,
    0x3f46b61055f2935c,
    0x3f57e2794a601240,
    0x3f9eb4b45f6aadd3,
    0x3f85000b967b3698,
    0x3fc03a940fadc092,
    0x3fb1940bf3bf874c,
    0x3fded26e1a2a2110,
    0x3ffb4740205796d6,
    0x40267bb3f55cb85d,
    0x401c435ff81e18ac,
    0x404d8fee052bdea4,
    0x40651d7edccde926,
    0x40704b1644557e0e,
    0x4076a6b5ca0a9e1c,
    0x40afd9cc72249abe,
    0x40ce58de693edab5,
    0x40d8c70158ac6364,
    0x40e7614764f43e20,
    0x4106337db36fc718,
    0x41212d98b1f611e2,
    0x412392bc108b37cc,
    0x415ce87bdc3473dc,
    0x414bc8d5ae99ad14,
    0x415d20d76744835c,
];

/// Hyperbolic sine. Matches the reference for `|x| < 36`; from there up it is uncertified.
pub fn sinh(x: f64) -> f64 {
    let ux = x.to_bits();
    let aux = ux & !SIGN;
    if aux < TINY {
        return x;
    }
    if aux >= INF {
        return if aux == INF { x } else { d(ux | QNAN_BIT) };
    }
    let y = d(aux);
    let z = if y < 36.0 {
        let ind = y as usize;
        let dy = y - ind as f64;
        let dy2 = dy * dy;
        let mut p = fma(
            0.7746188980094184251527126e-12,
            dy2,
            0.160576793121939886190847e-9,
        );
        p = fma(p, dy2, 0.250521176994133472333666e-7);
        p = fma(p, dy2, 0.275573191913636406057211e-5);
        p = fma(p, dy2, 0.198412698413242405162014e-3);
        p = fma(p, dy2, 0.833333333333329931873097e-2);
        p = fma(p, dy2, 0.166666666666666667013899e0);
        let sdy = dy * dy2 * p;
        let mut q = fma(
            0.1163921388172173692062032e-10,
            dy2,
            0.208744349831471353536305e-8,
        );
        q = fma(q, dy2, 0.275573350756016588011357e-6);
        q = fma(q, dy2, 0.248015872460622433115785e-4);
        q = fma(q, dy2, 0.138888888889814854814536e-2);
        q = fma(q, dy2, 0.416666666666660876512776e-1);
        q = fma(q, dy2, 0.500000000000000005911074e0);
        let cdy = dy2 * q;
        // sinh(dy) is dy + sdy; move some of dy's low bits into the tail.
        let sdy1 = d(dy.to_bits() & 0xffff_ffff_f800_0000);
        let sdy2 = sdy + (dy - sdy1);
        let (sl, st) = (d(SINH_LEAD[ind]), d(SINH_TAIL[ind]));
        let (cl, ct) = (d(COSH_LEAD[ind]), d(COSH_TAIL[ind]));
        // sinh(y0) cosh(dy) + cosh(y0) sinh(dy), smallest terms first.
        let a = fma(ct, sdy2, st * cdy);
        let a = fma(ct, sdy1, a) + st;
        let a = fma(cl, sdy2, a);
        let a = fma(sl, cdy, a);
        fma(cl, sdy1, a) + sl
    } else {
        let e = super::log_pow::exp(y);
        (e - 1.0 / e) * 0.5
    };
    if ux & SIGN != 0 {
        -z
    } else {
        z
    }
}

/// Inverse hyperbolic sine. Matches the reference for `|x| < 2^54` and from `2^60`.
pub fn asinh(x: f64) -> f64 {
    let ux = x.to_bits();
    let aux = ux & !SIGN;
    if aux == 0 {
        return x;
    }
    if aux >= INF {
        return if aux == INF { x } else { d(ux | QNAN_BIT) };
    }
    let ax = d(aux);
    let w = if aux >= ASINH_LARGE {
        log(ax) + LN2
    } else {
        let t = ax * ax;
        let v = ax + t / (1.0 + (1.0 + t).sqrt());
        let u = 1.0 + v;
        if u == 1.0 {
            v
        } else {
            log(u) - ((u - 1.0) - v) / u
        }
    };
    if ux & SIGN != 0 {
        -w
    } else {
        w
    }
}

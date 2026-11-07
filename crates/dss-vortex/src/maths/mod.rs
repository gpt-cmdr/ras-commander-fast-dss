//! Portable maths for every value-affecting path in the Vortex tier.
//!
//! Decision (2026-11-01, platform parity): the warp and projection paths must give the same bits
//! on Windows, Linux and macOS, and must not call the platform maths library, including through
//! `f64::sin` and friends. Every function here is plain IEEE-754 double arithmetic, with
//! exact fused steps from `fused.rs` where the source algorithm has one, and is ported from
//! AMD's AOCL-LibM (BSD-3-Clause) and, for `pow`, ARM optimized-routines (MIT). See `NOTICE`.
//!
//! The target is bit identity with the qualified reference: `ucrtbase.dll` with FMA3 enabled, as
//! measured from outside by black-box comparison. Evidence: `evidence/P44a/`.
//! Operation order and constants are the algorithm. Do not simplify, reassociate or fuse them.
//!
//! `uncertified::exp` does not reach bit identity, nor does `pow` for bases close to even powers
//! of two with half-integer exponents. That report states both limitations.
//! The identity is a measurement on sampled inputs, not a proof over all inputs.

mod fused;
mod hyperbolic;
mod inverse;
mod log_pow;
mod tables;
mod trig;

pub use hyperbolic::{asinh, sinh};
pub use inverse::{asin, atan, atan2, hypot};
pub use log_pow::{log, pow};
pub use trig::{cos, sin, tan};

/// `exp` for PROJ's exact Transverse Mercator inverse (`tmerc.cpp` `exact_e_inv`), and for nothing
/// else. Below `2^-26` in magnitude the reference returns `1 + x`, and so does this: measured with
/// no difference. From there up it is [`uncertified::exp`], with no bit-identity claim: about 0.4%
/// of results differ by one ulp. In that inverse it feeds only the Clenshaw correction, whose
/// terms are about a thousandth of the coordinate, so a difference reaches a result rarely. The
/// P53b report gives the measured rate and sizes.
pub fn tmerc_exp(x: f64) -> f64 {
    if x.abs() < f64::from_bits(0x3e50_0000_0000_0000) {
        1.0 + x
    } else {
        log_pow::exp(x)
    }
}

use fused::fma;

/// Exposed so tests can compare it against hardware. Callers use the functions above.
#[doc(hidden)]
pub use fused::software as software_fma;

/// Functions that do not match the reference and are not certified for value-affecting code.
///
/// `tests/maths_no_std_floats.rs` fails if crate source outside this module names this path.
pub mod uncertified {
    /// Differs from the reference in about 0.4% of inputs, by 1 ulp. See the P44a report.
    pub use super::log_pow::exp;
}

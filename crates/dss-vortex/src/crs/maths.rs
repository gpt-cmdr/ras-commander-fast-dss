//! The only maths this module's value path may call (maintainer decision, "Platform parity",
//! `VORTEX_DECISIONS_20261101.md`). Platform libm and the UCRT differ, so nothing under `crs/`
//! may use `f64::sin` and friends; `tests/crs_maths_gate.rs` enforces that.
//!
//! This is the narrow interface P43 needs, over P44a's portable module (`crate::maths`). What
//! that gives against PROJ 7.2.0, and what remains, is in P43's report.
//!
//! There is deliberately no platform path here. Evidence runs with host maths copy this crate
//! outside the workspace and replace this file (`oracle/vortex/crs_evidence.py`), so no feature
//! or flag can bring it into a wheel.

use crate::maths;

macro_rules! unary {
    ($($name:ident),* $(,)?) => {$(
        #[inline]
        pub fn $name(x: f64) -> f64 {
            maths::$name(x)
        }
    )*};
}

unary!(sin, cos, tan, asin, atan, log, sinh, asinh, tmerc_exp);

/// `atan2(y, x)`.
#[inline]
pub fn atan2(y: f64, x: f64) -> f64 {
    maths::atan2(y, x)
}

/// `pow(x, y)`.
#[inline]
pub fn pow(x: f64, y: f64) -> f64 {
    maths::pow(x, y)
}

/// `hypot(x, y)`.
#[inline]
pub fn hypot(x: f64, y: f64) -> f64 {
    maths::hypot(x, y)
}

/// Correctly rounded by IEEE 754, so identical everywhere.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

/// Exact, so identical everywhere. Written out because `f64::floor` can become a call to the
/// platform's `floor` on a target without a rounding instruction, such as baseline x86-64.
#[inline]
pub fn floor(x: f64) -> f64 {
    // From 2^52 up, every double is an integer.
    if x.is_nan() || x.abs() >= 4503599627370496.0 {
        return x;
    }
    let t = x as i64 as f64;
    let r = if t > x { t - 1.0 } else { t };
    // Keeps -0.0, the only input whose result's sign the steps above lose.
    r.copysign(x)
}

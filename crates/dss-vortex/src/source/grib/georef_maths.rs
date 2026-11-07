//! The maths the GRIB georeference needs, behind one small interface.
//!
//! Value-affecting code must not call `f64::sin` and friends (platform parity decision,
//! 2026-11-01). These pass through to the portable module, `crate::maths`, which is the same bits
//! on every platform.
//!
//! The reference here is `java.lang.Math` under NetCDF-Java, not the UCRT that `crate::maths`
//! targets. Its results are not bit-identical to `crate::maths`, so the reference for this layer
//! is the `f32` the axis keeps: `Array.makeArray` rounds the start point to `f32` km. The evidence
//! report counts the `f64` differences and shows the `f32` is exact.

pub(super) use crate::maths::{cos, log, pow, sin, tan};

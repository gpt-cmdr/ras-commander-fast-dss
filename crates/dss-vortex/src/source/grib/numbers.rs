//! Number rules shared by the GRIB2 sections. Sources: NetCDF-Java 5.5.3 `GribNumbers` and
//! `ucar.nc2.util.Misc`, and PROJ 7.2.0 `internal::toString`.

/// `GribNumbers.UNDEFINED`.
pub(super) const UNDEFINED: i32 = -9999;

/// `GribNumbers.int4`: sign and magnitude, all ones undefined. The sign test in Java's formula
/// is `1 - ((a & 128) >> 6)`: 1 or -1.
pub(super) fn int4(b: [u8; 4]) -> i32 {
    if b == [0xff; 4] {
        return UNDEFINED;
    }
    let a = i32::from(b[0]);
    let magnitude =
        (a & 127) << 24 | i32::from(b[1]) << 16 | i32::from(b[2]) << 8 | i32::from(b[3]);
    (1 - ((a & 128) >> 6)) * magnitude
}

const POWERS_OF_TEN: [f64; 39] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 1e23, 1e24, 1e25, 1e26, 1e27, 1e28, 1e29, 1e30, 1e31, 1e32,
    1e33, 1e34, 1e35, 1e36, 1e37, 1e38,
];
const POWERS_OF_TENTH: [f64; 38] = [
    1e-1, 1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8, 1e-9, 1e-10, 1e-11, 1e-12, 1e-13, 1e-14, 1e-15,
    1e-16, 1e-17, 1e-18, 1e-19, 1e-20, 1e-21, 1e-22, 1e-23, 1e-24, 1e-25, 1e-26, 1e-27, 1e-28,
    1e-29, 1e-30, 1e-31, 1e-32, 1e-33, 1e-34, 1e-35, 1e-36, 1e-37, 1e-38,
];

/// `Math.pow(10, k)` for the `k` a real field carries. The nearest double, by literal, so it
/// needs no maths library. Outside the table it is `None`, which callers refuse.
pub(super) fn pow10_f64(k: i32) -> Option<f64> {
    match usize::try_from(k) {
        Ok(k) => POWERS_OF_TEN.get(k).copied(),
        Err(_) => POWERS_OF_TENTH
            .get(usize::try_from(-i64::from(k) - 1).ok()?)
            .copied(),
    }
}

/// `(float) Math.pow(10, D)`.
pub(super) fn pow10_f32(k: i32) -> Option<f32> {
    pow10_f64(k).map(|v| v as f32)
}

/// `(float) Math.pow(2.0, E)`. Exact from the exponent; out of range goes to 0 or infinity, as
/// the cast does.
pub(super) fn pow2_f32(e: i32) -> f32 {
    if e > 127 {
        f32::INFINITY
    } else if e >= -126 {
        f32::from_bits(((e + 127) as u32) << 23)
    } else if e >= -149 {
        f32::from_bits(1u32 << (e + 149))
    } else {
        0.0
    }
}

/// `Misc.relativeDifference(float, float) < 1e-5f`.
pub(super) fn nearly_equals_f32(a: f32, b: f32) -> bool {
    const MAX: f32 = 1.0e-5;
    let difference = (a - b).abs();
    let relative = if a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()) {
        0.0
    } else if a == 0.0 || b == 0.0 || difference < f32::MIN_POSITIVE {
        difference / f32::MIN_POSITIVE
    } else {
        difference / a.abs().max(b.abs())
    };
    relative < MAX
}

/// `Misc.nearlyEquals(double, double, double)`.
pub(crate) fn nearly_equals_f64(a: f64, b: f64, max: f64) -> bool {
    let difference = (a - b).abs();
    let relative = if a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()) {
        0.0
    } else if a == 0.0 || b == 0.0 || difference < f64::MIN_POSITIVE {
        difference / f64::MIN_POSITIVE
    } else {
        difference / a.abs().max(b.abs())
    };
    relative < max
}

/// `%.{digits}g` for a value in the range a WKT parameter takes, or `None` outside it.
fn format_g(value: f64, digits: usize) -> Option<String> {
    let scientific = format!("{:.*e}", digits - 1, value);
    let (mantissa, exponent) = scientific.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    if exponent < -4 || exponent >= digits as i32 {
        return None;
    }
    let negative = mantissa.starts_with('-');
    let figures: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut text = if exponent >= 0 {
        let split = exponent as usize + 1;
        format!("{}.{}", &figures[..split], &figures[split..])
    } else {
        format!("0.{}{}", "0".repeat((-exponent - 1) as usize), figures)
    };
    if text.contains('.') {
        text.truncate(text.trim_end_matches('0').trim_end_matches('.').len());
    }
    Some(if negative { format!("-{text}") } else { text })
}

/// How PROJ 7.2.0 writes a WKT number: `WKTFormatter::add` writes 0 as `0`, and
/// `internal::toString` uses `%.15g`, or `%.14g` when that shows a run of nines.
pub(crate) fn wkt_number(value: f64) -> Option<String> {
    if value == 0.0 {
        return Some("0".to_string());
    }
    if !value.is_finite() {
        return None;
    }
    let text = format_g(value, 15)?;
    if text.contains("9999999999") {
        return format_g(value, 14);
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int4_is_sign_and_magnitude() {
        assert_eq!(int4([0, 0, 0, 5]), 5);
        assert_eq!(int4([0x80, 0, 0, 5]), -5);
        assert_eq!(int4([0xff; 4]), UNDEFINED);
    }

    #[test]
    fn powers_cover_the_cast_range() {
        assert_eq!(pow2_f32(0), 1.0);
        assert_eq!(pow2_f32(-3), 0.125);
        assert_eq!(pow2_f32(-149), f32::from_bits(1));
        assert_eq!(pow2_f32(-150), 0.0);
        assert_eq!(pow2_f32(128), f32::INFINITY);
        assert_eq!(pow10_f32(1), Some(10.0));
        assert_eq!(pow10_f64(-3), Some(0.001));
        assert_eq!(pow10_f64(39), None);
        assert_eq!(pow10_f64(i32::MIN), None);
    }

    #[test]
    fn wkt_numbers_read_as_proj_writes_them() {
        assert_eq!(wkt_number(6371229.0).unwrap(), "6371229");
        assert_eq!(wkt_number(262.5).unwrap(), "262.5");
        assert_eq!(wkt_number(0.9330127018922193).unwrap(), "0.933012701892219");
        assert_eq!(
            wkt_number(0.0174532925199433).unwrap(),
            "0.0174532925199433"
        );
        assert_eq!(wkt_number(-38.5).unwrap(), "-38.5");
        assert_eq!(wkt_number(0.0).unwrap(), "0");
        assert_eq!(wkt_number(89.99999999999999).unwrap(), "90");
    }
}

//! Sections 3 and 4: the bitmap and simple packing, to `f32`.
//!
//! Sources, NetCDF-Java 5.5.3: `Grib1SectionBitMap.getBitmap`, `Grib1SectionBinaryData.getBinaryDataInfo`,
//! `Grib1DataReader.getData` and `readSimplePacking`, `GribNumbers.float4`, `int2` and `testBitIsSet`,
//! `GribData.Info.isGridPointData` and `isSimplePacking`, `BitReader.bits2UInt`.
//!
//! The value is Java's, in Java's order and types:
//! `ref = (float) (10^-D * R)`, `scale = (float) (10^-D * 2^E)`, value `ref + scale * X` in `float`,
//! where `R` is the IBM single-precision reference already cast to `float` by `float4`.

use super::pds::UNDEFINED;
use super::scan::{decode_error, u24};
use crate::error::VortexError;
use crate::source::grib::Limits;

fn refused(reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: "GRIB1 data".to_string(),
        reason: reason.into(),
    }
}

/// `Math.pow(10.0, -d)` for the `d` a real field carries: the nearest double, by literal.
fn pow10_negative(d: i32) -> Option<f64> {
    const TENTHS: [f64; 21] = [
        1e0, 1e-1, 1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8, 1e-9, 1e-10, 1e-11, 1e-12, 1e-13,
        1e-14, 1e-15, 1e-16, 1e-17, 1e-18, 1e-19, 1e-20,
    ];
    const TENS: [f64; 21] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20,
    ];
    if d >= 0 {
        TENTHS.get(usize::try_from(d).ok()?).copied()
    } else {
        TENS.get(usize::try_from(-i64::from(d)).ok()?).copied()
    }
}

/// `Math.pow(2.0, e)`: exact for the range a 2-octet scale factor takes.
fn pow2(e: i32) -> f64 {
    // |e| <= 32767; f64 holds 2^-1074 to 2^1023, and outside that Java's pow gives 0 or infinity.
    if e > 1023 {
        f64::INFINITY
    } else if e >= -1022 {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else if e >= -1074 {
        f64::from_bits(1u64 << (e + 1074))
    } else {
        0.0
    }
}

/// `GribNumbers.float4`: IBM single precision, computed in `double` and cast to `float`.
pub(super) fn ibm_float(b: [u8; 4]) -> f32 {
    let mantissa = (u32::from(b[1]) << 16) | (u32::from(b[2]) << 8) | u32::from(b[3]);
    if mantissa == 0 {
        return 0.0;
    }
    let sign = if b[0] & 128 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from(b[0] & 127) - 64;
    // Math.pow(16.0, exp - 6) is a power of two, which pow returns exactly.
    (sign * pow2(4 * (exponent - 6)) * f64::from(mantissa)) as f32
}

/// Section 3: the explicit bitmap, MSB first. A predefined bitmap (octets 5-6 nonzero) is a
/// table the centre publishes; `getBitmap` warns and returns none, so the record would decode as
/// if every point were present. That is refused.
fn bitmap(section: &[u8], points: usize) -> Result<&[u8], VortexError> {
    let length = u24(section, 0).ok_or_else(|| decode_error("BMS truncated".to_string()))?;
    let table = u16::from_be_bytes([section[4], section[5]]);
    if table != 0 {
        return Err(refused(format!(
            "predefined bitmap {table}. Observed behaviour we did not implement: NetCDF-Java \
             ignores it and reads every point as present"
        )));
    }
    if length <= 6 {
        return Err(refused(
            "an empty bitmap section. Observed behaviour we did not implement: NetCDF-Java reads \
             every point as present",
        ));
    }
    let bits = &section[6..length];
    // Octet 4: unused bits at the end of the section (NCEP ON388 section 3; ECMWF GRIB1
    // section 3). Even-octet padding can make it exceed 7. NetCDF-Java ignores it and checks only
    // `8 * bitmap.length < nPts`; here the usable bits must be the grid's points exactly, so
    // padding the file declares unusable cannot supply bitmap entries.
    let unused = usize::from(section[3]);
    let physical = bits.len().saturating_mul(8);
    if unused > physical || physical - unused != points {
        return Err(refused(format!(
            "the bitmap holds {physical} bits and declares {unused} unused, which leaves {} \
             for {points} points",
            physical.saturating_sub(unused)
        )));
    }
    if !padding_is_zero(bits, points, physical) {
        return Err(refused(
            "the bitmap's declared unused bits are not zero".to_string(),
        ));
    }
    Ok(bits)
}

/// Whether bits `from..to` of `bytes`, MSB first, are all zero.
fn padding_is_zero(bytes: &[u8], from: usize, to: usize) -> bool {
    (from..to).all(|i| bytes[i / 8] & (128 >> (i % 8)) == 0)
}

/// Read `bits` bits MSB first from `payload` at bit `at`, as `BitReader.bits2UInt`.
#[inline]
fn read_bits(payload: &[u8], at: usize, bits: u32) -> u32 {
    let mut value: u64 = 0;
    let first = at / 8;
    let skip = (at % 8) as u32;
    let needed = (skip + bits).div_ceil(8) as usize;
    for i in 0..needed {
        value = (value << 8) | u64::from(payload[first + i]);
    }
    let total = needed as u32 * 8;
    ((value >> (total - skip - bits)) & ((1u64 << bits) - 1)) as u32
}

pub(super) struct Inputs<'a> {
    pub bitmap_section: Option<&'a [u8]>,
    pub binary_section: &'a [u8],
    pub decimal_scale: i32,
    pub points: usize,
}

/// Decode one record. Points the bitmap leaves out are NaN (`staticMissingValue`).
pub(super) fn unpack(inputs: &Inputs, limits: &Limits) -> Result<Vec<f32>, VortexError> {
    let points = inputs.points;
    if points > limits.max_cells {
        return Err(VortexError::Refused {
            what: "GRIB1 grid".to_string(),
            reason: format!("{points} cells is over the limit of {}", limits.max_cells),
        });
    }
    let bds = inputs.binary_section;
    let flag = bds[3];
    // GribData.Info: octet 4 bit 1 set is spherical harmonics, bit 2 set is complex or
    // second-order packing. getData throws on the first and reads the second through
    // readExtendedComplexPacking, which Stage IV does not use.
    if flag & 0x80 != 0 {
        return Err(VortexError::Unsupported {
            what: "GRIB1 spherical harmonic coefficients".to_string(),
        });
    }
    if flag & 0x40 != 0 {
        return Err(VortexError::Unsupported {
            what: "GRIB1 complex or second-order packing; this reader takes simple packing, \
                   which Stage IV uses"
                .to_string(),
        });
    }
    // Flag table 11 bit 4: additional flags at octet 14 (matrix values, secondary bitmaps,
    // different widths), which change where and how the values are stored. NetCDF-Java would read
    // the payload from octet 12 as if they were absent. Bit 3 (original data were integers) does
    // not change the decoding and is accepted.
    if flag & 0x10 != 0 {
        return Err(VortexError::Unsupported {
            what: "GRIB1 data section with additional flags (flag table 11 bit 4); this reader \
                   takes simple grid-point packing without them, which Stage IV uses"
                .to_string(),
        });
    }
    let binary_scale = super::pds::int2(bds[4], bds[5]);
    if binary_scale == UNDEFINED {
        return Err(refused("binary scale factor is all ones (undefined)"));
    }
    let reference = ibm_float([bds[6], bds[7], bds[8], bds[9]]);
    let bits = u32::from(bds[10]);
    if bits > 31 {
        return Err(refused(format!(
            "{bits} bits per value; BitReader.bits2UInt returns a signed int"
        )));
    }
    let pow10 = pow10_negative(inputs.decimal_scale).ok_or_else(|| {
        refused(format!(
            "decimal scale factor {} is outside the range a field carries",
            inputs.decimal_scale
        ))
    })?;
    let reference = (pow10 * f64::from(reference)) as f32;
    let scale = (pow10 * pow2(binary_scale)) as f32;
    let payload = &bds[11..];
    let mask = inputs
        .bitmap_section
        .map(|s| bitmap(s, points))
        .transpose()?;
    let present = match mask {
        Some(m) => (0..points)
            .filter(|&i| m[i / 8] & (128 >> (i % 8)) != 0)
            .count(),
        None => points,
    };
    // Octet 4, low nibble: unused bits at the end of the section (flag table 11; NCEP ON388
    // section 4). The usable bits must hold the present values exactly: fewer and Java reads past
    // the section, more and the counts disagree. Stage IV declares its padding exactly.
    let unused = usize::from(flag & 0x0f);
    let physical = payload.len().saturating_mul(8);
    let needed = present.saturating_mul(bits as usize);
    if unused > physical || physical - unused != needed {
        return Err(refused(format!(
            "{present} values of {bits} bits need {needed} bits; the data section holds \
             {physical} and declares {unused} unused, which leaves {}",
            physical.saturating_sub(unused)
        )));
    }
    if !padding_is_zero(payload, needed, physical) {
        return Err(refused(
            "the data section's declared unused bits are not zero".to_string(),
        ));
    }
    let mut values = Vec::new();
    values
        .try_reserve_exact(points)
        .map_err(|_| refused(format!("could not allocate {points} cells")))?;
    let mut position = 0usize;
    for i in 0..points {
        let set = mask.is_none_or(|m| m[i / 8] & (128 >> (i % 8)) != 0);
        if !set {
            values.push(f32::NAN);
        } else if bits == 0 {
            values.push(reference);
        } else {
            let x = read_bits(payload, position, bits);
            position += bits as usize;
            // int to float, then float multiply and add, as Java evaluates it.
            values.push(reference + scale * (x as i32) as f32);
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ibm_floats_read_as_float4() {
        assert_eq!(ibm_float([0x41, 0x10, 0x00, 0x00]), 1.0);
        assert_eq!(ibm_float([0xc1, 0x10, 0x00, 0x00]), -1.0);
        assert_eq!(ibm_float([0x40, 0x80, 0x00, 0x00]), 0.5);
        assert_eq!(ibm_float([0x42, 0x64, 0x00, 0x00]), 100.0);
        assert_eq!(ibm_float([0x00, 0x00, 0x00, 0x00]), 0.0);
    }

    #[test]
    fn bits_read_msb_first() {
        let payload = [0b1010_1100, 0b0101_0000];
        assert_eq!(read_bits(&payload, 0, 3), 0b101);
        assert_eq!(read_bits(&payload, 3, 7), 0b011_0001);
        assert_eq!(read_bits(&payload, 0, 12), 0b1010_1100_0101);
    }

    #[test]
    fn powers_are_exact() {
        assert_eq!(pow2(0), 1.0);
        assert_eq!(pow2(-3), 0.125);
        assert_eq!(pow2(-1074), f64::from_bits(1));
        assert_eq!(pow2(-1075), 0.0);
        assert_eq!(pow10_negative(1), Some(0.1));
        assert_eq!(pow10_negative(-2), Some(100.0));
        assert_eq!(pow10_negative(21), None);
    }
}

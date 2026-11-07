//! Stored values to Vortex's `f32`: scale, offset, unsigned and missing handling.
//!
//! Source: NetCDF-Java 5.5.3 `EnhanceScaleMissingUnsignedImpl`, `N3iosp.getFillValueDefault`, and
//! `Array.get1DJavaArray(DataType.FLOAT)`. The evaluation order is `(float)(scale * v + offset)`,
//! in `double`, with no fused step.

use super::model::{attr_numbers, attr_string, find_attr, Var};
use super::refused;
use crate::error::VortexError;
use crate::source::grib::nearly_equals_f64;
use netcdf_reader::{NcAttrValue, NcAttribute, NcType};

/// `Misc.defaultMaxRelativeDiffFloat`.
const MAX_RELATIVE_DIFF_FLOAT: f64 = 1.0e-5;

/// The numeric types `DataType` orders by `rank`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Byte,
    UByte,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    Float,
    Double,
}

impl Kind {
    pub fn of(dtype: &NcType) -> Option<Kind> {
        Some(match dtype {
            NcType::Byte => Kind::Byte,
            NcType::UByte => Kind::UByte,
            NcType::Short => Kind::Short,
            NcType::UShort => Kind::UShort,
            NcType::Int => Kind::Int,
            NcType::UInt => Kind::UInt,
            NcType::Int64 => Kind::Long,
            NcType::UInt64 => Kind::ULong,
            NcType::Float => Kind::Float,
            NcType::Double => Kind::Double,
            _ => return None,
        })
    }

    fn is_unsigned(self) -> bool {
        matches!(self, Kind::UByte | Kind::UShort | Kind::UInt | Kind::ULong)
    }

    /// `DataType.withSignedness`.
    fn with_unsigned(self, unsigned: bool) -> Kind {
        match (self, unsigned) {
            (Kind::Byte, true) => Kind::UByte,
            (Kind::UByte, false) => Kind::Byte,
            (Kind::Short, true) => Kind::UShort,
            (Kind::UShort, false) => Kind::Short,
            (Kind::Int, true) => Kind::UInt,
            (Kind::UInt, false) => Kind::Int,
            (Kind::Long, true) => Kind::ULong,
            (Kind::ULong, false) => Kind::Long,
            (other, _) => other,
        }
    }

    /// `EnhanceScaleMissingUnsignedImpl.nextLarger`.
    fn next_larger(self) -> Kind {
        match self {
            Kind::Byte => Kind::Short,
            Kind::UByte => Kind::UShort,
            Kind::Short => Kind::Int,
            Kind::UShort => Kind::UInt,
            Kind::Int => Kind::Long,
            Kind::UInt => Kind::ULong,
            Kind::Long | Kind::ULong => Kind::Double,
            other => other,
        }
    }

    /// `N3iosp.getFillValueDefault(...).doubleValue()`. The unsigned constants are stored in a
    /// signed Java type, so they read as -1 and -2.
    fn default_fill(self) -> f64 {
        match self {
            Kind::Byte => -127.0,
            Kind::UByte | Kind::UShort | Kind::UInt => -1.0,
            Kind::Short => -32767.0,
            Kind::Int => -2147483647.0,
            Kind::Long => -9223372036854775806_i64 as f64,
            Kind::ULong => -2.0,
            Kind::Float => f64::from(9.969_21e36_f32),
            Kind::Double => 9.969_209_968_386_869e36,
        }
    }
}

fn attr_kind(attr: &NcAttribute) -> Option<Kind> {
    Some(match &attr.value {
        NcAttrValue::Bytes(_) => Kind::Byte,
        NcAttrValue::Shorts(_) => Kind::Short,
        NcAttrValue::Ints(_) => Kind::Int,
        NcAttrValue::Floats(_) => Kind::Float,
        NcAttrValue::Doubles(_) => Kind::Double,
        NcAttrValue::UBytes(_) => Kind::UByte,
        NcAttrValue::UShorts(_) => Kind::UShort,
        NcAttrValue::UInts(_) => Kind::UInt,
        NcAttrValue::Int64s(_) => Kind::Long,
        NcAttrValue::UInt64s(_) => Kind::ULong,
        _ => return None,
    })
}

/// `DataType.widenNumberIfNegative`, for an attribute or value under an unsigned variable.
fn widen(value: f64, kind: Kind, unsigned: bool) -> f64 {
    if !unsigned || value >= 0.0 {
        return value;
    }
    match kind {
        Kind::Byte | Kind::UByte => value + 256.0,
        Kind::Short | Kind::UShort => value + 65536.0,
        Kind::Int | Kind::UInt => value + 4294967296.0,
        _ => value,
    }
}

/// What `EnhanceScaleMissingUnsignedImpl`'s constructor works out for one variable.
#[derive(Debug, Clone)]
pub(super) struct Enhance {
    orig: Kind,
    unsigned: bool,
    /// `unsignedConversionType`.
    converted: Kind,
    use_scale_offset: bool,
    scale: f64,
    offset: f64,
    /// `scaledOffsetType`.
    scaled: Kind,
    has_valid_range: bool,
    has_valid_min: bool,
    has_valid_max: bool,
    valid_min: f64,
    valid_max: f64,
    has_fill: bool,
    /// `getFillValue`: 0 when the file has none and the format gives no default.
    pub fill: f64,
    has_missing: bool,
    missing: Vec<f64>,
}

impl Enhance {
    /// `is_netcdf`: `orgFileTypeId` names NetCDF, so a missing `_FillValue` takes the default.
    pub fn new(var: &Var, is_netcdf: bool) -> Result<Self, VortexError> {
        let orig = Kind::of(&var.dtype).ok_or_else(|| {
            refused(
                format!("NetCDF variable {}", var.full),
                format!(
                    "data type {:?} is not a number this reader takes",
                    var.dtype
                ),
            )
        })?;
        // 64-bit integers go through different rounding paths; no sample has one.
        if matches!(orig, Kind::Long | Kind::ULong) {
            return Err(refused(
                format!("NetCDF variable {}", var.full),
                "64-bit integer cells have no sample here",
            ));
        }
        let mut unsigned = orig.is_unsigned();
        if !unsigned {
            if let Some(attr) = find_attr(&var.attrs, "_Unsigned").and_then(attr_string) {
                unsigned = attr.eq_ignore_ascii_case("true");
            }
        }
        let converted = if unsigned {
            orig.with_unsigned(true).next_larger().with_unsigned(true)
        } else {
            orig
        };
        let attribute_kind =
            |attr: &NcAttribute| attr_kind(attr).map(|k| k.with_unsigned(unsigned));
        let first = |attr: &NcAttribute, index: usize| -> Option<f64> {
            let kind = attribute_kind(attr)?;
            let value = *attr_numbers(attr)?.get(index)?;
            Some(widen(value, kind, kind.is_unsigned()))
        };

        let (mut scale, mut offset) = (1.0, 0.0);
        let mut use_scale_offset = false;
        let (mut scale_kind, mut offset_kind) = (None, None);
        if let Some(attr) = find_attr(&var.attrs, "scale_factor") {
            if let Some(v) = first(attr, 0) {
                scale = v;
                scale_kind = attribute_kind(attr);
                use_scale_offset = true;
            }
        }
        if let Some(attr) = find_attr(&var.attrs, "add_offset") {
            if let Some(v) = first(attr, 0) {
                offset = v;
                offset_kind = attribute_kind(attr);
                use_scale_offset = true;
            }
        }
        let apply = |v: f64| {
            if use_scale_offset {
                scale * v + offset
            } else {
                v
            }
        };

        let (mut has_valid_range, mut has_valid_min, mut has_valid_max) = (false, false, false);
        let (mut valid_min, mut valid_max) = (-f64::MAX, f64::MAX);
        let mut valid_kind = None;
        if let Some(attr) = find_attr(&var.attrs, "valid_range") {
            if let (Some(a), Some(b)) = (first(attr, 0), first(attr, 1)) {
                valid_min = a;
                valid_max = b;
                valid_kind = attribute_kind(attr);
                has_valid_range = true;
            }
        }
        if !has_valid_range {
            if let Some(attr) = find_attr(&var.attrs, "valid_min") {
                if let Some(v) = first(attr, 0) {
                    valid_min = v;
                    valid_kind = attribute_kind(attr);
                    has_valid_min = true;
                }
            }
            if let Some(attr) = find_attr(&var.attrs, "valid_max") {
                if let Some(v) = first(attr, 0) {
                    valid_max = v;
                    valid_kind = largest(valid_kind, attribute_kind(attr));
                    has_valid_max = true;
                }
            }
            if has_valid_min && has_valid_max {
                has_valid_range = true;
            }
        }

        let mut has_fill = false;
        let mut fill = 0.0;
        match find_attr(&var.attrs, "_FillValue").and_then(|a| first(a, 0)) {
            Some(v) => {
                fill = apply(v);
                has_fill = true;
            }
            None => {
                if is_netcdf {
                    fill = apply(converted.default_fill());
                    has_fill = true;
                }
            }
        }

        let mut has_missing = false;
        let mut missing = Vec::new();
        if let Some(attr) = find_attr(&var.attrs, "missing_value") {
            if let Some(text) = attr_string(attr) {
                // Numeric text, as NetCDF-Java tries. Its Double.parseDouble is not ported in full.
                if let Ok(v) = text.trim().parse::<f64>() {
                    missing.push(v);
                    has_missing = true;
                }
            } else if let Some(values) = attr_numbers(attr) {
                let kind = attribute_kind(attr);
                for v in values {
                    let v = widen(
                        v,
                        kind.unwrap_or(Kind::Double),
                        kind.is_some_and(Kind::is_unsigned),
                    );
                    missing.push(apply(v));
                }
                has_missing = missing.iter().any(|m| !m.is_nan());
            }
        }

        let mut scaled = converted;
        if use_scale_offset {
            let wide = largest(largest(Some(converted), scale_kind), offset_kind);
            scaled = wide.unwrap_or(converted).with_unsigned(unsigned);
            if has_valid_range || has_valid_min || has_valid_max {
                let widest_scale = largest(scale_kind, offset_kind);
                let stored_unpacked = valid_kind.is_some()
                    && valid_kind == widest_scale
                    && valid_kind > Some(converted);
                if !stored_unpacked {
                    if has_valid_range || has_valid_min {
                        valid_min = apply(valid_min);
                    }
                    if has_valid_range || has_valid_max {
                        valid_max = apply(valid_max);
                    }
                }
                if valid_min > valid_max {
                    std::mem::swap(&mut valid_min, &mut valid_max);
                }
            }
        }
        Ok(Enhance {
            orig,
            unsigned,
            converted,
            use_scale_offset,
            scale,
            offset,
            scaled,
            has_valid_range,
            has_valid_min,
            has_valid_max,
            valid_min,
            valid_max,
            has_fill,
            fill,
            has_missing,
            missing,
        })
    }

    fn is_missing(&self, value: f64) -> bool {
        if value.is_nan() {
            return true;
        }
        if self.has_missing
            && self
                .missing
                .iter()
                .any(|m| nearly_equals_f64(value, *m, MAX_RELATIVE_DIFF_FLOAT))
        {
            return true;
        }
        if self.has_fill && nearly_equals_f64(value, self.fill, MAX_RELATIVE_DIFF_FLOAT) {
            return true;
        }
        let at_least = nearly_equals_f64(value, self.valid_min, MAX_RELATIVE_DIFF_FLOAT)
            || value > self.valid_min;
        let at_most = nearly_equals_f64(value, self.valid_max, MAX_RELATIVE_DIFF_FLOAT)
            || value < self.valid_max;
        (self.has_valid_range && !(at_least && at_most))
            || (self.has_valid_min && !at_least)
            || (self.has_valid_max && !at_most)
    }

    /// The type of the enhanced array, `convert`'s `outType`.
    fn out_kind(&self) -> Kind {
        if self.use_scale_offset {
            self.scaled
        } else if self.unsigned {
            self.converted
        } else {
            self.orig
        }
    }

    /// One stored value, as a `double`, to the enhanced value as a `double` in `outType`.
    /// `convert` then `setObjectNext`: `Number.floatValue`, `intValue` and so on.
    fn convert(&self, stored: f64) -> f64 {
        let widened = widen(
            stored,
            self.orig.with_unsigned(self.unsigned),
            self.unsigned,
        );
        let value = if self.use_scale_offset {
            self.scale * widened + self.offset
        } else {
            widened
        };
        let out = self.out_kind();
        match out {
            Kind::Float | Kind::Double => {
                if self.is_missing(value) {
                    f64::NAN
                } else if out == Kind::Float {
                    f64::from(value as f32)
                } else {
                    value
                }
            }
            // Number.byteValue and friends, then reading back through the array's signedness.
            Kind::Byte => f64::from(value as i32 as i8),
            Kind::UByte => f64::from(value as i32 as i8 as u8),
            Kind::Short => f64::from(value as i32 as i16),
            Kind::UShort => f64::from(value as i32 as i16 as u16),
            Kind::Int => f64::from(value as i32),
            Kind::UInt => f64::from(value as i32 as u32),
            Kind::Long => (value as i64) as f64,
            Kind::ULong => (value as i64 as u64) as f64,
        }
    }

    /// Cell for `(float[]) array.get1DJavaArray(FLOAT)`.
    pub fn to_f32(&self, stored: f64) -> f32 {
        self.convert(stored) as f32
    }

    /// Value for `(double[]) array.get1DJavaArray(DOUBLE)`, as axes are read.
    pub fn to_f64(&self, stored: f64) -> f64 {
        self.convert(stored)
    }

    /// Bounds are read with `ConvertMissing` removed.
    pub fn to_f64_keep_missing(&self, stored: f64) -> f64 {
        let widened = widen(
            stored,
            self.orig.with_unsigned(self.unsigned),
            self.unsigned,
        );
        let value = if self.use_scale_offset {
            self.scale * widened + self.offset
        } else {
            widened
        };
        match self.out_kind() {
            Kind::Float => f64::from(value as f32),
            Kind::Double => value,
            _ => self.convert(stored),
        }
    }
}

/// `EnhanceScaleMissingUnsignedImpl.largestOf`: `Kind`'s order is `rank`'s.
fn largest(a: Option<Kind>, b: Option<Kind>) -> Option<Kind> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// `Math.round(double)`: half up, saturating, NaN to 0.
pub(super) fn java_round(x: f64) -> i64 {
    if x.is_nan() {
        return 0;
    }
    if x.abs() >= 4.5e15 {
        return x as i64;
    }
    let floor = x.floor();
    (if x - floor >= 0.5 { floor + 1.0 } else { floor }) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_round_goes_half_up() {
        assert_eq!(java_round(2.5), 3);
        assert_eq!(java_round(-2.5), -2);
        assert_eq!(java_round(0.49999999999999994), 0);
        assert_eq!(java_round(f64::NAN), 0);
        assert_eq!(java_round(1e30), i64::MAX);
    }

    #[test]
    fn kinds_order_as_rank_does() {
        assert!(Kind::Byte < Kind::UByte && Kind::UInt < Kind::Long && Kind::ULong < Kind::Float);
        assert_eq!(
            largest(Some(Kind::Short), Some(Kind::Float)),
            Some(Kind::Float)
        );
    }
}

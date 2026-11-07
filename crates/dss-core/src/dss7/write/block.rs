//! One regular time-series block as HEC lays it out on disk: first/last valid values,
//! the values1 aggregate with repeat compression, and the packed internal header.
//!
//! Ports (paths relative to `heclib/heclib_c/src/`, `hec-dss` tag `7-IU-15`):
//! - `Internal/ztsFirstLastValidValues.c:ztsFirstLastValidFloat/Double()`;
//! - `Internal/ztsAggregate.c:ztsAggregate()` lines 145-741 (files from 7-HA on; the
//!   deprecated `ztsAggregateDep` layout is not written, P20 §1);
//! - `Internal/ztsRegRepeatCompress.c:ztsRegRepeatCompress()`;
//! - `Internal/ztsInternalHeaderPack.c:ztsInternalHeaderPack()` with
//!   `Utilities/stringHeaderUtility.c:appendStringToHeader()` and
//!   `Internal/zcharTo8Byte.c:zcharTo8Byte()`.
//!
//! Everything is in int32 words, as in HEC; a double is two words, low word first.

use crate::timeseries::is_hec_missing;

/// TS internal header indices (`headers/zStructAllocation.h` lines 87-104).
pub(crate) mod ih {
    pub const TIME_GRANULARITY: usize = 0;
    pub const PRECISION: usize = 1;
    pub const TIME_OFFSET: usize = 2;
    pub const PROFILE_DEPTHS: usize = 3;
    pub const BLOCK_START: usize = 4;
    pub const BLOCK_END: usize = 5;
    pub const VALUES_NUMBER: usize = 6;
    pub const VALUE_SIZE: usize = 7;
    pub const VALUE_ELEMENT_SIZE: usize = 8;
    pub const VALUES_COMPRESSION: usize = 9;
    pub const QUALITY_NUMBER: usize = 10;
    pub const QUALITY_ELEMENT_SIZE: usize = 11;
    pub const QUALITY_COMPRESSION: usize = 12;
    pub const INOTES_NUMBER: usize = 13;
    pub const INOTES_ELEMENT_SIZE: usize = 14;
    pub const INOTES_COMPRESSION: usize = 15;
    pub const CNOTES_LENGTH: usize = 16;
    pub const UNITS: usize = 17;
    /// Words before the strings (`INT_HEAD_units + 1`).
    pub const FIXED: usize = 18;
}

/// Largest string area of an internal header HEC reads back: `ztsInternalHeaderUnpack`
/// copies at most 200 bytes (dss7.md §7.4). The writer refuses longer strings.
pub const MAX_HEADER_STRING_BYTES: usize = 200;

/// Block values in stored width.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockValues {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl BlockValues {
    pub fn len(&self) -> usize {
        match self {
            BlockValues::F32(v) => v.len(),
            BlockValues::F64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Words per value (`valueElementSize`): 1 float, 2 double.
    pub fn element_size(&self) -> usize {
        match self {
            BlockValues::F32(_) => 1,
            BlockValues::F64(_) => 2,
        }
    }

    pub fn is_missing(&self, i: usize) -> bool {
        match self {
            BlockValues::F32(v) => is_hec_missing(f64::from(v[i])),
            BlockValues::F64(v) => is_hec_missing(v[i]),
        }
    }

    /// Values as int32 words (a double is low word, high word).
    pub fn words(&self, from: usize, n: usize) -> Vec<i32> {
        match self {
            BlockValues::F32(v) => v[from..from + n]
                .iter()
                .map(|x| x.to_bits() as i32)
                .collect(),
            BlockValues::F64(v) => v[from..from + n]
                .iter()
                .flat_map(|x| {
                    let b = x.to_bits();
                    [b as u32 as i32, (b >> 32) as u32 as i32]
                })
                .collect(),
        }
    }
}

/// `ztsFirstLastValidFloat/Double()` without profiles or notes. A position is valid when
/// its value is not missing (`zisMissingFloat`: −901, −902, −FLT_MAX; NaN is a value) or
/// its quality is non-zero. Returns `(all_missing, first, last)`; when every position is
/// missing HEC sets both positions to 0 and returns true (`ztsFirstLastValidValues.c`
/// lines 189-193).
pub fn first_last_valid(values: &BlockValues, quality: Option<&[i32]>) -> (bool, usize, usize) {
    let valid = |i: usize| !values.is_missing(i) || quality.is_some_and(|q| q[i] != 0);
    let n = values.len();
    match (0..n).find(|&i| valid(i)) {
        None => (true, 0, 0),
        Some(first) => {
            let last = (0..n).rev().find(|&i| valid(i)).unwrap_or(first);
            (false, first, last)
        }
    }
}

/// Output of `ztsAggregate()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    pub values1: Vec<i32>,
    pub header2: Vec<i32>,
}

fn repeats(words: &[i32], n: usize, size: usize) -> usize {
    (1..n)
        .filter(|&i| words[i * size..(i + 1) * size] == words[(i - 1) * size..i * size])
        .count()
}

/// `ztsRegRepeatCompress()`: keep the first value and every value that differs from its
/// predecessor; set bit `i % 32` of word `i / 32` of `header2` for each repeat.
fn repeat_compress(words: &[i32], n: usize, size: usize, out: &mut Vec<i32>, header2: &mut [i32]) {
    out.extend_from_slice(&words[..size]);
    for i in 1..n {
        if words[i * size..(i + 1) * size] == words[(i - 1) * size..i * size] {
            header2[i / 32] |= 1i32.wrapping_shl((i % 32) as u32);
        } else {
            out.extend_from_slice(&words[i * size..(i + 1) * size]);
        }
    }
}

fn pad_even(v: &mut Vec<i32>) {
    if v.len() % 2 == 1 {
        v.push(0);
    }
}

/// `ztsAggregate()` (7-HA layout) for a regular block without times or notes. Sets
/// internal-header words 6..=16 as HEC does.
pub fn aggregate(
    n: usize,
    values: &[i32],
    value_element_size: usize,
    quality: Option<&[i32]>,
    ih_words: &mut [i32],
) -> Aggregate {
    aggregate_with_times(n, None, values, value_element_size, quality, ih_words)
}

/// `ztsAggregate()` (7-HA layout) without notes, with the irregular time array
/// (`boolUseTimes`, P21): the `n` times lead values1 uncompressed ("We never compress
/// time array", `ztsWriteBlock()` line 178; `ztsAggregate()` lines 377-382, 491-495 and
/// 598-603), and the first even-padding follows times and values together (lines 397,
/// 501 and 630). Times never take part in the compression decision.
pub fn aggregate_with_times(
    n: usize,
    times: Option<&[i32]>,
    values: &[i32],
    value_element_size: usize,
    quality: Option<&[i32]>,
    ih_words: &mut [i32],
) -> Aggregate {
    let qes = usize::from(quality.is_some());
    ih_words[ih::VALUE_SIZE] = value_element_size as i32;
    ih_words[ih::VALUES_COMPRESSION] = 0;
    ih_words[ih::QUALITY_ELEMENT_SIZE] = qes as i32;
    ih_words[ih::QUALITY_COMPRESSION] = 0;
    ih_words[ih::INOTES_ELEMENT_SIZE] = 0;
    ih_words[ih::INOTES_COMPRESSION] = 0;
    ih_words[ih::CNOTES_LENGTH] = 0;
    ih_words[ih::INOTES_NUMBER] = 0;

    let (mut compress_values, mut compress_quality) = (false, false);
    let (mut value_repeats, mut quality_repeats) = (0usize, 0usize);
    // Lines 289-365: no compression for 12 values or fewer (profiles never reach here).
    if n > 12 {
        value_repeats = repeats(values, n, value_element_size);
        if let Some(q) = quality {
            quality_repeats = repeats(q, n, 1);
        }
        // `ratio` is a C float compared with the double constants 0.05 and 0.10.
        let ratio = |r: usize| f64::from(r as f32 / n as f32);
        compress_values = ratio(value_repeats) > 0.05;
        if quality.is_some() {
            compress_quality = ratio(quality_repeats) > 0.10;
        }
    }

    let mut out: Vec<i32> = Vec::new();
    if let Some(t) = times {
        out.extend_from_slice(&t[..n]);
    }
    if !compress_values && !compress_quality {
        // Lines 368-465.
        out.extend_from_slice(&values[..n * value_element_size]);
        ih_words[ih::VALUES_NUMBER] = (n * value_element_size) as i32;
        pad_even(&mut out);
        match quality {
            Some(q) => {
                ih_words[ih::QUALITY_NUMBER] = n as i32;
                out.extend_from_slice(&q[..n]);
                pad_even(&mut out);
            }
            None => ih_words[ih::QUALITY_NUMBER] = 0,
        }
        return Aggregate {
            values1: out,
            header2: Vec::new(),
        };
    }

    // Lines 471-518: every value repeats and there is no quality -> flag 2.
    if value_repeats + 1 == n && quality.is_none() {
        ih_words[ih::VALUE_SIZE] = value_element_size as i32;
        ih_words[ih::VALUES_COMPRESSION] = 2;
        ih_words[ih::VALUES_NUMBER] = value_element_size as i32;
        ih_words[ih::QUALITY_NUMBER] = 0;
        ih_words[ih::INOTES_NUMBER] = 0;
        out.extend_from_slice(&values[..value_element_size]);
        pad_even(&mut out);
        return Aggregate {
            values1: out,
            header2: Vec::new(),
        };
    }

    // Lines 531-741: full compression.
    let chn = (n - 1) / 32 + 1;
    let header2_count = (usize::from(compress_values) + usize::from(compress_quality)) * chn;
    let mut header2 = vec![0i32; header2_count];
    let mut ipos_comp = 0usize;

    let values_number = if compress_values {
        if value_repeats + 1 == n {
            ih_words[ih::VALUES_COMPRESSION] = 2;
            value_element_size
        } else {
            ih_words[ih::VALUES_COMPRESSION] = 1;
            (n - value_repeats) * value_element_size
        }
    } else {
        n * value_element_size
    };
    ih_words[ih::VALUES_NUMBER] = values_number as i32;
    let quality_number = match quality {
        Some(_) if compress_quality => {
            if quality_repeats + 1 == n {
                ih_words[ih::QUALITY_COMPRESSION] = 2;
                1
            } else {
                ih_words[ih::QUALITY_COMPRESSION] = 1;
                n - quality_repeats
            }
        }
        Some(_) => n,
        None => 0,
    };
    ih_words[ih::QUALITY_NUMBER] = quality_number as i32;

    if compress_values {
        if ih_words[ih::VALUES_COMPRESSION] == 1 {
            repeat_compress(
                values,
                n,
                value_element_size,
                &mut out,
                &mut header2[ipos_comp..ipos_comp + chn],
            );
            ipos_comp += chn;
        } else {
            out.extend_from_slice(&values[..value_element_size]);
        }
    } else {
        out.extend_from_slice(&values[..n * value_element_size]);
    }
    pad_even(&mut out);
    if let Some(q) = quality {
        if compress_quality {
            if ih_words[ih::QUALITY_COMPRESSION] == 1 {
                repeat_compress(q, n, 1, &mut out, &mut header2[ipos_comp..ipos_comp + chn]);
            } else {
                out.push(q[0]);
            }
        } else {
            out.extend_from_slice(&q[..n]);
        }
        pad_even(&mut out);
    }
    Aggregate {
        values1: out,
        header2,
    }
}

/// `zcharTo8Byte()` + `appendStringToHeader()`: the string with trailing blanks trimmed,
/// blank-padded and NUL-terminated to a whole number of 8-byte words. `None` gives seven
/// blanks and a NUL, as HEC writes for a null string.
pub fn header_string(s: Option<&[u8]>, out: &mut Vec<u8>) {
    let s = s.unwrap_or(b"");
    let len = s.iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let words = len / 8 + 1;
    out.extend_from_slice(&s[..len]);
    out.resize(out.len() + (words * 8 - 1 - len), b' ');
    out.push(0);
}

/// Packed string area for `units`, `data_type`, `time_zone` (`ztsInternalHeaderPack()`
/// lines 72-97). The caller checks it against [`MAX_HEADER_STRING_BYTES`].
pub fn header_strings(units: &[u8], data_type: &[u8], time_zone: Option<&[u8]>) -> Vec<u8> {
    let mut c = Vec::with_capacity(48);
    header_string(Some(units), &mut c);
    header_string(Some(data_type), &mut c);
    header_string(time_zone, &mut c);
    c
}

/// Complete the internal header (`ztsInternalHeaderPack()` lines 95-104): int 17 is four
/// blanks, the strings start at int 18. Returns the words actually stored.
pub fn pack_internal_header(fixed: &[i32; ih::FIXED], strings: &[u8]) -> Vec<i32> {
    let mut w = fixed.to_vec();
    w[ih::UNITS] = i32::from_le_bytes(*b"    ");
    for c in strings.chunks(4) {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        w.push(i32::from_le_bytes(b));
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fwords(v: &[f32]) -> Vec<i32> {
        BlockValues::F32(v.to_vec()).words(0, v.len())
    }

    #[test]
    fn strings_match_hec_padding() {
        let mut c = Vec::new();
        header_string(Some(b"cfs"), &mut c);
        assert_eq!(c, b"cfs    \0");
        c.clear();
        header_string(Some(b"INST-VAL"), &mut c);
        assert_eq!(c, b"INST-VAL       \0");
        c.clear();
        header_string(Some(b"1234567"), &mut c);
        assert_eq!(c, b"1234567\0");
        c.clear();
        header_string(None, &mut c);
        assert_eq!(c, b"       \0");
        c.clear();
        header_string(Some(b"cfs   "), &mut c);
        assert_eq!(c, b"cfs    \0");
        // A HEC-written record: 26 internal-header ints for cfs / INST-VAL / no zone.
        let s = header_strings(b"cfs", b"INST-VAL", None);
        let w = pack_internal_header(&[0; ih::FIXED], &s);
        assert_eq!(w.len(), 26);
        assert_eq!(&w[17..20], &[538_976_288, 544_433_763, 2_105_376]);
    }

    #[test]
    fn aggregate_small_uncompressed() {
        let mut h = [0i32; ih::FIXED];
        let a = aggregate(5, &fwords(&[1.0, 2.0, 3.0, 4.0, 5.0]), 1, None, &mut h);
        assert_eq!(a.values1.len(), 6);
        assert!(a.header2.is_empty());
        assert_eq!(h[ih::VALUES_NUMBER], 5);
        // 12 identical values are still not compressed.
        let a = aggregate(12, &fwords(&[7.0; 12]), 1, None, &mut h);
        assert_eq!(a.values1.len(), 12);
        assert_eq!(h[ih::VALUES_COMPRESSION], 0);
    }

    #[test]
    fn aggregate_all_repeat_and_bits() {
        let mut h = [0i32; ih::FIXED];
        let a = aggregate(13, &fwords(&[7.0; 13]), 1, None, &mut h);
        assert_eq!(h[ih::VALUES_COMPRESSION], 2);
        assert_eq!(h[ih::VALUES_NUMBER], 1);
        assert_eq!(a.values1.len(), 2);
        let mut v = vec![1.0f32; 40];
        v[0] = 0.0;
        v[39] = 2.0;
        let a = aggregate(40, &fwords(&v), 1, None, &mut h);
        assert_eq!(h[ih::VALUES_COMPRESSION], 1);
        assert_eq!(h[ih::VALUES_NUMBER], 3);
        assert_eq!(a.header2.len(), 2);
        // Repeats at 2..=38: bits 2..31 of word 0 and bits 0..6 of word 1.
        assert_eq!(a.header2[0] as u32, 0xFFFF_FFFC);
        assert_eq!(a.header2[1], 0x7F);
        // Quality forces the values bits to be kept and no flag 2.
        let q = vec![3i32; 13];
        let a = aggregate(13, &fwords(&[7.0; 13]), 1, Some(&q), &mut h);
        assert_eq!(h[ih::VALUES_COMPRESSION], 2);
        assert_eq!(h[ih::QUALITY_COMPRESSION], 2);
        assert_eq!(a.header2.len(), 2);
        assert_eq!(a.values1, vec![fwords(&[7.0])[0], 0, 3, 0]);
    }

    #[test]
    fn first_last() {
        let m = -f32::MAX;
        let v = BlockValues::F32(vec![m, -901.0, 1.0, f32::NAN, m]);
        assert_eq!(first_last_valid(&v, None), (false, 2, 3));
        let v = BlockValues::F32(vec![m, m]);
        assert_eq!(first_last_valid(&v, None), (true, 0, 0));
        assert_eq!(first_last_valid(&v, Some(&[0, 5])), (false, 1, 1));
    }

    #[test]
    fn aggregate_with_times_leads_and_pads_together() {
        let mut h = [0i32; ih::FIXED];
        // 3 times + 3 floats = 6 ints: no pad; with quality, 3 more + 1 pad.
        let t = [60, 120, 180];
        let a = aggregate_with_times(3, Some(&t), &fwords(&[1.0, 2.0, 3.0]), 1, None, &mut h);
        assert_eq!(&a.values1[..3], &t);
        assert_eq!(a.values1.len(), 6);
        assert_eq!(h[ih::VALUES_NUMBER], 3);
        let a = aggregate_with_times(
            3,
            Some(&t),
            &fwords(&[1.0, 2.0, 3.0]),
            1,
            Some(&[1, 2, 3]),
            &mut h,
        );
        assert_eq!(a.values1.len(), 10);
        // 13 times + flag 2: times, one value, pad to even.
        let t13: Vec<i32> = (1..=13).map(|i| i * 60).collect();
        let a = aggregate_with_times(13, Some(&t13), &fwords(&[7.0; 13]), 1, None, &mut h);
        assert_eq!(h[ih::VALUES_COMPRESSION], 2);
        assert_eq!(a.values1.len(), 14);
        assert_eq!(&a.values1[..13], &t13[..]);
    }
}

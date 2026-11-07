//! DSS6 regular time-series data compression (record type 100, `KICOMP` 1..5).
//!
//! Decoder for HEC's `DUREAL` family (dss6.md §8). Sources, all `heclib_f/src/Support/`:
//! - `dureal.f:DUREAL` (driver: delta or sig-digits first, then repeat expansion),
//! - `dhinfo.f:DHINFO` (header fields), `dhchek.f:DHCHEK` (section letters `C D S R E`),
//! - `durept.f:DUREPT` (repeat flags), `dudltr.f:DUDLTR` (delta), `dusd3r.f:DUSD3R` and
//!   `sd32r.f:SD32R` (3-significant-digit codes), `bits.f:BITS` (bit numbering),
//! - caller: `heclib_f/src/zrrtsb6.f:ZRRTSB6` lines 76-121.
//!
//! The compression header and data area are logical words (record pads already removed);
//! both are read as byte streams in file byte order (`GETHOL`).

use std::fmt;

/// Why a compressed block could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressError {
    /// A scheme the caller did not enable: schemes 4 and 5 (sig-digits) with
    /// `allow_sig_digits = false`. They are absent from the corpus (P02); P33 verified
    /// them bit for bit against heclib on HEC-written data.
    UnsupportedScheme(i32),
    /// The header is inconsistent (`DHCHEK`/`DHINFO` status, or out-of-range fields).
    BadHeader(String),
    /// The data area is shorter than the header says.
    Truncated {
        needed_bytes: usize,
        have_bytes: usize,
    },
}

impl fmt::Display for CompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompressError::UnsupportedScheme(s) => {
                write!(f, "DSS6 compression scheme {s} is not supported")
            }
            CompressError::BadHeader(d) => write!(f, "bad DSS6 compression header: {d}"),
            CompressError::Truncated {
                needed_bytes,
                have_bytes,
            } => write!(
                f,
                "compressed data has {have_bytes} bytes, header needs {needed_bytes}"
            ),
        }
    }
}

impl std::error::Error for CompressError {}

/// Fields of a compression header (`DHINFO`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressionHeader {
    /// `ISKM` bit mask: 1 repeat, 2 delta, 4 sig-digits.
    pub scheme: i32,
    /// `NELMS`: uncompressed value count.
    pub nelms: usize,
    /// `NR2`: stored (non-repeated) value count.
    pub stored: usize,
    pub repeat: bool,
    pub delta: bool,
    pub sig_digits: bool,
    /// Delta: bytes per value, decimal exponent, base, caller-supplied-base flag.
    pub delta_size: usize,
    pub delta_prec: i32,
    pub base: f32,
    pub base_given: bool,
    /// Byte offset (0-based) of the repeat flag bits in the header.
    pub repeat_bits_at: usize,
}

fn byte(h: &[u8], pos1: usize) -> Result<u8, CompressError> {
    // 1-based byte position, as GETHOL uses.
    h.get(pos1.wrapping_sub(1))
        .copied()
        .ok_or_else(|| CompressError::BadHeader(format!("header too short for byte {pos1}")))
}

fn words_to_bytes(words: &[i32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Bit `i` (1-based) of a byte stream: bit `(i-1) mod 8` (LSB = 0) of byte `(i-1)/8 + 1`
/// (`bits.f:BITS` lines 14-36).
fn bit(bytes: &[u8], i: usize) -> Option<bool> {
    let b = *bytes.get((i - 1) / 8)?;
    Some(b & (1 << ((i - 1) % 8)) != 0)
}

impl CompressionHeader {
    /// Parse and validate the header words (`DHCHEK`, `DHINFO`).
    pub fn parse(words: &[i32]) -> Result<Self, CompressError> {
        let h = words_to_bytes(words);
        if h.is_empty() {
            return Err(CompressError::BadHeader(
                "empty header (DHCHEK -1601)".into(),
            ));
        }
        // DHCHEK lines 56-67: section version 16.
        if byte(&h, 2)? != 16 {
            return Err(CompressError::BadHeader(
                "version is not 16 (DHCHEK -1602)".into(),
            ));
        }
        // DHCHEK lines 83-123: sections C, D, S, R, E chained by their lengths.
        let letters = *b"CDSRE";
        let mut start = 1usize;
        let mut starts = [0usize; 5];
        for (i, &want) in letters.iter().enumerate() {
            starts[i] = start;
            if byte(&h, start)? != want {
                return Err(CompressError::BadHeader(format!(
                    "section '{}' not at byte {start} (DHCHEK -1603)",
                    want as char
                )));
            }
            if i == 4 {
                break;
            }
            let len = if i == 3 {
                // R section length is two bytes: low at +2, high at +4 (lines 108-113).
                usize::from(byte(&h, start + 2)?) + 256 * usize::from(byte(&h, start + 4)?)
            } else {
                usize::from(byte(&h, start + 2)?)
            };
            if len == 0 {
                return Err(CompressError::BadHeader("zero section length".into()));
            }
            start += len;
        }
        // DHINFO lines 63-105: scheme mask.
        let scheme = i32::from(byte(&h, 4)?);
        if !(1..=5).contains(&scheme) {
            return Err(CompressError::BadHeader(format!(
                "scheme {scheme} (DHINFO -1701)"
            )));
        }
        let repeat = scheme & 1 != 0;
        let delta = scheme & 2 != 0;
        let sig_digits = scheme & 4 != 0;
        if delta && sig_digits {
            return Err(CompressError::BadHeader(
                "delta and sig-digits (DHINFO -1702)".into(),
            ));
        }
        // DHINFO lines 109-130: NELMS from bytes 5-8 (little-endian INTEGER*4).
        let nelms = i32::from_le_bytes([byte(&h, 5)?, byte(&h, 6)?, byte(&h, 7)?, byte(&h, 8)?]);
        if nelms <= 0 {
            return Err(CompressError::BadHeader(format!(
                "NELMS {nelms} (DHINFO -1703)"
            )));
        }
        let nelms = nelms as usize;
        // Repeat flag bits start at byte 33 (IH(9)), DHINFO line 142 / DUREAL line 174.
        let repeat_bits_at = 32;
        let mut stored = nelms;
        if repeat {
            let bits = h.get(repeat_bits_at..).unwrap_or(&[]);
            if bits.len() * 8 < nelms {
                return Err(CompressError::BadHeader(format!(
                    "repeat bits cover {} values, NELMS {nelms}",
                    bits.len() * 8
                )));
            }
            stored = (1..=nelms).filter(|&i| bit(bits, i) == Some(false)).count();
        }
        let (mut delta_size, mut delta_prec, mut base, mut base_given) =
            (0usize, 0i32, 0f32, false);
        if delta {
            // DHINFO lines 153-186: D section at byte 9.
            if byte(&h, 9)? != b'D' {
                return Err(CompressError::BadHeader(
                    "no D section (DHINFO -1704)".into(),
                ));
            }
            delta_size = usize::from(byte(&h, 12)?);
            delta_prec = i32::from(byte(&h, 13)?) - 10;
            base_given = byte(&h, 14)? != 0;
            base = f32::from_le_bytes([byte(&h, 16)?, byte(&h, 17)?, byte(&h, 18)?, byte(&h, 19)?]);
            // DUDLTR lines 37-45: ISIZE 1..6. Values wider than the INTEGER*4 work
            // variable would overflow it, so only 1..4 are meaningful.
            if !(1..=4).contains(&delta_size) {
                return Err(CompressError::BadHeader(format!(
                    "delta value size {delta_size} (DUDLTR -2001)"
                )));
            }
        }
        // Sig-digits: byte 25 (the S section's ISIZE) is not checked (P33). DHINFO line 208
        // reads it, but HEC's writer stores whatever ISIZE the caller passed: `dhinit.f`
        // line 74 `PUTHOL(IH, 25, ISIZE)` (its -1802 status for ISIZE /= 2 is overwritten
        // by `dcreal.f` before it is tested), and `zsrtsi6.f` lines 825-832 set that
        // argument (NBYTES) only for schemes 2/3, so HEC-written scheme 4/5 headers carry
        // 0, or 2 left over from an earlier delta write. HEC's reader ignores the value:
        // `dureal.f` line 66 `IBBYTE = IBGELM*ISIZE - ISIZE + 1` is 1 for the `IBGELM = 1`
        // that `zrrtsb6.f` lines 93/104 pass, and `dusd3r.f` always unpacks 2-byte codes.
        Ok(Self {
            scheme,
            nelms,
            stored,
            repeat,
            delta,
            sig_digits,
            delta_size,
            delta_prec,
            base,
            base_given,
            repeat_bits_at,
        })
    }
}

/// `SD32R`: one 3-significant-digit code to a value. `None` for codes HEC leaves
/// undefined (below 1024 other than 1001..=1005).
pub fn sd32r(code: u16) -> Option<f32> {
    let i4 = i32::from(code);
    match i4 {
        1001 => Some(0.0),
        1002 => Some(1.0e-37),
        1003 => Some(-1.0e-37),
        1004 => Some(1.0e37),
        1005 => Some(-1.0e37),
        c if c > 1023 => {
            let mut e = c / 1024;
            let neg = e > 32;
            if neg {
                e -= 32;
            }
            let mant = (c % 1024) as f32;
            // sd32r.f line 45: RA * 10.0**IP with an INTEGER exponent (REAL**INTEGER).
            let r = mant * pow10_int(e - 15);
            Some(if neg { -r } else { r })
        }
        _ => None,
    }
}

/// `10.0**IP` for an integer exponent, rounded once to `REAL*4`.
fn pow10_int(p: i32) -> f32 {
    10f64.powi(p) as f32
}

/// `10.**REAL(IPREC)` in `DUDLTR` line 137, rounded once to `REAL*4`. For the exponents
/// HEC writes (`|IPREC| < 7`) this is the correctly rounded `f32`, which is what a
/// correctly rounded `powf` returns.
fn delta_quantum(prec: i32) -> f32 {
    10f64.powi(prec) as f32
}

/// Decode a compressed block (`DUREAL`). `header` is the `KINCHE` words at `KIACHE`,
/// `data` the `KINDAT` words at `KIADAT`, `allow_sig_digits` enables schemes 4/5
/// (verified against heclib's `zrrtsx` on HEC-written data in P33). Returns `NELMS` values; HEC's -901 markers are produced as stored.
pub fn decode(
    header: &[i32],
    data: &[i32],
    allow_sig_digits: bool,
) -> Result<(CompressionHeader, Vec<f32>), CompressError> {
    let ch = CompressionHeader::parse(header)?;
    if ch.sig_digits && !allow_sig_digits {
        return Err(CompressError::UnsupportedScheme(ch.scheme));
    }
    let bytes = words_to_bytes(data);
    let n = ch.stored;
    // Step 2 of dss6.md §8: unpack NR2 stored values.
    let stored: Vec<f32> = if ch.delta {
        let size = ch.delta_size;
        let need = n * size;
        if bytes.len() < need {
            return Err(CompressError::Truncated {
                needed_bytes: need,
                have_bytes: bytes.len(),
            });
        }
        let quantum = delta_quantum(ch.delta_prec);
        // DUDLTR lines 140-141: IEXP = 2**(8*ISIZE) - 1 in INTEGER*4 (wraps for 4).
        let all_ones = ((1i64 << (8 * size)) - 1) as i32;
        (0..n)
            .map(|i| {
                let mut b = [0u8; 4];
                b[..size].copy_from_slice(&bytes[i * size..i * size + size]);
                let v = i32::from_le_bytes(b);
                if v == all_ones {
                    -901.0
                } else {
                    // DUDLTR line 138: BASE + I2TMP*REXP, INTEGER converted to REAL.
                    ch.base + (v as f32) * quantum
                }
            })
            .collect()
    } else if ch.sig_digits {
        let need = n * 2;
        if bytes.len() < need {
            return Err(CompressError::Truncated {
                needed_bytes: need,
                have_bytes: bytes.len(),
            });
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let code = u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]);
            out.push(sd32r(code).ok_or_else(|| {
                CompressError::BadHeader(format!("undefined sig-digit code {code}"))
            })?);
        }
        out
    } else {
        // Repeat only: NR2 raw REAL*4 words.
        if data.len() < n {
            return Err(CompressError::Truncated {
                needed_bytes: n * 4,
                have_bytes: data.len() * 4,
            });
        }
        data[..n]
            .iter()
            .map(|&w| f32::from_bits(w as u32))
            .collect()
    };
    if !ch.repeat {
        return Ok((ch, stored));
    }
    // DUREPT lines 116-132: bit i set means "same as value i-1".
    let hb = words_to_bytes(header);
    let bits = &hb[ch.repeat_bits_at..];
    let mut out = Vec::with_capacity(ch.nelms);
    let mut next = stored.iter();
    let first = *next
        .next()
        .ok_or_else(|| CompressError::BadHeader("no stored values (DUREPT -2201)".into()))?;
    out.push(first);
    for i in 2..=ch.nelms {
        let v = if bit(bits, i) == Some(true) {
            out[i - 2]
        } else {
            *next.next().ok_or_else(|| {
                CompressError::BadHeader("repeat flags need more stored values".into())
            })?
        };
        out.push(v);
    }
    Ok((ch, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a compression header like `DHINIT`/`DCREAL` write (dss6.md §8 table).
    pub(crate) fn header(
        scheme: u8,
        nelms: i32,
        delta: Option<(u8, i8, f32)>,
        flags: &[bool],
    ) -> Vec<i32> {
        let nw = if scheme & 1 != 0 {
            (nelms as usize).div_ceil(32)
        } else {
            0
        };
        let mut b = vec![0u8; 36 + 4 * nw];
        b[0] = b'C';
        b[1] = 16;
        b[2] = 8;
        b[3] = scheme;
        b[4..8].copy_from_slice(&nelms.to_le_bytes());
        b[8] = b'D';
        b[9] = 16;
        b[10] = 13;
        if let Some((size, prec, base)) = delta {
            b[11] = size;
            b[12] = (i16::from(prec) + 10) as u8;
            b[14] = 4;
            b[15..19].copy_from_slice(&base.to_le_bytes());
        }
        b[21] = b'S';
        b[22] = 16;
        b[23] = 4;
        b[24] = 2;
        b[25] = b'R';
        b[26] = 16;
        let rlen = 8 + 4 * nw;
        b[27] = (rlen % 256) as u8;
        b[28] = (nelms % 32) as u8;
        b[29] = (rlen / 256) as u8;
        for (i, &f) in flags.iter().enumerate() {
            if f {
                b[32 + i / 8] |= 1 << (i % 8);
            }
        }
        b[33 + 4 * nw] = b'E';
        b.chunks(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn repeat_only() {
        let flags = [false, true, true, false, true];
        let h = header(1, 5, None, &flags);
        let data = [1.5f32.to_bits() as i32, 2.0f32.to_bits() as i32];
        let (ch, v) = decode(&h, &data, false).unwrap();
        assert_eq!(ch.stored, 2);
        assert_eq!(v, vec![1.5, 1.5, 1.5, 2.0, 2.0]);
    }

    #[test]
    fn delta_and_missing() {
        let h = header(2, 3, Some((1, -1, 10.0)), &[]);
        let data = [i32::from_le_bytes([0, 5, 255, 0])];
        let (_, v) = decode(&h, &data, false).unwrap();
        assert_eq!(v[0], 10.0);
        assert_eq!(v[1], 10.0 + 5.0 * 0.1f32);
        assert_eq!(v[2], -901.0);
    }

    #[test]
    fn repeat_delta() {
        let flags = [false, true, false];
        let h = header(3, 3, Some((2, 0, 0.0)), &flags);
        let data = [i32::from_le_bytes([7, 0, 0xff, 0xff])];
        let (_, v) = decode(&h, &data, false).unwrap();
        assert_eq!(v, vec![7.0, 7.0, -901.0]);
    }

    #[test]
    fn sig_digits_refused_when_disallowed() {
        let h = header(4, 1, None, &[]);
        let data = [i32::from_le_bytes([0xe9, 0x03, 0, 0])];
        assert_eq!(
            decode(&h, &data, false).unwrap_err(),
            CompressError::UnsupportedScheme(4)
        );
        let (_, v) = decode(&h, &data, true).unwrap();
        assert_eq!(v, vec![0.0]);
        assert_eq!(sd32r(16 * 1024 + 123), Some(1230.0));
        assert_eq!(sd32r((32 + 15) * 1024 + 5), Some(-5.0));
        assert_eq!(sd32r(5), None);
    }

    #[test]
    fn sig_digit_size_byte_ignored() {
        // P33: HEC-written scheme 4/5 headers carry 0 (or a stale 2) at byte 25; HEC's
        // DUREAL ignores it. Codes 1001 (0.0) and 15*1024 + 123 (123 x 10**0).
        for size in [0u8, 2, 7] {
            let mut h = header(4, 2, None, &[]);
            let mut b: Vec<u8> = h.iter().flat_map(|w| w.to_le_bytes()).collect();
            b[24] = size;
            h = b
                .chunks(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let code = (15u16 * 1024 + 123).to_le_bytes();
            let data = [i32::from_le_bytes([0xe9, 0x03, code[0], code[1]])];
            let (ch, v) = decode(&h, &data, true).unwrap();
            assert_eq!(ch.scheme, 4);
            assert_eq!(v, vec![0.0, 123.0]);
        }
    }

    #[test]
    fn hostile_headers() {
        assert!(decode(&[], &[], false).is_err());
        let mut h = header(1, 5, None, &[false; 5]);
        h[0] = 0;
        assert!(decode(&h, &[0; 5], false).is_err());
        let h = header(1, 5, None, &[false; 5]);
        assert!(matches!(
            decode(&h, &[0; 2], false),
            Err(CompressError::Truncated { .. })
        ));
        let mut h = header(2, 5, Some((9, 0, 0.0)), &[]);
        assert!(decode(&h, &[0; 20], false).is_err());
        h[1] = i32::from_le_bytes([0, 0, 0, 0x80]); // NELMS negative
        assert!(decode(&h, &[0; 20], false).is_err());
    }
}

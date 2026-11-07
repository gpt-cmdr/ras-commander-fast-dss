//! Pathname validation for the regular (P20 §3.1, gate G3) and irregular (P21 §3.1)
//! time-series writers.
//!
//! HEC cleans pathnames silently: `Public/zpathnameClean.c:zpathnameClean()` drops bytes
//! below 32 and above 126 and everything after the seventh slash, and lookups hash only
//! the first 392 bytes (`Internal/zcheckInternal.c:zcheckInternal()` line 125). The
//! writer refuses instead, so a stored pathname is always exactly the one given (with
//! the block D part and canonical E part HEC itself substitutes).
//!
//! Paths are relative to `heclib/heclib_c/src/` of `hec-dss` tag `7-IU-15`.

use super::WriteError;
use crate::dss7::interval::{self as iv, julian_to_d_part};

/// `MAX_PART_SIZE` − 1 (`headers/zdssVals.h` line 52): longest part in bytes.
pub const MAX_PART_BYTES: usize = 128;
/// Longest pathname the writer stores: lookups truncate at 392 bytes
/// (`zcheckInternal()` line 125; `MAX_PATHNAME_LENGTH` is 393).
pub const MAX_WRITE_PATHNAME_BYTES: usize = 392;

/// Regular E parts of `eParts7` (`headers/standardIntervals.h` lines 108-118) with
/// their `secondsInInterval7` values.
const EPARTS7_REGULAR: [(&str, i64); 34] = [
    ("1Year", iv::SECS_IN_1_YEAR),
    ("1Month", iv::SECS_IN_1_MONTH),
    ("Semi-Month", iv::SECS_IN_SEMI_MONTH),
    ("Tri-Month", iv::SECS_IN_TRI_MONTH),
    ("1Week", iv::SECS_IN_1_WEEK),
    ("1Day", iv::SECS_IN_1_DAY),
    ("12Hour", 43_200),
    ("8Hour", 28_800),
    ("6Hour", 21_600),
    ("4Hour", 14_400),
    ("3Hour", 10_800),
    ("2Hour", 7_200),
    ("1Hour", 3_600),
    ("30Minute", 1_800),
    ("20Minute", 1_200),
    ("15Minute", 900),
    ("12Minute", 720),
    ("10Minute", 600),
    ("6Minute", 360),
    ("5Minute", 300),
    ("4Minute", 240),
    ("3Minute", 180),
    ("2Minute", 120),
    ("1Minute", 60),
    ("30Second", 30),
    ("20Second", 20),
    ("15Second", 15),
    ("10Second", 10),
    ("6Second", 6),
    ("5Second", 5),
    ("4Second", 4),
    ("3Second", 3),
    ("2Second", 2),
    ("1Second", 1),
];

/// Regular E parts of `eParts6` (`headers/standardIntervals.h` lines 132-139), which
/// `SemiPublic/ztsGetStandardInterval.c` also recognizes and maps to the DSS7 name.
const EPARTS6_REGULAR: [(&str, i64); 24] = [
    ("1YEAR", iv::SECS_IN_1_YEAR),
    ("1MON", iv::SECS_IN_1_MONTH),
    ("SEMI-MONTH", iv::SECS_IN_SEMI_MONTH),
    ("TRI-MONTH", iv::SECS_IN_TRI_MONTH),
    ("1WEEK", iv::SECS_IN_1_WEEK),
    ("1DAY", iv::SECS_IN_1_DAY),
    ("12HOUR", 43_200),
    ("8HOUR", 28_800),
    ("6HOUR", 21_600),
    ("4HOUR", 14_400),
    ("3HOUR", 10_800),
    ("2HOUR", 7_200),
    ("1HOUR", 3_600),
    ("30MIN", 1_800),
    ("20MIN", 1_200),
    ("15MIN", 900),
    ("12MIN", 720),
    ("10MIN", 600),
    ("6MIN", 360),
    ("5MIN", 300),
    ("4MIN", 240),
    ("3MIN", 180),
    ("2MIN", 120),
    ("1MIN", 60),
];

/// A pathname accepted for a regular time-series write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtsPathname {
    /// Parts A..F as given (D is replaced per block).
    pub parts: [Vec<u8>; 6],
    /// Interval in seconds (`secondsInInterval7`).
    pub interval_seconds: i64,
    /// Canonical DSS7 E part (`1Hour`, `15Minute`, ...).
    pub e_part: &'static str,
}

impl RtsPathname {
    /// Pathname of the block starting at `block_julian`: D = `julianToDate(…, 4)`
    /// (`ztsStoreReg7()` main loop), E = the canonical name.
    pub fn block_pathname(&self, block_julian: i64) -> Vec<u8> {
        let d = julian_to_d_part(block_julian);
        let mut v = Vec::with_capacity(96);
        v.push(b'/');
        for (i, p) in self.parts.iter().enumerate() {
            match i {
                3 => v.extend_from_slice(d.as_bytes()),
                4 => v.extend_from_slice(self.e_part.as_bytes()),
                _ => v.extend_from_slice(p),
            }
            v.push(b'/');
        }
        v
    }
}

fn bad(reason: impl Into<String>) -> WriteError {
    WriteError::BadPathname(reason.into())
}

/// Resolve an E part: exact (case-insensitive) DSS7 or DSS6 regular name only. HEC's
/// prefix rule (`ztsGetStandardInterval`, dss7.md §7.2) is not used for writes.
pub fn regular_e_part(e: &[u8]) -> Option<(i64, &'static str)> {
    let secs = EPARTS7_REGULAR
        .iter()
        .chain(EPARTS6_REGULAR.iter())
        .find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(e))
        .map(|&(_, s)| s)?;
    EPARTS7_REGULAR
        .iter()
        .find(|&&(_, s)| s == secs)
        .map(|&(n, s)| (s, n))
}

/// Checks shared by the regular and irregular writers (P20 §3.1): slashes, bytes and
/// part lengths. Returns parts A..F as given.
fn split_checked(pathname: &[u8]) -> Result<[Vec<u8>; 6], WriteError> {
    if pathname.first() != Some(&b'/') || pathname.last() != Some(&b'/') {
        return Err(bad("pathname must start and end with '/'"));
    }
    let slashes = pathname.iter().filter(|&&b| b == b'/').count();
    if slashes != 7 {
        return Err(bad(format!(
            "pathname must have exactly 7 slashes (found {slashes})"
        )));
    }
    if let Some(&b) = pathname.iter().find(|&&b| !(32..=126).contains(&b)) {
        return Err(bad(format!(
            "pathname byte 0x{b:02x} is outside printable ASCII (HEC would drop it)"
        )));
    }
    let inner = &pathname[1..pathname.len() - 1];
    let v: Vec<&[u8]> = inner.split(|&b| b == b'/').collect();
    let parts: [Vec<u8>; 6] = std::array::from_fn(|i| v[i].to_vec());
    for (i, p) in parts.iter().enumerate() {
        if p.len() > MAX_PART_BYTES {
            return Err(bad(format!(
                "part {} is {} bytes (HEC limit {MAX_PART_BYTES})",
                (b'A' + i as u8) as char,
                p.len()
            )));
        }
    }
    Ok(parts)
}

/// D and F part rules shared by both writers: D empty or a date (it is replaced per
/// block), no pattern D part, no collection F part.
fn check_d_and_f(parts: &[Vec<u8>; 6]) -> Result<(), WriteError> {
    let d = &parts[3];
    if d.len() >= 4 && d[..4].eq_ignore_ascii_case(b"TS-P") {
        return Err(bad("pattern (TS-PATTERN) records are not written"));
    }
    if !d.is_empty() && iv::parse_date(d).is_none() {
        return Err(bad(format!(
            "D part {:?} is neither empty nor a date",
            String::from_utf8_lossy(d)
        )));
    }
    if parts[5].len() >= 2 && parts[5][..2].eq_ignore_ascii_case(b"C:") {
        return Err(bad("collection pathnames (F part C:...) are not written"));
    }
    Ok(())
}

/// The longest stored block pathname (D parts are 9 bytes for years 1..9999) must fit
/// the 392 bytes lookups hash.
fn check_stored_length(stored: usize) -> Result<(), WriteError> {
    if stored > MAX_WRITE_PATHNAME_BYTES {
        return Err(bad(format!(
            "stored pathname would be {stored} bytes (limit {MAX_WRITE_PATHNAME_BYTES})"
        )));
    }
    Ok(())
}

/// Validate a pathname for a regular time-series write (P20 §3.1).
pub fn validate_rts_pathname(pathname: &[u8]) -> Result<RtsPathname, WriteError> {
    let parts = split_checked(pathname)?;
    let (interval_seconds, e_part) = regular_e_part(&parts[4]).ok_or_else(|| {
        bad(format!(
            "E part {:?} is not a standard regular interval (irregular, pseudo-regular and \
             abbreviated E parts are not written)",
            String::from_utf8_lossy(&parts[4])
        ))
    })?;
    check_d_and_f(&parts)?;
    let p = RtsPathname {
        parts,
        interval_seconds,
        e_part,
    };
    check_stored_length(p.block_pathname(iv::ymd_to_julian(2000, 1, 1)).len())?;
    Ok(p)
}

/// Validate a paired-data pathname (P23): the P20 §3.1 slash, byte and part-length
/// rules and the 392-byte lookup limit. D, E and F are free text for paired data
/// (`zpdStore()` stores the pathname as given); the case is kept.
pub fn validate_pd_pathname(pathname: &[u8]) -> Result<(), WriteError> {
    split_checked(pathname)?;
    check_stored_length(pathname.len())?;
    Ok(())
}

/// Irregular E parts of `eParts7` (`headers/standardIntervals.h` lines 117-118) with
/// their block sizes (`secondsInInterval7` stores them negated). The `eParts6` names
/// (lines 138-139) are the same letters in upper case, so the case-insensitive match
/// covers both.
const EPARTS7_IRREGULAR: [(&str, i64); 5] = [
    ("IR-Century", iv::BLOCK_1_CENTURY),
    ("IR-Decade", iv::BLOCK_1_DECADE),
    ("IR-Year", iv::BLOCK_1_YEAR),
    ("IR-Month", iv::BLOCK_1_MONTH),
    ("IR-Day", iv::BLOCK_1_DAY),
];

/// Resolve an irregular E part: exact (case-insensitive) `IR-Day` .. `IR-Century` only.
/// Pseudo-regular (`~1Day`) E parts have their own resolver ([`pseudo_e_part`], P38);
/// HEC's prefix rule is not used for writes (P21 §3.1).
pub fn irregular_e_part(e: &[u8]) -> Option<(i64, &'static str)> {
    EPARTS7_IRREGULAR
        .iter()
        .find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(e))
        .map(|&(n, b)| (b, n))
}

/// Stored names of the pseudo-regular E parts, in the order of [`EPARTS7_REGULAR`]:
/// `ztsGetStandardInterval()` keeps the `~` and standardizes the rest (`&Epart[pseudoRegular]`).
const PSEUDO_NAMES: [&str; 34] = [
    "~1Year",
    "~1Month",
    "~Semi-Month",
    "~Tri-Month",
    "~1Week",
    "~1Day",
    "~12Hour",
    "~8Hour",
    "~6Hour",
    "~4Hour",
    "~3Hour",
    "~2Hour",
    "~1Hour",
    "~30Minute",
    "~20Minute",
    "~15Minute",
    "~12Minute",
    "~10Minute",
    "~6Minute",
    "~5Minute",
    "~4Minute",
    "~3Minute",
    "~2Minute",
    "~1Minute",
    "~30Second",
    "~20Second",
    "~15Second",
    "~10Second",
    "~6Second",
    "~5Second",
    "~4Second",
    "~3Second",
    "~2Second",
    "~1Second",
];

/// Resolve a pseudo-regular E part (P38): `~` and an exact (case-insensitive) regular name.
/// The data is irregular; the block comes from the named interval
/// (`SemiPublic/ztsGetStandardInterval.c` lines 193-216, [`iv::standard_interval`]).
pub fn pseudo_e_part(e: &[u8]) -> Option<(i64, &'static str)> {
    let rest = e.strip_prefix(b"~")?;
    let (_, name) = regular_e_part(rest)?;
    let i = EPARTS7_REGULAR.iter().position(|&(n, _)| n == name)?;
    let stored = PSEUDO_NAMES[i];
    let block = -iv::standard_interval(stored.as_bytes())?.code;
    Some((block, stored))
}

/// A pathname accepted for an irregular time-series write (P21 §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItsPathname {
    /// Parts A..F as given (D is replaced per block).
    pub parts: [Vec<u8>; 6],
    /// Block size code (`BLOCK_1_DAY` 1 .. `BLOCK_1_CENTURY` 5, `ztsIrregGetBlockStart`).
    pub block_size: i64,
    /// Canonical DSS7 E part (`IR-Day`, `IR-Month`, ...).
    pub e_part: &'static str,
}

impl ItsPathname {
    /// Pathname of the block starting at `block_julian`: `ztsStoreIrreg7()` main loop
    /// (`julianToDate(julianBlockDate, 4, …)` into the D part); `ztsProcessTimes()` line
    /// 229 puts the standardized E part back.
    pub fn block_pathname(&self, block_julian: i64) -> Vec<u8> {
        let d = julian_to_d_part(block_julian);
        let mut v = Vec::with_capacity(96);
        v.push(b'/');
        for (i, p) in self.parts.iter().enumerate() {
            match i {
                3 => v.extend_from_slice(d.as_bytes()),
                4 => v.extend_from_slice(self.e_part.as_bytes()),
                _ => v.extend_from_slice(p),
            }
            v.push(b'/');
        }
        v
    }
}

/// Validate a pathname for an irregular time-series write (P21 §3.1): the P20 rules,
/// with the E part one of the irregular block names.
pub fn validate_its_pathname(pathname: &[u8]) -> Result<ItsPathname, WriteError> {
    let parts = split_checked(pathname)?;
    let resolved = irregular_e_part(&parts[4]).or_else(|| pseudo_e_part(&parts[4]));
    let (block_size, e_part) = resolved.ok_or_else(|| {
        bad(format!(
            "E part {:?} is not an irregular block (IR-Day, IR-Month, IR-Year, IR-Decade or \
             IR-Century) or a pseudo-regular interval (~1Day); abbreviated E parts are not \
             written",
            String::from_utf8_lossy(&parts[4])
        ))
    })?;
    check_d_and_f(&parts)?;
    let p = ItsPathname {
        parts,
        block_size,
        e_part,
    };
    check_stored_length(p.block_pathname(iv::ymd_to_julian(2000, 1, 1)).len())?;
    Ok(p)
}

/// Validate a pattern pathname (P38): the P20 §3.1 rules, a D part starting `TS-P` and
/// longer than 4 bytes (`ztsProcessTimes()` lines 268-272) and a standard regular or `IR-`
/// E part, which HEC standardizes (line 229). Returns the stored pathname and, for a
/// regular E part, the interval in seconds.
pub(super) fn validate_pattern_pathname(
    pathname: &[u8],
) -> Result<(Vec<u8>, Option<i64>), WriteError> {
    let parts = split_checked(pathname)?;
    let d = &parts[3];
    if d.len() <= 4 || !d[..4].eq_ignore_ascii_case(b"TS-P") {
        return Err(bad(format!(
            "D part {:?} does not name a pattern (TS-PATTERN)",
            String::from_utf8_lossy(d)
        )));
    }
    if parts[5].len() >= 2 && parts[5][..2].eq_ignore_ascii_case(b"C:") {
        return Err(bad("collection pathnames (F part C:...) are not written"));
    }
    let (e_part, regular): (&str, Option<i64>) = if let Some((s, n)) = regular_e_part(&parts[4]) {
        (n, Some(s))
    } else if let Some((_, n)) = irregular_e_part(&parts[4]) {
        (n, None)
    } else {
        return Err(bad(format!(
            "E part {:?} is not a standard regular or IR- interval",
            String::from_utf8_lossy(&parts[4])
        )));
    };
    let mut v = Vec::with_capacity(pathname.len() + 8);
    v.push(b'/');
    for (i, p) in parts.iter().enumerate() {
        if i == 4 {
            v.extend_from_slice(e_part.as_bytes());
        } else {
            v.extend_from_slice(p);
        }
        v.push(b'/');
    }
    check_stored_length(v.len())?;
    Ok((v, regular))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_canonicalizes() {
        let p = validate_rts_pathname(b"/A/B/FLOW//1HOUR/OBS/").unwrap();
        assert_eq!(p.e_part, "1Hour");
        assert_eq!(p.interval_seconds, 3600);
        assert_eq!(
            p.block_pathname(iv::ymd_to_julian(2020, 1, 1)),
            b"/A/B/FLOW/01Jan2020/1Hour/OBS/".to_vec()
        );
        assert_eq!(regular_e_part(b"1min"), Some((60, "1Minute")));
        assert_eq!(
            regular_e_part(b"1mon"),
            Some((iv::SECS_IN_1_MONTH, "1Month"))
        );
        assert!(validate_rts_pathname(b"/A/B/FLOW/01JAN2020/15Second/F/").is_ok());
    }

    #[test]
    fn refuses() {
        for p in [
            &b"A/B/C//1Hour/F/"[..],
            b"/A/B/C//1Hour/F",
            b"/A/B/C//1Hour/F/G/",
            b"/A/B/C/1Hour/F/",
            b"/A/B/C//1Hou/F/",
            b"/A/B/C//IR-Day/F/",
            b"/A/B/C//~1Day/F/",
            b"/A/B/C/TS-PATTERN/1Hour/F/",
            b"/A/B/C/notadate/1Hour/F/",
            b"/A/B/C//1Hour/C:000001|X/",
            b"/A/B\x01/C//1Hour/F/",
            b"/A/B\xe9/C//1Hour/F/",
        ] {
            assert!(
                matches!(validate_rts_pathname(p), Err(WriteError::BadPathname(_))),
                "{:?}",
                String::from_utf8_lossy(p)
            );
        }
        let long = format!("/{}/B/C//1Hour/F/", "A".repeat(129));
        assert!(validate_rts_pathname(long.as_bytes()).is_err());
        let total = format!("/{a}/{a}/{a}//1Hour/{a}/", a = "A".repeat(100));
        assert!(validate_rts_pathname(total.as_bytes()).is_err());
    }

    #[test]
    fn irregular_pathnames() {
        let p = validate_its_pathname(b"/A/B/FLOW//ir-month/OBS/").unwrap();
        assert_eq!(p.e_part, "IR-Month");
        assert_eq!(p.block_size, iv::BLOCK_1_MONTH);
        assert_eq!(
            p.block_pathname(iv::ymd_to_julian(2020, 3, 1)),
            b"/A/B/FLOW/01Mar2020/IR-Month/OBS/".to_vec()
        );
        for e in ["IR-DAY", "IR-Year", "ir-decade", "IR-CENTURY"] {
            let path = format!("/A/B/C//{e}/F/");
            assert!(validate_its_pathname(path.as_bytes()).is_ok(), "{e}");
        }
        for p in [
            &b"/A/B/C//1Hour/F/"[..],
            b"/A/B/C//IR-Mon/F/",
            b"/A/B/C//~1Da/F/",
            b"/A/B/C//IR-Months/F/",
            b"/A/B/C/TS-PATTERN/IR-Day/F/",
            b"/A/B/C//IR-Day/C:000001|X/",
            b"/A/B/C/xx/IR-Day/F/",
            b"/A/B\x7f/C//IR-Day/F/",
        ] {
            assert!(
                matches!(validate_its_pathname(p), Err(WriteError::BadPathname(_))),
                "{:?}",
                String::from_utf8_lossy(p)
            );
        }
        assert!(validate_rts_pathname(b"/A/B/C//IR-Day/F/").is_err());
    }

    #[test]
    fn pseudo_and_pattern_pathnames() {
        let p = validate_its_pathname(b"/A/B/C//~1day/F/").unwrap();
        assert_eq!(p.e_part, "~1Day");
        assert_eq!(p.block_size, iv::BLOCK_1_YEAR);
        assert_eq!(pseudo_e_part(b"~6HOUR"), Some((iv::BLOCK_1_YEAR, "~6Hour")));
        assert_eq!(
            pseudo_e_part(b"~1Month"),
            Some((iv::BLOCK_1_CENTURY, "~1Month"))
        );
        assert_eq!(
            pseudo_e_part(b"~5Minute"),
            Some((iv::BLOCK_1_DAY, "~5Minute"))
        );
        assert_eq!(pseudo_e_part(b"~IR-Day"), None);
        let (v, r) = validate_pattern_pathname(b"/A/B/C/TS-PATTERN/1HOUR/F/").unwrap();
        assert_eq!(v, b"/A/B/C/TS-PATTERN/1Hour/F/".to_vec());
        assert_eq!(r, Some(3600));
        let (v, r) = validate_pattern_pathname(b"/A/B/C/ts-pat/ir-day/F/").unwrap();
        assert_eq!(v, b"/A/B/C/ts-pat/IR-Day/F/".to_vec());
        assert_eq!(r, None);
        for b in [
            &b"/A/B/C/TS-P/1Hour/F/"[..],
            b"/A/B/C/01JAN2000/1Hour/F/",
            b"/A/B/C/TS-PATTERN/~1Day/F/",
        ] {
            assert!(validate_pattern_pathname(b).is_err());
        }
    }
}

//! DSS6 pathname validation for the writer.
//!
//! HEC upper-cases a pathname before hashing and storing it (`zcheck6.f:ZCHECK6` lines
//! 70-82), so the writer does too. Everything HEC would change some other way is refused:
//! bytes outside 32..126 (`zcheck6` turns control bytes into `?`), parts over
//! `dss_maxpart` = 128 characters and stored pathnames over `dss_maxpath` = 392
//! (`headers/dss_parameters.h`; `zwritex6.f` lines 47-48), pattern (`TS-`) D parts,
//! collections (`C:` F parts) and pseudo-regular (`~`) E parts.

use super::WriteError;
use crate::dss6::interval::{d_part, parse_d_part, BlockSpan, REGULAR_INTERVALS};

/// `dss_maxpart` (`headers/dss_parameters.h`).
pub const MAX_PART_BYTES6: usize = 128;
/// `dss_maxpath` (`headers/dss_parameters.h`).
pub const MAX_PATHNAME_BYTES6: usize = 392;
/// Units and data type are `CHARACTER*8` in the internal header (`zsrtsi6.f` lines
/// 872-875, `zsitsi6.f` lines 987-990); HEC truncates longer strings, the writer refuses.
pub const MAX_UNITS_TYPE_CHARS: usize = 8;

/// DSS7 E part names for the DSS6 regular intervals (`headers/standardIntervals.h`), so a
/// DSS7-style pathname can be written to a DSS6 file.
const DSS7_NAMES: [(&str, i32); 24] = [
    ("1Year", 525_600),
    ("1Month", 43_200),
    ("Semi-Month", 21_600),
    ("Tri-Month", 14_400),
    ("1Week", 10_080),
    ("1Day", 1_440),
    ("12Hour", 720),
    ("8Hour", 480),
    ("6Hour", 360),
    ("4Hour", 240),
    ("3Hour", 180),
    ("2Hour", 120),
    ("1Hour", 60),
    ("30Minute", 30),
    ("20Minute", 20),
    ("15Minute", 15),
    ("12Minute", 12),
    ("10Minute", 10),
    ("6Minute", 6),
    ("5Minute", 5),
    ("4Minute", 4),
    ("3Minute", 3),
    ("2Minute", 2),
    ("1Minute", 1),
];

/// Regular E part: the interval in minutes and the canonical DSS6 name (`zgintl6.f`
/// `CVALS`), for an exact case-insensitive match of a DSS6 name (`1HOUR`, `15MIN`, `1MON`)
/// or a DSS7 name (`1Hour`, `15Minute`, `1Month`). HEC matches the first four characters
/// only (`zgintl6.f` line 76); abbreviations and second intervals are refused.
pub fn regular_e_part6(e: &[u8]) -> Option<(i32, &'static str)> {
    let minutes = REGULAR_INTERVALS
        .iter()
        .find(|(_, name, _)| name.as_bytes().eq_ignore_ascii_case(e))
        .map(|&(m, _, _)| m)
        .or_else(|| {
            DSS7_NAMES
                .iter()
                .find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(e))
                .map(|&(_, m)| m)
        })?;
    REGULAR_INTERVALS
        .iter()
        .find(|&&(m, _, _)| m == minutes)
        .map(|&(m, name, _)| (m, name))
}

/// Irregular E part: block span and canonical name (`zirbeg6.f` `CINTLS`), exact
/// case-insensitive match.
pub fn irregular_e_part6(e: &[u8]) -> Option<(BlockSpan, &'static str)> {
    [
        ("IR-DAY", BlockSpan::Day),
        ("IR-MONTH", BlockSpan::Month),
        ("IR-YEAR", BlockSpan::Year),
        ("IR-DECADE", BlockSpan::Decade),
        ("IR-CENTURY", BlockSpan::Century),
    ]
    .iter()
    .find(|(n, _)| n.as_bytes().eq_ignore_ascii_case(e))
    .map(|&(n, b)| (b, n))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Regular { minutes: i64 },
    Irregular { block: BlockSpan },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Path6 {
    /// Parts A..F, upper case; D is replaced per block.
    pub parts: [Vec<u8>; 6],
    pub e_part: &'static str,
    pub kind: Kind,
    /// The interval in minutes of a pseudo-regular (`~`) E part (`KBPINTL`, `INTLPS`);
    /// 0 otherwise.
    pub pseudo_minutes: i32,
}

impl Path6 {
    /// Stored pathname of the block starting at `julian` (D part `DDMMMYYYY`, `YMDDAT`
    /// style 104, `zsrtsi6.f` line 505, `zsitsi6.f` line 476).
    pub fn block_pathname(&self, julian: i64) -> Vec<u8> {
        let d = d_part(julian);
        let mut v = Vec::with_capacity(96);
        v.push(b'/');
        for (i, p) in self.parts.iter().enumerate() {
            match i {
                3 => v.extend_from_slice(&d),
                4 => v.extend_from_slice(self.e_part.as_bytes()),
                _ => v.extend_from_slice(p),
            }
            v.push(b'/');
        }
        v
    }

    /// True when the C part starts with `ELEV` (`verticalDatum.c:pathnameIsElevTs`): HEC's
    /// stores then apply vertical-datum handling and may keep an existing user header
    /// (`zsrtsi6.f` lines 252-441, `zsitsi6.f` lines 285-453).
    pub fn is_elevation(&self) -> bool {
        self.parts[2].len() >= 4 && self.parts[2][..4].eq_ignore_ascii_case(b"ELEV")
    }
}

fn bad(reason: impl Into<String>) -> WriteError {
    WriteError::BadPathname(reason.into())
}

fn split(pathname: &[u8]) -> Result<[Vec<u8>; 6], WriteError> {
    split_as(pathname, false, false)
}

/// The shared checks; a pattern D part and a `~` E part are allowed only where the caller
/// handles them.
fn split_as(
    pathname: &[u8],
    allow_pattern: bool,
    allow_pseudo: bool,
) -> Result<[Vec<u8>; 6], WriteError> {
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
            "pathname byte 0x{b:02x} is outside printable ASCII (HEC would change it)"
        )));
    }
    let inner = &pathname[1..pathname.len() - 1];
    let v: Vec<&[u8]> = inner.split(|&b| b == b'/').collect();
    let parts: [Vec<u8>; 6] = std::array::from_fn(|i| v[i].to_ascii_uppercase());
    for (i, p) in parts.iter().enumerate() {
        if p.len() > MAX_PART_BYTES6 {
            return Err(bad(format!(
                "part {} is {} bytes (DSS6 limit {MAX_PART_BYTES6})",
                (b'A' + i as u8) as char,
                p.len()
            )));
        }
    }
    let d = &parts[3];
    let pattern = d.len() >= 3 && &d[..3] == b"TS-";
    if pattern && !allow_pattern {
        return Err(bad(
            "a pattern (TS-) D part needs a pattern write, not a dated series",
        ));
    }
    if !pattern && !d.is_empty() && parse_d_part(d).is_none() {
        return Err(bad(format!(
            "D part {:?} is neither empty nor a date (DDMMMYYYY)",
            String::from_utf8_lossy(d)
        )));
    }
    if parts[5].len() >= 2 && &parts[5][..2] == b"C:" {
        return Err(bad("collection pathnames (F part C:...) are not written"));
    }
    if parts[4].first() == Some(&b'~') && !allow_pseudo {
        return Err(bad(
            "pseudo-regular (~) E parts are not written; use the IR- E part with times",
        ));
    }
    Ok(parts)
}

fn check_length(p: &Path6) -> Result<(), WriteError> {
    let n = p.block_pathname(36_524).len(); // any 9-character D part
    if n > MAX_PATHNAME_BYTES6 {
        return Err(bad(format!(
            "stored pathname would be {n} bytes (DSS6 limit {MAX_PATHNAME_BYTES6})"
        )));
    }
    Ok(())
}

/// Validate a regular-interval pathname for a DSS6 write.
pub(super) fn validate_regular(pathname: &[u8]) -> Result<Path6, WriteError> {
    let parts = split(pathname)?;
    let (minutes, e_part) = regular_e_part6(&parts[4]).ok_or_else(|| {
        bad(format!(
            "E part {:?} is not a DSS6 regular interval (1MIN..1YEAR; second intervals, \
             irregular and abbreviated E parts are not written as regular data)",
            String::from_utf8_lossy(&parts[4])
        ))
    })?;
    let p = Path6 {
        parts,
        e_part,
        kind: Kind::Regular {
            minutes: i64::from(minutes),
        },
        pseudo_minutes: 0,
    };
    check_length(&p)?;
    Ok(p)
}

/// Validate an irregular-interval pathname for a DSS6 write.
pub(super) fn validate_irregular(pathname: &[u8]) -> Result<Path6, WriteError> {
    let parts = split_as(pathname, false, true)?;
    let (block, e_part, pseudo_minutes) = if let Some(rest) = parts[4].strip_prefix(b"~") {
        // zsitsi6.f lines 187-201, zpseudorts6.f: stored as IR-*, the interval kept in the
        // bin (KBPINTL) and the internal header (INTLPS).
        let (minutes, _) = regular_e_part6(rest).ok_or_else(|| {
            bad(format!(
                "E part {:?} is not ~ and a DSS6 regular interval",
                String::from_utf8_lossy(&parts[4])
            ))
        })?;
        let (block, name) = pseudo_block6(minutes);
        (block, name, minutes)
    } else {
        let (block, name) = irregular_e_part6(&parts[4]).ok_or_else(|| {
            bad(format!(
                "E part {:?} is not IR-DAY, IR-MONTH, IR-YEAR, IR-DECADE, IR-CENTURY or a \
                 pseudo-regular ~ interval",
                String::from_utf8_lossy(&parts[4])
            ))
        })?;
        (block, name, 0)
    };
    let p = Path6 {
        parts,
        e_part,
        kind: Kind::Irregular { block },
        pseudo_minutes,
    };
    check_length(&p)?;
    Ok(p)
}

/// `zpseudorts6.f` lines 60-72: the stored block of a pseudo-regular interval in minutes.
fn pseudo_block6(minutes: i32) -> (BlockSpan, &'static str) {
    if minutes < 15 {
        (BlockSpan::Day, "IR-DAY")
    } else if minutes < 1440 {
        (BlockSpan::Month, "IR-MONTH")
    } else if minutes < 10_080 {
        (BlockSpan::Year, "IR-YEAR")
    } else if minutes < 50_000 {
        (BlockSpan::Decade, "IR-DECADE")
    } else {
        (BlockSpan::Century, "IR-CENTURY")
    }
}

/// Validate a pattern pathname: D starts `TS-` (`zsrtsi6.f` line 231, `zsitsi6.f` line
/// 213), E a regular or `IR-` interval. Returns the stored pathname, upper case, with the
/// D part as given and the canonical E part.
pub(super) fn validate_pattern(pathname: &[u8]) -> Result<(Vec<u8>, Kind), WriteError> {
    let parts = split_as(pathname, true, false)?;
    if parts[3].len() < 3 || &parts[3][..3] != b"TS-" {
        return Err(bad(format!(
            "D part {:?} does not name a pattern (TS-PATTERN)",
            String::from_utf8_lossy(&parts[3])
        )));
    }
    let (e_part, kind): (&str, Kind) = if let Some((m, n)) = regular_e_part6(&parts[4]) {
        (
            n,
            Kind::Regular {
                minutes: i64::from(m),
            },
        )
    } else if let Some((b, n)) = irregular_e_part6(&parts[4]) {
        (n, Kind::Irregular { block: b })
    } else {
        return Err(bad(format!(
            "E part {:?} is not a DSS6 regular or IR- interval",
            String::from_utf8_lossy(&parts[4])
        )));
    };
    let mut v = Vec::with_capacity(pathname.len() + 8);
    v.push(b'/');
    for (i, p) in parts.iter().enumerate() {
        v.extend_from_slice(if i == 4 { e_part.as_bytes() } else { p });
        v.push(b'/');
    }
    if v.len() > MAX_PATHNAME_BYTES6 {
        return Err(bad(format!(
            "stored pathname would be {} bytes (DSS6 limit {MAX_PATHNAME_BYTES6})",
            v.len()
        )));
    }
    Ok((v, kind))
}

/// Units or data type: printable ASCII, at most 8 characters (stored blank-padded).
pub(super) fn check_string(what: &str, s: &[u8]) -> Result<(), WriteError> {
    if s.len() > MAX_UNITS_TYPE_CHARS {
        return Err(WriteError::BadInput(format!(
            "{what} is {} characters; DSS6 stores at most {MAX_UNITS_TYPE_CHARS}",
            s.len()
        )));
    }
    if let Some(&b) = s.iter().find(|&&b| !(32..=126).contains(&b)) {
        return Err(WriteError::BadInput(format!(
            "{what} byte 0x{b:02x} is outside printable ASCII"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_names() {
        assert_eq!(regular_e_part6(b"1hour"), Some((60, "1HOUR")));
        assert_eq!(regular_e_part6(b"15Minute"), Some((15, "15MIN")));
        assert_eq!(regular_e_part6(b"1Month"), Some((43_200, "1MON")));
        assert_eq!(regular_e_part6(b"semi-month"), Some((21_600, "SEMI-MONTH")));
        assert_eq!(regular_e_part6(b"1HOU"), None);
        assert_eq!(regular_e_part6(b"15Second"), None);
        assert_eq!(regular_e_part6(b"IR-MONTH"), None);
        assert_eq!(
            irregular_e_part6(b"ir-month"),
            Some((BlockSpan::Month, "IR-MONTH"))
        );
        assert_eq!(irregular_e_part6(b"IR-MON"), None);
    }

    #[test]
    fn validates() {
        let p = validate_regular(b"/a/b/flow//1Hour/f/").unwrap();
        assert_eq!(
            p.block_pathname(43_830),
            b"/A/B/FLOW/01JAN2020/1HOUR/F/".to_vec()
        );
        assert!(validate_regular(b"/A/B/C/01JAN2020/1HOUR/F/").is_ok());
        for b in [
            &b"A/B/C//1HOUR/F/"[..],
            b"/A/B/C//1HOUR/F",
            b"/A/B/C//1HOUR/F/G/",
            b"/A/B/C/TS-PATTERN/1HOUR/F/",
            b"/A/B/C/JAN2020/1HOUR/F/",
            b"/A/B/C//1HOUR/C:000001|X/",
            b"/A/B/C//~1HOUR/F/",
            b"/A/B/C//1SECOND/F/",
            b"/A/B/C//IR-MONTH/F/",
            b"/A/B/C\x01//1HOUR/F/",
        ] {
            assert!(
                validate_regular(b).is_err(),
                "{:?}",
                String::from_utf8_lossy(b)
            );
        }
        let long = format!("/{}/B/C//1HOUR/F/", "A".repeat(129));
        assert!(validate_regular(long.as_bytes()).is_err());
        let total = format!(
            "/{a}/{a}/{a}//1HOUR/{f}/",
            a = "A".repeat(100),
            f = "F".repeat(80)
        );
        assert!(validate_regular(total.as_bytes()).is_err());
        let p = validate_irregular(b"/A/B/ELEV//ir-year/F/").unwrap();
        assert!(p.is_elevation());
        assert_eq!(p.e_part, "IR-YEAR");
        assert!(validate_irregular(b"/A/B/C//1HOUR/F/").is_err());
        // Pseudo-regular E part.
        let p = validate_irregular(b"/A/B/C//~1hour/F/").unwrap();
        assert_eq!((p.e_part, p.pseudo_minutes), ("IR-MONTH", 60));
        let e = |b: &[u8]| validate_irregular(b).unwrap().e_part;
        assert_eq!(e(b"/A/B/C//~5MIN/F/"), "IR-DAY");
        assert_eq!(e(b"/A/B/C//~1WEEK/F/"), "IR-DECADE");
        assert_eq!(e(b"/A/B/C//~1YEAR/F/"), "IR-CENTURY");
        assert!(validate_irregular(b"/A/B/C//~1HOU/F/").is_err());
        let (v, k) = validate_pattern(b"/a/b/c/ts-pattern/1hour/f/").unwrap();
        assert_eq!(v, b"/A/B/C/TS-PATTERN/1HOUR/F/".to_vec());
        assert_eq!(k, Kind::Regular { minutes: 60 });
        assert!(validate_pattern(b"/A/B/C/01JAN2000/1HOUR/F/").is_err());
        assert!(check_string("units", b"CFS").is_ok());
        assert!(check_string("units", b"123456789").is_err());
        assert!(check_string("units", b"C\tS").is_err());
    }
}

//! DSS6 pathname helpers: part splitting and pseudo-regular time-series E parts.
//!
//! Sources:
//! - Pseudo-regular conversion: `heclib_f/src/zpseudorts6.f:ZPSEUDORTS6` lines 41-86.
//! - Interval names and minutes: `heclib_f/src/zgintl6.f:ZGINTL6` (`JVALS`, `CVALS`).
//! - Catalog display (`LPSEUDO`, default true in `zinit6.f:ZBKDAT` line 883):
//!   `heclib_f/src/zcatfi6.f:ZCATFI6` lines 128-135; `heclib_f/src/zplist6.f` lines 155-164.
//! - Lookup conversion of a `~` E part: `heclib_f/src/zcheck6.f:ZCHECK6` lines 86-97.

/// `zgintl6` regular intervals (minutes, E part), in table order. Lookups compare the
/// first 4 characters only.
const REGULAR_INTERVALS: [(i32, &str); 24] = [
    (525600, "1YEAR"),
    (43200, "1MON"),
    (21600, "SEMI-MONTH"),
    (14400, "TRI-MONTH"),
    (10080, "1WEEK"),
    (1440, "1DAY"),
    (720, "12HOUR"),
    (480, "8HOUR"),
    (360, "6HOUR"),
    (240, "4HOUR"),
    (180, "3HOUR"),
    (120, "2HOUR"),
    (60, "1HOUR"),
    (30, "30MIN"),
    (20, "20MIN"),
    (15, "15MIN"),
    (12, "12MIN"),
    (10, "10MIN"),
    (6, "6MIN"),
    (5, "5MIN"),
    (4, "4MIN"),
    (3, "3MIN"),
    (2, "2MIN"),
    (1, "1MIN"),
];

/// `zgintl6` irregular E parts (they end the `CVALS` table).
const IRREGULAR: [&str; 5] = ["IR-CENTURY", "IR-DECADE", "IR-YEAR", "IR-MONTH", "IR-DAY"];

/// Byte ranges of parts A..F, or `None` unless the path is `/A/B/C/D/E/F/` with exactly
/// seven slashes (the shape `ZUPATH` accepts for a pathname).
pub fn split_parts(path: &[u8]) -> Option<[std::ops::Range<usize>; 6]> {
    let slashes: Vec<usize> = path
        .iter()
        .enumerate()
        .filter(|(_, &c)| c == b'/')
        .map(|(i, _)| i)
        .collect();
    if slashes.len() != 7 || slashes[0] != 0 || slashes[6] != path.len() - 1 {
        return None;
    }
    Some(std::array::from_fn(|k| slashes[k] + 1..slashes[k + 1]))
}

/// Minutes to regular E part (`zgintl6` with `ISTAT = 2`).
pub fn interval_name(minutes: i32) -> Option<&'static str> {
    REGULAR_INTERVALS
        .iter()
        .find(|(m, _)| *m == minutes)
        .map(|(_, n)| *n)
}

/// Regular E part to minutes, matching the first 4 characters case-insensitively
/// (`zgintl6` with `ISTAT = 1`). Irregular names return `None`.
pub fn interval_minutes(e_part: &[u8]) -> Option<i32> {
    let key = first4_upper(e_part);
    if IRREGULAR.iter().any(|n| first4_upper(n.as_bytes()) == key) {
        return None;
    }
    REGULAR_INTERVALS
        .iter()
        .find(|(_, n)| first4_upper(n.as_bytes()) == key)
        .map(|(m, _)| *m)
}

fn first4_upper(s: &[u8]) -> [u8; 4] {
    let mut out = [b' '; 4];
    for (o, c) in out.iter_mut().zip(s) {
        *o = c.to_ascii_uppercase();
    }
    out
}

/// Catalog display path for a pseudo-regular record: the stored `IR-*` E part replaced by
/// `~<interval>` (`ZPSEUDORTS6` action 2). `None` when the path does not split into parts
/// or the interval is not a `zgintl6` regular interval, in which case HEC keeps the
/// stored path.
pub fn pseudo_display_path(stored: &[u8], interval_minutes: i32) -> Option<Vec<u8>> {
    let parts = split_parts(stored)?;
    let name = interval_name(interval_minutes)?;
    let e = &parts[4];
    let mut out = Vec::with_capacity(stored.len() + 8);
    out.extend_from_slice(&stored[..e.start]);
    out.push(b'~');
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&stored[e.end..]);
    Some(out)
}

/// Stored (irregular) form of a `~<interval>` pathname (`ZPSEUDORTS6` action 1): the E
/// part becomes `IR-DAY` (< 15 minutes), `IR-MONTH` (< 1440), `IR-YEAR` (< 10080),
/// `IR-DECADE` (< 50000) or `IR-CENTURY`. Returns `(stored_path, minutes)`, or `None`
/// when the path is not pseudo-regular.
pub fn pseudo_to_stored(path: &[u8]) -> Option<(Vec<u8>, i32)> {
    let parts = split_parts(path)?;
    let e = &path[parts[4].clone()];
    if e.first() != Some(&b'~') {
        return None;
    }
    let minutes = interval_minutes(&e[1..])?;
    let name = match minutes {
        m if m < 15 => "IR-DAY",
        m if m < 1440 => "IR-MONTH",
        m if m < 10080 => "IR-YEAR",
        m if m < 50000 => "IR-DECADE",
        _ => "IR-CENTURY",
    };
    let mut out = Vec::with_capacity(path.len() + 8);
    out.extend_from_slice(&path[..parts[4].start]);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&path[parts[4].end..]);
    Some((out, minutes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_parts() {
        let p = b"/A/B/FLOW/01JAN2000/1HOUR/OBS/";
        let r = split_parts(p).unwrap();
        assert_eq!(&p[r[2].clone()], b"FLOW");
        assert_eq!(&p[r[0].clone()], b"A");
        assert!(split_parts(b"/A/B/").is_none());
        assert!(split_parts(b"").is_none());
        assert!(split_parts(b"A/B/C/D/E/F//").is_none());
    }

    #[test]
    fn pseudo_round_trip() {
        let stored = b"/A/B/FLOW/01JAN2000/IR-YEAR/OBS/";
        let shown = pseudo_display_path(stored, 1440).unwrap();
        assert_eq!(shown, b"/A/B/FLOW/01JAN2000/~1DAY/OBS/".to_vec());
        let (back, m) = pseudo_to_stored(&shown).unwrap();
        assert_eq!(m, 1440);
        assert_eq!(back, stored.to_vec());
        assert_eq!(
            pseudo_to_stored(b"/A/B/C/D/~15MIN/F/").unwrap().0,
            b"/A/B/C/D/IR-MONTH/F/".to_vec()
        );
        assert!(pseudo_display_path(stored, 7).is_none());
        assert!(pseudo_to_stored(b"/A/B/C/D/1DAY/F/").is_none());
        assert!(pseudo_to_stored(b"/A/B/C/D/~IR-DAY/F/").is_none());
    }

    #[test]
    fn interval_lookup_uses_four_characters() {
        assert_eq!(interval_minutes(b"15min"), Some(15));
        assert_eq!(interval_minutes(b"1MONTH"), Some(43200));
        assert_eq!(interval_minutes(b"1MIN"), Some(1));
        assert_eq!(interval_minutes(b"IR-DAY"), None);
        assert_eq!(interval_name(60), Some("1HOUR"));
    }
}

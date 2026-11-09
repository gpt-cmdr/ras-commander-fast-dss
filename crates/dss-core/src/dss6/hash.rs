//! DSS6 pathname hashing and lookup normalization.
//!
//! Sources:
//! - `heclib_f/src/zhash6.f:ZHASH6` lines 11-80 (Pabst bit-group XOR).
//! - `heclib_f/src/zcheck6.f:ZCHECK6` lines 70-82 (upper case, control characters) and
//!   lines 174-208 (collection hash path).
//! - `heclib_f/src/zcolpath6.f:ZCOLPATH6` lines 25-39.

/// `zhash6`: hash code `1..=max_hash` of the pathname bytes, taken **unsigned**
/// (`ICHAR` of a Fortran character; all 40 Latin-1 pathnames in the files tested hash
/// this way).
///
/// `IBIT` is the smallest `i` in 9..=13 with `max_hash <= 2^i`, else 14. The bytes, plus
/// two NULs and then blanks (`CT = CPATH; CT(NPATH+1:NPATH+2) = CHAR(0)//CHAR(0)`), are an
/// MSB-first bit stream cut into `IBIT`-bit groups that are XORed together; the result is
/// `IT mod max_hash + 1`. Returns 0 when `max_hash < 1` (no valid code).
pub fn zhash6(path: &[u8], max_hash: i32) -> i32 {
    if max_hash < 1 {
        return 0;
    }
    // zhash6.f lines 18-25.
    let ibit: i64 = (9..=13)
        .find(|&i| i64::from(max_hash) <= 1i64 << i)
        .unwrap_or(14);
    let n = path.len() as i64;
    // zhash6.f lines 33-34; Fortran integer division truncates toward zero, as Rust does.
    let nt = (n * 8 - 1) / ibit + 1;
    let nt = (nt * ibit - 1) / 8 + 1;
    let byte_at = |i: i64| -> i64 {
        // 0-based index into CT.
        if i < n {
            i64::from(path[i as usize])
        } else if i < n + 2 {
            0
        } else {
            i64::from(b' ')
        }
    };
    // zhash6.f lines 37-75, transcribed. I2 never exceeds 2^(8+IBIT).
    let (mut it, mut i2, mut ineed, mut ihave, mut ibyte) = (0i64, 0i64, ibit, 0i64, 0i64);
    loop {
        if ihave > 0 {
            let imove = ineed.min(ihave);
            i2 <<= imove;
            ineed -= imove;
            ihave -= imove;
            if ineed <= 0 {
                let itp = i2 / 256;
                i2 %= 256;
                it ^= itp;
                ineed = ibit;
            }
        }
        if ihave <= 0 {
            ibyte += 1;
            if ibyte > nt {
                break;
            }
            i2 += byte_at(ibyte - 1);
            ihave = 8;
        }
    }
    (it % i64::from(max_hash)) as i32 + 1
}

/// Lookup normalization from `zcheck6.f:ZCHECK6` lines 70-82: `a`-`z` become upper case
/// and bytes below 32 become `?`. Bytes 0x80 and above are left as they are.
pub fn normalize_pathname(path: &[u8]) -> Vec<u8> {
    path.iter()
        .map(|&c| match c {
            0..=31 => b'?',
            b'a'..=b'z' => c - 32,
            _ => c,
        })
        .collect()
}

/// The path that is hashed for a stored pathname (`zcolpath6.f:ZCOLPATH6` lines 25-39).
///
/// A collection path has `|` exactly 9 characters after the start of `/C:` (1-based
/// `IBAR = ICOLL + 9`, both found by `INDEX`, i.e. first occurrence). The 6 characters
/// after `/C:` become `XXXXXX` and the result is upper-cased. Any other path is returned
/// unchanged.
pub fn hash_path(path: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let Some(ibar) = path.iter().position(|&c| c == b'|') else {
        return path.into();
    };
    let Some(icoll) = path.windows(3).position(|w| w == b"/C:") else {
        return path.into();
    };
    if ibar != icoll + 9 {
        return path.into();
    }
    let mut out = path.to_vec();
    out[icoll + 3..ibar].copy_from_slice(b"XXXXXX");
    // UPCASE: Fortran upper-casing of a-z only.
    for c in &mut out {
        c.make_ascii_uppercase();
    }
    out.into()
}

/// Hash code a pathname is stored under: `zhash6` of the collection-mapped path
/// (`zcheck6.f` lines 174-208).
pub fn pathname_hash(path: &[u8], max_hash: i32) -> i32 {
    zhash6(&hash_path(path), max_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_stored_hashes_from_public_files() {
        // Stored KBHASH values from HMS 4.3 castro.dss (KHASH 128), RasExamples
        // Muncie.dss and precip.2018.09.dss (KHASH 2048).
        let cases: [(&[u8], i32, i32); 6] = [
            (b"/CASTRO VALLEY/OUTLET/FLOW/16JAN1973/10MIN/OBS/", 128, 118),
            (
                b"/CASTRO VALLEY/FIRE DEPT./PRECIP-INC/16JAN1973/10MIN/OBS/",
                128,
                70,
            ),
            (b"//TABLE 1/STORAGE-FLOW///TABLE/", 128, 99),
            (
                b"/WHITE MUNCIE/13214 LAT STRUCT/FLOW-TOTAL/01JAN1900/5MIN/2D 50FT GRID/",
                2048,
                1019,
            ),
            (
                b"/WHITE MUNCIE/13214 LAT STRUCT/FLOW-WEIR/01JAN1900/5MIN/2D 50FT GRID/",
                2048,
                2016,
            ),
            (
                b"/SHG/MARFC/PRECIP/10SEP2018:0600/10SEP2018:1200/QPF-QPF/",
                2048,
                496,
            ),
        ];
        for (p, max, want) in cases {
            assert_eq!(zhash6(p, max), want, "{}", String::from_utf8_lossy(p));
        }
    }

    #[test]
    fn range_and_edge_cases() {
        for max in [1, 8, 32, 128, 512, 1024, 2048, 4096, 8192, 16348, i32::MAX] {
            for p in [&b""[..], b"/", b"/A/B/C/D/E/F/", &[0xFFu8; 392][..]] {
                let h = zhash6(p, max);
                assert!((1..=max).contains(&h));
            }
        }
        assert_eq!(zhash6(b"/A/", 0), 0);
        assert_eq!(zhash6(b"/A/", -3), 0);
    }

    #[test]
    fn normalization_and_collections() {
        assert_eq!(normalize_pathname(b"/a/b\x01/c/"), b"/A/B?/C/".to_vec());
        assert_eq!(normalize_pathname(&[0xCD]), vec![0xCD]);
        let coll = b"/YUBA/SMARTSVILLE/FLOW/01JAN1997/1HOUR/C:000042|OPERATION a/";
        assert_eq!(
            hash_path(coll).as_ref(),
            b"/YUBA/SMARTSVILLE/FLOW/01JAN1997/1HOUR/C:XXXXXX|OPERATION A/"
        );
        let plain = b"/A/B/C/D/E/F|G/";
        assert_eq!(hash_path(plain).as_ref(), plain);
    }
}

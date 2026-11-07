//! DSS7 pathname hashes, `docs/format/dss7.md` §3.1–§3.2.
//!
//! Source: `heclib_c/src/Internal/zhash.c:zhash()`.
//!
//! `zhash()` reads each pathname byte into an `int` from a (signed, on MSVC and x86 gcc)
//! `char`. Any value below 32 becomes `'?'`, which on signed-`char` platforms includes
//! every byte >= 0x80; everything else goes through `toupper()`. We implement that
//! signed-`char` behaviour: bytes < 32 and >= 0x80 hash as `'?'`, ASCII letters are
//! upper-cased. The P02 census found no DSS7 pathname with a byte >= 0x80, so this branch
//! is not corpus-verified.

/// Pathnames longer than this are truncated before hashing and lookup
/// (`Internal/zcheckInternal.c:zcheckInternal()`).
pub const MAX_HASHED_PATH: usize = 392;

/// Normalize one pathname byte the way `zhash()` fills its `path[]` buffer.
#[inline]
fn hash_byte(b: u8) -> u8 {
    // `ich = pathname[i]` on a signed char: bytes >= 0x80 are negative, so `ich < 32`.
    if !(32..0x80).contains(&b) {
        b'?'
    } else {
        b.to_ascii_uppercase()
    }
}

fn normalized(pathname: &[u8]) -> Vec<u8> {
    let n = pathname.len().min(MAX_HASHED_PATH);
    let mut p: Vec<u8> = pathname[..n].iter().map(|&b| hash_byte(b)).collect();
    // zhash() zeroes the 8 bytes after the path; the table hash may read up to 3 of them.
    p.extend_from_slice(&[0u8; 8]);
    p
}

/// Pathname ("bin") hash: Java `String.hashCode` widened to 64 bits; 0 becomes 1.
pub fn pathname_hash(pathname: &[u8]) -> i64 {
    let p = normalized(pathname);
    let n = pathname.len().min(MAX_HASHED_PATH);
    let mut h: i64 = 0;
    for &c in &p[..n] {
        h = h.wrapping_mul(31).wrapping_add(i64::from(c));
    }
    if h == 0 {
        1
    } else {
        h
    }
}

/// `ibit` from `frexp(maxHash)`: the exponent, minus one for exact powers of two.
fn ibit(max_hash: i64) -> i32 {
    if max_hash <= 0 {
        return 1;
    }
    // frexp(x) = f * 2^e with 0.5 <= f < 1, so e = floor(log2 x) + 1.
    let e = 64 - (max_hash as u64).leading_zeros() as i32;
    let pow2 = (max_hash & (max_hash - 1)) == 0;
    if pow2 {
        e - 1
    } else {
        e
    }
}

/// Table hash: XOR of successive `ibit`-bit groups of the upper-cased pathname, modulo
/// `kmaxHash`. Returns a value in `0..max_hash` for any input this module produces (all
/// normalized bytes are < 0x80, so `it` is never negative).
pub fn table_hash(pathname: &[u8], max_hash: i64) -> i64 {
    let max_hash = max_hash.max(1);
    let p = normalized(pathname);
    let n = pathname.len().min(MAX_HASHED_PATH) as i64;
    let ibit = i64::from(ibit(max_hash)).max(1);
    let mut nt = ((n * 8 - 1) / ibit) + 1;
    nt = ((nt * ibit - 1) / 8) + 1;
    let (mut it, mut i2): (i64, i64) = (0, 0);
    let (mut ineed, mut ihave, mut ibyte) = (ibit, 0i64, 0i64);
    loop {
        if ihave > 0 {
            let imove = ineed.min(ihave);
            i2 <<= imove;
            ineed -= imove;
            ihave -= imove;
            if ineed <= 0 {
                // C int division truncates toward zero; i2 is never negative here.
                let itp = i2 / 256;
                i2 %= 256;
                it ^= itp;
                ineed = ibit;
            }
        }
        if ihave <= 0 {
            ibyte += 1;
            if ibyte > nt || n == 0 {
                break;
            }
            i2 += i64::from(*p.get((ibyte - 1) as usize).unwrap_or(&0));
            ihave = 8;
        }
    }
    it % max_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ibit_matches_frexp() {
        assert_eq!(ibit(8192), 13);
        assert_eq!(ibit(4096), 12);
        assert_eq!(ibit(16348), 14);
        assert_eq!(ibit(64000), 16);
        assert_eq!(ibit(1), 0);
    }

    #[test]
    fn hash_is_case_insensitive() {
        let a = b"/A/B/FLOW/01Jan2020/1Hour/x/";
        let b = b"/a/b/flow/01JAN2020/1HOUR/X/";
        assert_eq!(pathname_hash(a), pathname_hash(b));
        assert_eq!(table_hash(a, 8192), table_hash(b, 8192));
    }

    #[test]
    fn table_hash_in_range_for_odd_sizes() {
        for mh in [1i64, 3, 100, 4096, 8192, 16348, 64000] {
            let t = table_hash(b"//SACRAMENTO/PRECIP-INC/01Jan1877/1Day/OBS/", mh);
            assert!((0..mh).contains(&t));
        }
        assert_eq!(table_hash(b"", 8192), 0);
    }
}

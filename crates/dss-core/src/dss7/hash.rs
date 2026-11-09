//! DSS7 pathname hashes, `docs/format/dss7.md` §3.1–§3.2.
//!
//! Source: `heclib_c/src/Internal/zhash.c:zhash()`.
//!
//! `zhash()` reads each pathname byte into an `int` from a (signed, on MSVC and x86 gcc)
//! `char`. Any value below 32 becomes `'?'`, which on signed-`char` platforms includes
//! every byte >= 0x80; everything else goes through `toupper()`. We implement that
//! signed-`char` behaviour: bytes < 32 and >= 0x80 hash as `'?'`, ASCII letters are
//! upper-cased. No DSS7 pathname with a byte >= 0x80 was found in the files tested, so this
//! branch is not corpus-verified.

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

/// The normalized path plus the 8 zero bytes `zhash()` appends (the table hash may read
/// up to 3 of them), written into a caller buffer so a catalog walk does not allocate
/// per entry.
fn normalized_into<'a>(pathname: &[u8], buf: &'a mut [u8; MAX_HASHED_PATH + 8]) -> &'a [u8] {
    let n = pathname.len().min(MAX_HASHED_PATH);
    for (d, &b) in buf.iter_mut().zip(&pathname[..n]) {
        *d = hash_byte(b);
    }
    buf[n..n + 8].fill(0);
    &buf[..n + 8]
}

/// Earlier allocating form, kept as the reference for the differential tests.
#[cfg(test)]
fn normalized(pathname: &[u8]) -> Vec<u8> {
    let n = pathname.len().min(MAX_HASHED_PATH);
    let mut p: Vec<u8> = pathname[..n].iter().map(|&b| hash_byte(b)).collect();
    // zhash() zeroes the 8 bytes after the path; the table hash may read up to 3 of them.
    p.extend_from_slice(&[0u8; 8]);
    p
}

/// Pathname ("bin") hash: Java `String.hashCode` widened to 64 bits; 0 becomes 1.
pub fn pathname_hash(pathname: &[u8]) -> i64 {
    let n = pathname.len().min(MAX_HASHED_PATH);
    let p = &pathname[..n];
    // `h = 31 * h + c` per byte, taken four bytes at a time:
    // `h = 31^4 * h + (31^3 c0 + 31^2 c1 + 31 c2 + c3)`. Wrapping arithmetic is exact
    // modulo 2^64, so this equals the byte-at-a-time loop (normalized bytes are
    // below 128, so the inner sum cannot overflow). No allocation.
    const P2: i64 = 31 * 31;
    const P3: i64 = 31 * 31 * 31;
    const P4: i64 = 31 * 31 * 31 * 31;
    let c = |b: u8| i64::from(hash_byte(b));
    let mut h: i64 = 0;
    let mut quads = p.chunks_exact(4);
    for q in &mut quads {
        let v = c(q[0]) * P3 + c(q[1]) * P2 + c(q[2]) * 31 + c(q[3]);
        h = h.wrapping_mul(P4).wrapping_add(v);
    }
    for &b in quads.remainder() {
        h = h.wrapping_mul(31).wrapping_add(c(b));
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
    let mut buf = [0u8; MAX_HASHED_PATH + 8];
    let p = normalized_into(pathname, &mut buf);
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

    /// Earlier `pathname_hash`, the reference for [`hashes_match_reference`].
    fn pathname_hash_reference(pathname: &[u8]) -> i64 {
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

    #[test]
    fn hashes_match_reference() {
        // Random bytes over the whole range (control, ASCII, >= 0x80), lengths across the
        // 392-byte truncation, and table sizes including odd and power-of-two ones. The
        // table hash changed only in where its normalized bytes live, so the buffer is
        // compared with the allocating normalization directly.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let sizes = [1i64, 2, 3, 100, 4096, 8192, 16348, 64000, 1 << 20];
        for case in 0..20_000 {
            let len = match case % 4 {
                0 => (rnd() % 8) as usize,
                1 => (rnd() % 120) as usize,
                2 => 385 + (rnd() % 16) as usize,
                _ => (rnd() % 500) as usize,
            };
            let p: Vec<u8> = (0..len)
                .map(|_| match rnd() % 4 {
                    0 => (rnd() % 256) as u8,
                    _ => b"/ABCabc019-_. "[(rnd() % 14) as usize],
                })
                .collect();
            assert_eq!(pathname_hash(&p), pathname_hash_reference(&p), "{p:?}");
            let mut buf = [0u8; MAX_HASHED_PATH + 8];
            assert_eq!(normalized_into(&p, &mut buf), normalized(&p).as_slice());
            let mh = sizes[case % sizes.len()];
            let t = table_hash(&p, mh);
            assert!((0..mh).contains(&t));
        }
        // A pathname whose hash is 0 before the 0 -> 1 rule.
        assert_eq!(pathname_hash(b""), 1);
        assert_eq!(pathname_hash_reference(b""), 1);
    }

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

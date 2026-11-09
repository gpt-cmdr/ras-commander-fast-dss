//! `decode_utf8_replacing` against Java's own UTF-8 decoder with replacement, black-box
//! (the clean-room reimplementation).
//!
//! The pinned hashes were computed with Java's
//! `new String(bytes, StandardCharsets.UTF_8)` on the same byte strings; Java 17.0.11+9
//! (the HMS 4.13 JRE Monolith runs on) and 17.0.20.1+1 print the same values. Each hash
//! is FNV-1a 64 over every string's UTF-16 code units (little-endian) followed by FF FF.

use dss_core::decode_utf8_replacing;

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn eat(&mut self, b: u8) {
        self.0 ^= u64::from(b);
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
    }
    fn string(&mut self, b: &[u8]) {
        for u in decode_utf8_replacing(b).encode_utf16() {
            self.eat(u as u8);
            self.eat((u >> 8) as u8);
        }
        self.eat(0xFF);
        self.eat(0xFF);
    }
}

/// Every byte string of length 1-3 over all 256 byte values (16,843,008 strings).
#[test]
fn every_string_up_to_three_bytes_equals_java() {
    let mut h = Fnv::new();
    let mut n = 0u64;
    for len in 1..=3u32 {
        let mut b = vec![0u8; len as usize];
        for v in 0..1u32 << (8 * len) {
            for (i, x) in b.iter_mut().enumerate() {
                *x = (v >> (8 * (len as usize - 1 - i))) as u8;
            }
            h.string(&b);
            n += 1;
        }
    }
    assert_eq!(n, 16_843_008);
    assert_eq!(format!("{:016x}", h.0), "4869132e7673c6e5");
}

/// 1,000,000 seeded random strings of length 1-24, bytes half from a UTF-8 edge alphabet.
#[test]
fn seeded_random_strings_equal_java() {
    const EDGE: [u8; 24] = [
        0x00, 0x41, 0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0, 0xC1, 0xC2, 0xDF, 0xE0, 0xE1,
        0xEC, 0xED, 0xEE, 0xEF, 0xF0, 0xF1, 0xF4, 0xF5, 0xFF,
    ];
    let mut state: u64 = 0x5059;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut h = Fnv::new();
    for _ in 0..1_000_000 {
        let len = 1 + (next() % 24) as usize;
        let b: Vec<u8> = (0..len)
            .map(|_| {
                let r = next();
                if r & 1 == 0 {
                    EDGE[((r >> 1) % EDGE.len() as u64) as usize]
                } else {
                    (r >> 8) as u8
                }
            })
            .collect();
        h.string(&b);
    }
    assert_eq!(format!("{:016x}", h.0), "e1c85c6d1bda7443");
}

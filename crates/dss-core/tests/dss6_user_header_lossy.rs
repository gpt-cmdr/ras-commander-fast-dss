//! DSS6 user-header text that is invalid UTF-8 (`docs/format/dss6.md` §14.2).
//!
//! Rule: decode as UTF-8. Text that is neither standard nor Java
//! modified UTF-8 decodes as UTF-8 with each malformed sequence replaced by one U+FFFD,
//! exactly as Java's own decoder does (`new String(bytes, StandardCharsets.UTF_8)`,
//! `dss_core::decode_utf8_replacing`); the encoding is `"utf-8-replaced"`. Earlier engines
//! decoded such text as whole-text Latin-1.
//!
//! `fixtures/public/synthetic_userheader_lossy6.dss` was written by HEC Monolith 3.3.27
//! through heclib `ZSRTSX` (`Heclib.zsrtsx`, which stores the IUHEAD words as given). It is
//! not regenerated here;
//! its SHA-256 is checked by `tests/python/test_user_header_lossy.py`. Every header ends
//! in a NUL inside the stored words, so Monolith does not over-read.
//!
//! The expected engine texts are Java 17.0.11 `new String(text, UTF_8)` on the text
//! bytes (after the NUL cut and the trailing-blank trim). The Monolith texts are
//! `TimeSeriesContainer.supplementalInfo` from a fresh `HecTimeSeries` read of a fresh
//! copy (hash unchanged): HotSpot `NewStringUTF` never gives U+FFFD and drops characters
//! (see `fixture_text_equals_monolith`), so they are pinned in an ignored test only.

use dss_core::dss6::Dss6File;
use dss_core::location::Location;
use dss_core::user_header::{user_header_bytes, UserHeaderEncoding};
use dss_core::{decode_user_header_text, decode_utf8_replacing};
use std::path::PathBuf;

fn open() -> Dss6File<dss_core::dss6::FileSource> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/public/synthetic_userheader_lossy6.dss");
    assert!(p.exists(), "public fixture missing: {}", p.display());
    Dss6File::open(p).expect("open fixture")
}

fn path(case: &str) -> String {
    format!("/P31/{case}/FLOW/01JAN2020/1DAY/SYNTH/")
}

/// (case, stored text bytes before the NUL, engine text, Monolith UTF-16 code units).
#[allow(clippy::type_complexity)]
const CASES: &[(&str, &[u8], &str, &[u16])] = &[
    ("LONE-CONT", b"A\x80B", "A\u{fffd}B", &[0x41, 0x80]),
    ("LONE-CONT-END", b"ABC\xbf", "ABC\u{fffd}", &[0x41, 0x42, 0x43]),
    ("TRUNC2", b"A\xc3B", "A\u{fffd}B", &[0x41, 0xC3, 0x42]),
    ("TRUNC3", b"A\xe2\x82B", "A\u{fffd}B", &[0x41, 0xE2, 0x82]),
    ("TRUNC4", b"A\xf0\x9f\x8cB", "A\u{fffd}B", &[0x41, 0xF0, 0x9F]),
    ("OVERLONG2", b"A\xc1\x81B", "A\u{fffd}\u{fffd}B", &[0x41, 0x41, 0x42]),
    (
        "OVERLONG2-C0AF",
        b"A\xc0\xafB",
        "A\u{fffd}\u{fffd}B",
        &[0x41, 0x2F, 0x42],
    ),
    (
        "OVERLONG3",
        b"A\xe0\x80\xafB",
        "A\u{fffd}\u{fffd}\u{fffd}B",
        &[0x41, 0x2F, 0x42],
    ),
    (
        "OVERLONG4",
        b"A\xf0\x80\x80\xafB",
        "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B",
        &[0x41, 0xF0, 0x80],
    ),
    // Java: one U+FFFD for an encoded surrogate (from_utf8_lossy would give three).
    ("SURROGATE", b"A\xed\xa0\x80B", "A\u{fffd}B", &[0x41, 0xD800, 0x42]),
    (
        "SURROGATE-LOW",
        b"A\xed\xb0\x80B",
        "A\u{fffd}B",
        &[0x41, 0xDC00, 0x42],
    ),
    (
        "ABOVE-10FFFF",
        b"A\xf4\x90\x80\x80B",
        "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B",
        &[0x41, 0xF4, 0x90],
    ),
    (
        "F5-FF",
        b"A\xf5\xf6\xf7\xf8\xf9\xfa\xfb\xfc\xfd\xfe\xffB",
        "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}B",
        &[
            0x41, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFB, 0xFC, 0xFD, 0xFE, 0xFF, 0x42,
        ],
    ),
    (
        "F5-SEQ",
        b"A\xf5\x80\x80\x80B",
        "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B",
        &[0x41, 0xF5, 0x80],
    ),
    // Invalid bytes between valid multi-byte text: the valid characters are kept.
    (
        "MIXED",
        b"N: \xc3\xbd\xe9\xe2\x82\xac\x80\xf0\x9f\x8c\x8a;Y: \xc3\xbd",
        "N: \u{fd}\u{fffd}\u{20ac}\u{fffd}\u{1f30a};Y: \u{fd}",
        &[
            0x4E, 0x3A, 0x20, 0xFD, 0xE9, 0x20AC, 0x80, 0xF0, 0x9F, 0x8C, 0x8A, 0x3B,
        ],
    ),
    (
        "MIXED-LATIN1",
        b"Caf\xe9; Y\xc3\xbd",
        "Caf\u{fffd}; Y\u{fd}",
        &[0x43, 0x61, 0x66, 0xE9, 0x3B, 0x20, 0x59, 0xFD],
    ),
    // Modified-UTF-8 forms in text that is invalid in both forms: Java does not join them.
    (
        "MUTF8-PAIR-INVALID",
        b"A\xed\xa0\xbc\xed\xbc\x8a \xe9",
        "A\u{fffd}\u{fffd} \u{fffd}",
        &[0x41, 0xD83C, 0xDF0A, 0x20, 0xE9],
    ),
    (
        "C080-INVALID",
        b"A\xc0\x80B\xe9",
        "A\u{fffd}\u{fffd}B\u{fffd}",
        &[0x41, 0x00, 0x42, 0xE9],
    ),
    // An invalid sequence at the NUL cut (the bytes after the NUL are not text).
    ("NUL-CUT", b"Ab\xc3", "Ab\u{fffd}", &[0x41, 0x62, 0xC3]),
    ("NUL-CUT3", b"Ab\xe2\x82", "Ab\u{fffd}", &[0x41, 0x62, 0xE2]),
    // An invalid sequence at the trailing-blank trim: the trim runs first.
    (
        "TRIM",
        b"Ab\xe2\x82   ",
        "Ab\u{fffd}",
        &[0x41, 0x62, 0xE2, 0x82, 0x20, 0x20],
    ),
    (
        "TRIM-CONT",
        b"Ab\xe2\x82\xac\x80  ",
        "Ab\u{20ac}\u{fffd}",
        &[0x41, 0x62, 0x20AC, 0x80, 0x20],
    ),
    // Valid 4-byte UTF-8 (control case: standard UTF-8, step 1).
    ("VALID4", b"A\xf0\x9f\x8c\x8aB", "A\u{1f30a}B", &[0x41, 0xF0, 0x9F]),
    (
        "CONT-RUN",
        b"A\x80\x80\x80\x80BCDE",
        "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}BCDE",
        &[0x41, 0x80, 0x80, 0x80, 0x80],
    ),
];

#[test]
fn fixture_text_follows_java() {
    let f = open();
    for &(case, stored, want, _) in CASES {
        let r = f.read_ts_record(path(case).as_bytes()).unwrap();
        // The raw bytes are the stored text, then a NUL. Without a NUL in the text the
        // writer added one and NUL padding to a whole word; the NUL-CUT texts keep bytes
        // after their NUL.
        let raw = user_header_bytes(&r.user_header);
        assert_eq!(&raw[..stored.len()], stored, "{case}");
        assert_eq!(raw[stored.len()], 0, "{case}");
        let after: &[u8] = match case {
            "NUL-CUT" => b"\xbd tail",
            "NUL-CUT3" => b"\xac",
            _ => b"",
        };
        let full = [stored, b"\0", after].concat();
        assert_eq!(&raw[..full.len()], &full[..], "{case}");
        assert!(raw[full.len()..].iter().all(|&b| b == 0), "{case}");
        assert_eq!(r.user_header.len(), full.len().div_ceil(4), "{case}");
        assert_eq!(r.user_header_text(), want, "{case}");
        let enc = if case == "VALID4" {
            UserHeaderEncoding::Utf8
        } else {
            UserHeaderEncoding::Utf8Replaced
        };
        assert_eq!(r.user_header_encoding(), Some(enc), "{case}");
        // The location supplemental text uses the same decode; its bytes are unchanged.
        let l = Location::from_dss6_ts(&r.pathname, Some([1.0, 2.0, 0.0]), None)
            .unwrap()
            .with_dss6_user_header(&r.user_header);
        assert_eq!(l.supplemental_text().as_deref(), Some(want), "{case}");
        assert_eq!(l.supplemental_encoding(), Some(enc.as_str()), "{case}");
        let trimmed = stored.trim_ascii_end();
        assert_eq!(l.supplemental.as_deref(), Some(trimmed), "{case}");
        assert_eq!(decode_user_header_text(trimmed), (want.to_string(), enc));
    }
}

/// Monolith reads DSS6 user-header text with JNI `NewStringUTF`;
/// HotSpot (`utf8.cpp` `UTF8::next`, `UTF8::unicode_length`) turns each bad byte into its
/// Latin-1 character, never U+FFFD, and sizes the string by its non-continuation bytes,
/// so it drops characters at the end, and it also splits valid 4-byte UTF-8. The engine
/// follows the maintainer's "use UTF-8" decision instead. Compared after removing
/// Monolith's trailing blanks, which the engine trims (`docs/format/dss6.md` §14.2).
#[test]
#[ignore = "Monolith's NewStringUTF gives Latin-1 per bad byte, no U+FFFD, and truncates; the engine does not copy that loss"]
fn fixture_text_equals_monolith() {
    let f = open();
    for &(case, _, _, mono) in CASES {
        let r = f.read_ts_record(path(case).as_bytes()).unwrap();
        let m = String::from_utf16_lossy(mono);
        assert_eq!(r.user_header_text(), m.trim_end_matches(' '), "{case}");
    }
}

/// Recorded Monolith output for the public fixture; expectations are data, not a JNI model.
#[test]
fn monolith_texts_match_recorded_run() {
    let recorded = include_str!("data/p59b_monolith.tsv");
    let rows: Vec<_> = recorded.lines().skip(1).collect();
    assert_eq!(rows.len(), 24);
    assert_eq!(rows.len(), CASES.len());
    for (row, &(case, _, _, mono)) in rows.iter().zip(CASES) {
        let (name, units) = row.split_once('\t').unwrap();
        let units: Vec<u16> = units
            .split_whitespace()
            .map(|unit| u16::from_str_radix(unit, 16).unwrap())
            .collect();
        assert_eq!(name, case);
        assert_eq!(units, mono, "{case}");
    }
}

/// `decode_utf8_replacing` equals Java 17 `new String(bytes, UTF_8)` on every byte string
/// of length 1-5 over 20 edge bytes (3,368,420 strings). The hash was computed with
/// Java's `new String(bytes, UTF_8)` on Java 17.0.11+9 (the
/// HMS 4.13 JRE Monolith runs on) and 17.0.20.1+1; both print the same value.
#[test]
fn java_decoder_exhaustive() {
    const ALPHABET: [u8; 20] = [
        0x41, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0, 0xC1, 0xC2, 0xDF, 0xE0, 0xE1, 0xED, 0xEF,
        0xF0, 0xF1, 0xF4, 0xF5, 0xFF,
    ];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    let mut n = 0u64;
    let mut differ_from_lossy = 0u64;
    for len in 1..=5usize {
        let mut idx = vec![0usize; len];
        let mut b = vec![0u8; len];
        loop {
            for i in 0..len {
                b[i] = ALPHABET[idx[i]];
            }
            let s = decode_utf8_replacing(&b);
            if s != String::from_utf8_lossy(&b) {
                differ_from_lossy += 1;
                assert!(b.contains(&0xED), "{b:02x?}");
            }
            for u in s.encode_utf16() {
                eat(u as u8);
                eat((u >> 8) as u8);
            }
            eat(0xFF);
            eat(0xFF);
            n += 1;
            let mut k = len;
            loop {
                if k == 0 {
                    break;
                }
                k -= 1;
                idx[k] += 1;
                if idx[k] < ALPHABET.len() {
                    k = usize::MAX;
                    break;
                }
                idx[k] = 0;
            }
            if k != usize::MAX {
                break;
            }
        }
    }
    assert_eq!(n, 3_368_420);
    assert_eq!(format!("{h:016x}"), "9522dd440da71eb4");
    // Java departs from the maximal-subpart practice only at encoded surrogates (ED A0-BF).
    eprintln!("strings where Java != from_utf8_lossy: {differ_from_lossy}");
    assert!(differ_from_lossy > 0);
}

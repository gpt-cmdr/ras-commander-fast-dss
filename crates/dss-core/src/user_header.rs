//! Record user headers as text.
//!
//! A DSS record can carry a **user header**: an array of 4-byte words that HEC stores
//! beside the internal header and never interprets itself. In DSS6 it is the
//! `IUHEAD`/`NUHEAD` array of the time-series readers (`heclib_f/src/zrrtsi.f:ZRRTSI6`
//! lines 345-349, `heclib_f/src/zreadx6.f:ZREADX6` lines 53-58), located by info-block
//! words `KIAUHE`/`KINUHE` (dss6.md §5.1). Programs that write time series put free-form
//! `Keyword: value;` text in it; HEC-DSSVue's USGS import writes gage metadata there.
//! The text is not validated by HEC, and a datum named in it is informational only: it
//! is never merged into typed location fields.
//!
//! Text rules (docs/format/dss6.md §14):
//! - The words hold characters packed four per word in file byte order, the same packing
//!   as every other DSS6 character field (`HOLCHR`, `unpack_chars`).
//! - The text ends at the first NUL byte. For DSS6, javaHeclib passes the array to Java
//!   as a C string (`javaHeclib/src/Hec_ztsRetrieve.c:Java_hec_heclib_util_Heclib_Hec_1ztsRetrieve`,
//!   "User header (supplemental)": `headerString = (char*)tss->userHeader`); for DSS7 it
//!   uses `Utilities/verticalDatum.c:userHeaderToString`, which also stops at the first
//!   NUL. Without a NUL every stored byte is text.
//! - Trailing blanks are padding and are removed here (Fortran `HOLCHR` blank-fills, and
//!   `userHeaderToString`'s trailing-blank loop never runs because of operator
//!   precedence). Monolith keeps them; see [`user_header_text`].
//! - Decoding (maintainer decision 2026-10-28: the text is "probably UTF-8"; then
//!   maintainer decision 2026-10-30: "use UTF-8"): the text bytes (after the
//!   NUL cut and the trailing-blank trim) are decoded as standard UTF-8 when valid; else
//!   as Java modified UTF-8 when valid (what Monolith's JNI layer writes: 6-byte
//!   surrogate pairs for characters outside the BMP, `C0 80` for U+0000); else as UTF-8
//!   with each malformed sequence replaced by one U+FFFD, exactly as Java's own decoder
//!   does (`new String(bytes, StandardCharsets.UTF_8)`; [`decode_utf8_replacing`]).
//!   Earlier versions decoded that last case as whole-text Latin-1, and before that
//!   Latin-1 was used for all text
//!   ([`decode_user_header_text`], [`UserHeaderEncoding`]).
//! - Monolith reads the text with JNI `NewStringUTF` (`javaHeclib/src/jni_utility.c`,
//!   `hec_dss_jni_setStringField`, called from `Hec_ztsRetrieve.c` for
//!   `supplementalInfo`). For invalid text HotSpot's `NewStringUTF` never gives U+FFFD:
//!   it maps each bad byte to its Latin-1 character and sizes the string by the count of
//!   non-continuation bytes, so characters at the end are dropped; it also splits valid
//!   4-byte UTF-8 (docs/format/dss6.md §14.2). The engine does not copy that
//!   loss. Other DSS6 character fields (pathnames, units, types) stay Latin-1. The raw
//!   bytes are always available ([`user_header_bytes`]).
//! - [`parse_user_header_items`] follows `hec.io.DataContainer.getSupplementalInfo()`
//!   (Monolith 3.3.27).

/// The stored user-header words as bytes in file order (four per word, little-endian
/// words, as `dss6::info::unpack_chars` unpacks character words).
pub fn user_header_bytes(words: &[i32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// The text bytes: up to the first NUL (javaHeclib `Hec_ztsRetrieve.c`, C-string use of
/// `tss->userHeader`), trailing blanks removed.
pub fn user_header_text_bytes(words: &[i32]) -> Vec<u8> {
    let mut b = user_header_bytes(words);
    if let Some(n) = b.iter().position(|&c| c == 0) {
        b.truncate(n);
    }
    while b.last() == Some(&b' ') {
        b.pop();
    }
    b
}

/// How user-header text bytes were decoded (`Latin1` was replaced with
/// `Utf8Replaced`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserHeaderEncoding {
    /// The bytes are valid standard UTF-8 (this includes pure ASCII and empty text).
    Utf8,
    /// Not standard UTF-8, but valid Java modified UTF-8 (`java.io.DataInput`, JNI
    /// `NewStringUTF`): surrogate pairs as two 3-byte sequences and U+0000 as `C0 80`.
    ModifiedUtf8,
    /// Neither: UTF-8 with each malformed sequence replaced by one U+FFFD, as Java's
    /// decoder does ([`decode_utf8_replacing`]). The text has at least one U+FFFD that
    /// the bytes do not encode.
    Utf8Replaced,
}

impl UserHeaderEncoding {
    /// Name used by the Python layer: `"utf-8"`, `"modified-utf-8"` or `"utf-8-replaced"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::ModifiedUtf8 => "modified-utf-8",
            Self::Utf8Replaced => "utf-8-replaced",
        }
    }
}

/// Java modified UTF-8 (`java.io.DataInput.readUTF` rules) to a string, or `None` when
/// `b` is not valid modified UTF-8 or holds an unpaired surrogate. Sequences are 1 byte
/// (`01`-`7F`), 2 bytes (`C0`-`DF`, value 0 or >= 0x80; `C0 80` is U+0000) or 3 bytes
/// (`E0`-`EF`, value >= 0x800); a raw `00` byte and 4-byte forms are invalid.
fn decode_modified_utf8(b: &[u8]) -> Option<String> {
    let cont = |i: usize| {
        b.get(i)
            .filter(|&&c| c & 0xC0 == 0x80)
            .map(|&c| u16::from(c & 0x3F))
    };
    let mut units: Vec<u16> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            0x01..=0x7F => {
                units.push(u16::from(c));
                i += 1;
            }
            0xC0..=0xDF => {
                let u = (u16::from(c & 0x1F) << 6) | cont(i + 1)?;
                if u != 0 && u < 0x80 {
                    return None;
                }
                units.push(u);
                i += 2;
            }
            0xE0..=0xEF => {
                let u = (u16::from(c & 0x0F) << 12) | (cont(i + 1)? << 6) | cont(i + 2)?;
                if u < 0x800 {
                    return None;
                }
                units.push(u);
                i += 3;
            }
            _ => return None,
        }
    }
    String::from_utf16(&units).ok()
}

/// UTF-8 to a string with each malformed sequence replaced by one U+FFFD. It
/// matches the observable output of Java 17's UTF-8 decoder with replacement,
/// `new String(bytes, StandardCharsets.UTF_8)` (17.0.11+9 is the JRE Monolith 3.3.27 runs
/// on in HMS 4.13). Clean-room implementation: written from the Unicode Standard
/// (chapter 3, §3.9, Table 3-7 "Well-Formed UTF-8 Byte Sequences" and the U+FFFD
/// substitution of maximal subparts) and black-box tests of the JDK; no JDK source was
/// consulted (recorded JDK outputs: `tests/data/p59b_monolith.tsv`, checked by
/// `tests/dss6_user_header_lossy.rs`).
///
/// Valid UTF-8 decodes unchanged. For invalid input each maximal subpart of a
/// well-formed sequence (or each byte that starts none) becomes one U+FFFD, the practice
/// `String::from_utf8_lossy` also follows, except at an encoded surrogate (`ED A0`-`BF`):
/// Java treats `ED` like `E1`-`EC` (any continuation byte may follow) and replaces the
/// decoded surrogate, so it gives **one** U+FFFD for a complete `ED A0-BF 80-BF`, and one
/// for `ED A0-BF` followed by a non-continuation byte or by the end, where
/// `from_utf8_lossy` gives one per byte. Checked against Java on every byte string of
/// length 1-5 over 20 edge bytes (`tests/dss6_user_header_lossy.rs`) and on every byte
/// string of length 1-3 plus seeded random strings (`tests/p59_java_utf8_blackbox.rs`).
pub fn decode_utf8_replacing(src: &[u8]) -> String {
    // The byte range allowed second after a lead byte, and the number of continuation
    // bytes the lead needs (Unicode Table 3-7; `ED` widened to `80..=BF` as Java does).
    fn shape(lead: u8) -> Option<(u8, u8, usize)> {
        match lead {
            0xC2..=0xDF => Some((0x80, 0xBF, 1)),
            0xE0 => Some((0xA0, 0xBF, 2)),
            0xE1..=0xEF => Some((0x80, 0xBF, 2)),
            0xF0 => Some((0x90, 0xBF, 3)),
            0xF1..=0xF3 => Some((0x80, 0xBF, 3)),
            0xF4 => Some((0x80, 0x8F, 3)),
            _ => None,
        }
    }
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let lead = src[i];
        if lead < 0x80 {
            out.push(char::from(lead));
            i += 1;
            continue;
        }
        let Some((lo, hi, need)) = shape(lead) else {
            out.push('\u{FFFD}');
            i += 1;
            continue;
        };
        let mut value = u32::from(lead) & (0x7F >> (need + 1));
        let mut taken = 1;
        while taken <= need {
            let Some(&b) = src.get(i + taken) else { break };
            let ok = if taken == 1 {
                (lo..=hi).contains(&b)
            } else {
                b & 0xC0 == 0x80
            };
            if !ok {
                break;
            }
            value = (value << 6) | u32::from(b & 0x3F);
            taken += 1;
        }
        i += taken;
        // Incomplete (a maximal subpart) or a surrogate: one U+FFFD either way.
        out.push(if taken > need {
            char::from_u32(value).unwrap_or('\u{FFFD}')
        } else {
            '\u{FFFD}'
        });
    }
    out
}

/// Decode user-header text bytes, in this order:
/// 1. standard UTF-8 when `bytes` is valid UTF-8 ([`UserHeaderEncoding::Utf8`]);
/// 2. else Java modified UTF-8 when valid ([`UserHeaderEncoding::ModifiedUtf8`]): Monolith
///    writes text through JNI, which uses it for characters outside the BMP (a 6-byte
///    surrogate pair) and for U+0000 (`C0 80`);
/// 3. else UTF-8 with each malformed sequence replaced by one U+FFFD, as Java's decoder
///    does ([`UserHeaderEncoding::Utf8Replaced`], [`decode_utf8_replacing`]). Valid
///    sequences keep their characters.
///
/// Apply it to the bytes *after* the NUL cut and the trailing-blank trim
/// ([`user_header_text_bytes`]), so a multi-byte sequence split by the cut, by the end of
/// the words or by the trim is malformed and ends the text with one U+FFFD.
pub fn decode_user_header_text(bytes: &[u8]) -> (String, UserHeaderEncoding) {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return (s.to_string(), UserHeaderEncoding::Utf8);
    }
    if let Some(s) = decode_modified_utf8(bytes) {
        return (s, UserHeaderEncoding::ModifiedUtf8);
    }
    (
        decode_utf8_replacing(bytes),
        UserHeaderEncoding::Utf8Replaced,
    )
}

/// The user header as text, ended at the first NUL, trailing blanks removed, decoded
/// by [`decode_user_header_text`] (UTF-8, modified UTF-8, else UTF-8 with U+FFFD). Empty
/// when no words are stored or the first byte is NUL.
///
/// Monolith's `TimeSeriesContainer.supplementalInfo` for the same record equals this
/// text plus any trailing blanks the record stores, for ASCII, for valid UTF-8 of
/// characters in the BMP and for modified UTF-8. It differs for invalid text and for
/// 4-byte UTF-8, which HotSpot's `NewStringUTF` does not decode (dss6.md §14.2).
pub fn user_header_text(words: &[i32]) -> String {
    decode_user_header_text(&user_header_text_bytes(words)).0
}

/// The decoding [`user_header_text`] uses for these words; `None` when the text is
/// empty (no words, a leading NUL, or only blanks), matching
/// `Location::supplemental_encoding`.
pub fn user_header_text_encoding(words: &[i32]) -> Option<UserHeaderEncoding> {
    let b = user_header_text_bytes(words);
    (!b.is_empty()).then(|| decode_user_header_text(&b).1)
}

/// `Keyword: value` items of user-header text, in stored order, as Monolith parses them
/// (`hec.io.DataContainer.getSupplementalInfo()`, Monolith 3.3.27, read with `javap`):
///
/// 1. The text is split at `;` with `java.util.StringTokenizer`, so empty segments are
///    skipped.
/// 2. Each segment is split at **every** `:` (`hec.util.TextUtil.split(seg, ":")`, which
///    keeps empty parts). Only segments with exactly two parts are items; a segment
///    without `:` (for example `StreamSample Creek`, as one USGS import wrote it) or with
///    more than one `:` is not an item.
/// 3. Trailing blanks (only `' '`) are removed from the keyword and the value; leading
///    blanks are kept (`"State: X"` gives keyword `State`, value `" X"`).
/// 4. An empty keyword or value is kept as `""` (`":v"` gives keyword `""`, `"K:"`
///    value `""`). Monolith's trailing-blank loop calls `charAt(-1)` on an empty part and
///    throws `StringIndexOutOfBoundsException`; the engine keeps the item instead.
///
/// Monolith stores the items in a `HashMap`, so for a repeated keyword the **last**
/// value wins; this function returns every item, and the Python layer builds the dict
/// the same way (last wins).
pub fn parse_user_header_items(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for seg in text.split(';').filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = seg.split(':').collect();
        if parts.len() != 2 {
            continue;
        }
        let key = parts[0].trim_end_matches(' ');
        let value = parts[1].trim_end_matches(' ');
        out.push((key.to_string(), value.to_string()));
    }
    out
}

/// Segments of user-header text that are not items (no `:`, or more than one), in
/// stored order, trailing blanks removed. HEC's keyed lookups never find them
/// (`Support/get_user_header_param.f` and `Utilities/verticalDatum.c:extractFromDelimitedString`
/// search for `keyword:`), and Monolith drops them; they remain visible in the text.
pub fn user_header_unkeyed_segments(text: &str) -> Vec<String> {
    text.split(';')
        .filter(|s| !s.is_empty() && s.split(':').count() != 2)
        .map(|s| s.trim_end_matches(' ').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl crate::dss6::Dss6TsRecord {
    /// [`user_header_bytes`] of this block's user header.
    pub fn user_header_bytes(&self) -> Vec<u8> {
        user_header_bytes(&self.user_header)
    }

    /// [`user_header_text`] of this block's user header.
    pub fn user_header_text(&self) -> String {
        user_header_text(&self.user_header)
    }

    /// [`parse_user_header_items`] of this block's user-header text.
    pub fn user_header_items(&self) -> Vec<(String, String)> {
        parse_user_header_items(&self.user_header_text())
    }

    /// [`user_header_text_encoding`] of this block's user header; `None` when the
    /// text is empty.
    pub fn user_header_encoding(&self) -> Option<UserHeaderEncoding> {
        user_header_text_encoding(&self.user_header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &[u8]) -> Vec<i32> {
        let mut b = s.to_vec();
        while b.len() % 4 != 0 {
            b.push(b' ');
        }
        b.chunks(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn text_stops_at_nul_and_trims_blanks() {
        let w = words(b"A: 1;B: 2;  \0junk");
        assert_eq!(user_header_text(&w), "A: 1;B: 2;");
        assert_eq!(user_header_bytes(&w).len(), w.len() * 4);
        assert_eq!(user_header_text(&words(b"ab  ")), "ab");
        assert_eq!(user_header_text(&[]), "");
        assert_eq!(user_header_text(&[0]), "");
    }

    #[test]
    fn little_endian_word_order_from_literal_bytes() {
        // File bytes "A: 1" "B\0zz" written out by hand: word 1 is 41 3A 20 31, word 2
        // is 42 00 7A 7A. Read as little-endian i32 (the DSS6 word order) they are the
        // literals below; the text is "A: 1B" and stops at the NUL.
        let words = [0x3120_3A41_i32, 0x7A7A_0042_i32];
        assert_eq!(
            user_header_bytes(&words),
            vec![0x41, 0x3A, 0x20, 0x31, 0x42, 0x00, 0x7A, 0x7A]
        );
        assert_eq!(user_header_text(&words), "A: 1B");
        assert_eq!(user_header_text_bytes(&words), b"A: 1B".to_vec());
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        // A lone 0xE9 is one U+FFFD; the rest of the text is unchanged.
        let w = words(b"N\xe9e: x");
        assert_eq!(user_header_text(&w), "N\u{fffd}e: x");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8Replaced)
        );
        // A valid pair before the invalid byte keeps its character (no whole-text rule).
        let w = words(b"\xc3\xbd \xe9");
        assert_eq!(user_header_text(&w), "\u{fd} \u{fffd}");
        // The raw bytes are unchanged.
        assert_eq!(&user_header_bytes(&w)[..4], b"\xc3\xbd \xe9");
    }

    #[test]
    fn replacement_follows_java() {
        // Expected strings are Java 17 `new String(bytes, UTF_8)`.
        let cases: &[(&[u8], &str)] = &[
            (b"A\x80B", "A\u{fffd}B"),
            (b"A\xc3B", "A\u{fffd}B"),
            (b"A\xe2\x82B", "A\u{fffd}B"),
            (b"A\xf0\x9f\x8cB", "A\u{fffd}B"),
            (b"A\xc1\x81B", "A\u{fffd}\u{fffd}B"),
            (b"A\xe0\x80\xafB", "A\u{fffd}\u{fffd}\u{fffd}B"),
            (b"A\xf0\x80\x80\xafB", "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B"),
            // Encoded surrogates: one U+FFFD for three bytes (from_utf8_lossy gives three).
            (b"A\xed\xa0\x80B", "A\u{fffd}B"),
            (b"A\xed\xb0\x80B", "A\u{fffd}B"),
            (b"A\xed\xa0B", "A\u{fffd}B"),
            (b"A\xed\xa0", "A\u{fffd}"),
            (b"A\xf4\x90\x80\x80B", "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B"),
            (b"A\xf5\xf8\xffB", "A\u{fffd}\u{fffd}\u{fffd}B"),
            (b"A\xf5\x80\x80\x80B", "A\u{fffd}\u{fffd}\u{fffd}\u{fffd}B"),
            (
                b"\xc3\xbd\xe9\xe2\x82\xac\x80\xf0\x9f\x8c\x8a",
                "\u{fd}\u{fffd}\u{20ac}\u{fffd}\u{1f30a}",
            ),
            (b"Ab\xe2\x82", "Ab\u{fffd}"),
            (b"Ab\xf0\x9f", "Ab\u{fffd}"),
        ];
        for (b, want) in cases {
            assert_eq!(decode_utf8_replacing(b), *want, "{b:02x?}");
        }
        // Valid UTF-8 is unchanged.
        let s = "x\u{fd}\u{20ac}\u{1f30a}\u{10ffff}";
        assert_eq!(decode_utf8_replacing(s.as_bytes()), s);
        assert_eq!(decode_utf8_replacing(b""), "");
    }

    #[test]
    fn valid_utf8_decodes_to_the_character() {
        let w = words(b"Gage: R\xc3\xado Y\xc3\xbd;");
        assert_eq!(user_header_text(&w), "Gage: R\u{ed}o Y\u{fd};");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8)
        );
        assert_eq!(
            parse_user_header_items(&user_header_text(&w)),
            vec![("Gage".to_string(), " R\u{ed}o Y\u{fd}".to_string())]
        );
        // Three- and four-byte sequences.
        let w = words("\u{20ac}\u{1f30a}".as_bytes());
        assert_eq!(user_header_text(&w), "\u{20ac}\u{1f30a}");
    }

    #[test]
    fn ascii_is_unchanged() {
        let w = words(b"State: X;USGS No: 1;");
        assert_eq!(user_header_text(&w), "State: X;USGS No: 1;");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8)
        );
        // No text: no encoding (no words, a leading NUL, blanks only).
        assert_eq!(user_header_text_encoding(&[]), None);
        assert_eq!(user_header_text_encoding(&[0]), None);
        assert_eq!(user_header_text_encoding(&words(b"    ")), None);
        assert_eq!(decode_user_header_text(b"").1, UserHeaderEncoding::Utf8);
        assert_eq!(UserHeaderEncoding::Utf8.as_str(), "utf-8");
        assert_eq!(UserHeaderEncoding::ModifiedUtf8.as_str(), "modified-utf-8");
        assert_eq!(UserHeaderEncoding::Utf8Replaced.as_str(), "utf-8-replaced");
    }

    #[test]
    fn modified_utf8_surrogate_pair_and_c0_80() {
        // U+1F30A as Java writes it: surrogates D83C DF0A, each as a 3-byte sequence.
        let w = words(b"Note: \xed\xa0\xbc\xed\xbc\x8a;Gage: Y\xc3\xbd");
        assert_eq!(user_header_text(&w), "Note: \u{1f30a};Gage: Y\u{fd}");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::ModifiedUtf8)
        );
        // U+0000 as C0 80 (survives the NUL cut, which looks for a 00 byte).
        let w = words(b"A\xc0\x80B");
        assert_eq!(user_header_text(&w), "A\u{0}B");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::ModifiedUtf8)
        );
        // An unpaired surrogate is not modified UTF-8 either: one U+FFFD (Java).
        let w = words(b"x\xed\xa0\xbc");
        assert_eq!(user_header_text(&w), "x\u{fffd}");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8Replaced)
        );
        // A pair next to an invalid byte: the text is in neither form, so step 3 replaces
        // each surrogate half (Java's decoder does not join them).
        let w = words(b"\xed\xa0\xbc\xed\xbc\x8a \xe9");
        assert_eq!(user_header_text(&w), "\u{fffd}\u{fffd} \u{fffd}");
        // A 4-byte sequence is standard UTF-8 (step 1), not modified UTF-8.
        assert_eq!(
            decode_user_header_text("\u{1f30a}".as_bytes()).1,
            UserHeaderEncoding::Utf8
        );
        // Overlong 2-byte forms other than C0 80 are invalid: replaced.
        assert_eq!(
            decode_user_header_text(b"\xc1\x81"),
            (
                "\u{fffd}\u{fffd}".to_string(),
                UserHeaderEncoding::Utf8Replaced
            )
        );
    }

    #[test]
    fn cut_and_trim_apply_before_decoding() {
        // Bytes after the NUL (invalid 0xE9 here) do not affect the decoding.
        let w = words(b"Y\xc3\xbd  \0\xe9\xe9");
        assert_eq!(user_header_text(&w), "Y\u{fd}");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8)
        );
        // A two-byte sequence split by the NUL: one U+FFFD at the end.
        let w = words(b"Y\xc3\0\xbd");
        assert_eq!(user_header_text(&w), "Y\u{fffd}");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8Replaced)
        );
        // Split by the end of the words (no NUL, one whole word, no padding).
        let w = words(b"abc\xc3");
        assert_eq!(w.len(), 1);
        assert_eq!(user_header_text(&w), "abc\u{fffd}");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8Replaced)
        );
        // Split by the trailing-blank trim (0xE2 0x82 of a 3-byte sequence, then blanks):
        // the trim runs first, so the blanks are not text.
        let w = words(b"x\xe2\x82 ");
        assert_eq!(user_header_text(&w), "x\u{fffd}");
        assert_eq!(
            user_header_text_encoding(&w),
            Some(UserHeaderEncoding::Utf8Replaced)
        );
    }

    #[test]
    fn items_follow_monolith() {
        let t = "State: Example;StreamSample Creek;GageDatum: 1.5;;a:b:c;Empty:;K:  v  ;K:w";
        let items = parse_user_header_items(t);
        assert_eq!(
            items,
            vec![
                ("State".into(), " Example".into()),
                ("GageDatum".into(), " 1.5".into()),
                ("Empty".into(), "".into()),
                ("K".into(), "  v".into()),
                ("K".into(), "w".into()),
            ]
        );
        assert_eq!(
            user_header_unkeyed_segments(t),
            vec!["StreamSample Creek".to_string(), "a:b:c".to_string()]
        );
    }

    #[test]
    fn empty_keyword_or_value_is_kept() {
        assert_eq!(
            parse_user_header_items(":v;K:;B:   "),
            vec![
                ("".to_string(), "v".to_string()),
                ("K".to_string(), "".to_string()),
                ("B".to_string(), "".to_string()),
            ]
        );
    }
}

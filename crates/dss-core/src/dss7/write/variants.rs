//! Packet P38: the time-series store variants the P20/P21 block code does not carry:
//! multi-word quality, integer and character notes, and pattern records.
//!
//! Ports (paths relative to `heclib/heclib_c/src/`, `hec-dss` tag `7-IU-15`):
//! - `Internal/ztsAggregate.c:ztsAggregate()` lines 145-741, complete ([`aggregate_full`]);
//! - `Internal/ztsFirstLastValidValues.c` with quality, integer and character notes
//!   ([`first_last_full`]);
//! - `Utilities/convertDataLength.c:convertDataLength()` ([`resize_elements`]);
//! - `SemiPublic/ztsStorePattern.c:ztsStorePattern()` (the input types here; the record is
//!   built in `engine/variants.rs`).
//!
//! Everything is in int32 words, as in HEC.

use super::block::ih;
use super::WriteError;

/// Largest quality or integer-note element the writer stores, in words: what the engine's
/// reader takes back (`dss7/ts.rs`). heclib has no limit ("should handle any reasonable
/// number", `Public/ztsStore.c`).
pub const MAX_ELEMENT_WORDS: usize = 64;

/// Notes to store with a series: integer notes or character notes, never both, because
/// they share a storage area (`Public/ztsStore.c`, "You cannot use inotes if you use
/// cnotes").
#[derive(Debug, Clone, Copy)]
pub enum TsNotes<'a> {
    /// `element_size` words per value, `n × element_size` words.
    Int {
        words: &'a [i32],
        element_size: usize,
    },
    /// One byte string per value, empty for no note, without NUL bytes (HEC separates the
    /// notes with NUL).
    Char(&'a [&'a [u8]]),
}

/// The optional arrays of a [`super::RtsInput`] or [`super::ItsInput`] beyond single-word
/// quality.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsVariants<'a> {
    /// Quality of `element size` words per value: `(words, element_size)` with
    /// `n × element_size` words. Give this or the input's `quality`, not both.
    pub quality_words: Option<(&'a [i32], usize)>,
    pub notes: Option<TsNotes<'a>>,
}

impl TsVariants<'_> {
    pub fn is_empty(&self) -> bool {
        self.quality_words.is_none() && self.notes.is_none()
    }
}

/// One pattern record (`ztsStorePattern()`): types 101/106 for a regular E part, 111/116
/// for an `IR-` E part. HEC stores no quality or notes in a pattern, so there are none
/// here.
#[derive(Debug, Clone)]
pub struct PatternInput<'a> {
    /// `/A/B/C/TS-.../E/F/`. The D part is kept as given.
    pub pathname: &'a [u8],
    pub values: super::RtsValues<'a>,
    /// Irregular patterns: one time per value, strictly ascending, whole seconds from HEC
    /// julian 0, exactly what the reader returns for such a record. `None` for a regular
    /// pattern, which is positional.
    pub times: Option<&'a [i64]>,
    pub units: &'a [u8],
    pub data_type: &'a [u8],
    pub time_zone: Option<&'a [u8]>,
    /// Decimal places (`INT_HEAD_precision`); −1 = unset.
    pub precision: i32,
    /// Irregular patterns: seconds per stored time unit, 1, 60, 3600 or 86400. `None`
    /// chooses 60 when every time is a whole minute, else 1.
    pub time_granularity: Option<i64>,
}

impl<'a> PatternInput<'a> {
    pub fn new(
        pathname: &'a [u8],
        values: super::RtsValues<'a>,
        units: &'a [u8],
        data_type: &'a [u8],
    ) -> Self {
        PatternInput {
            pathname,
            values,
            times: None,
            units,
            data_type,
            time_zone: None,
            precision: -1,
            time_granularity: None,
        }
    }
}

fn bad_input(m: impl Into<String>) -> WriteError {
    WriteError::BadInput(m.into())
}

/// The arrays of one write, checked: quality words and notes per value.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Extras<'a> {
    pub quality: Option<&'a [i32]>,
    pub qes: usize,
    pub inotes: Option<&'a [i32]>,
    pub nes: usize,
    pub cnotes: Option<&'a [&'a [u8]]>,
}

impl Extras<'_> {
    pub fn any(&self) -> bool {
        self.qes > 0 || self.nes > 0 || self.cnotes.is_some()
    }
}

/// Validate the optional arrays of `n` values, before any file is touched.
pub(super) fn check_extras<'a>(
    n: usize,
    plain_quality: Option<&'a [i32]>,
    v: &TsVariants<'a>,
) -> Result<Extras<'a>, WriteError> {
    let mut x = Extras::default();
    if plain_quality.is_some() && v.quality_words.is_some() {
        return Err(bad_input("give quality or quality_words, not both"));
    }
    if let Some(q) = plain_quality {
        // The caller's plan already checked the length.
        x.quality = Some(q);
        x.qes = 1;
    }
    let size_ok = |what: &str, len: usize, k: usize| -> Result<(), WriteError> {
        if !(1..=MAX_ELEMENT_WORDS).contains(&k) {
            return Err(bad_input(format!(
                "{what} element size {k} is not 1..={MAX_ELEMENT_WORDS}"
            )));
        }
        if Some(len) != n.checked_mul(k) {
            return Err(bad_input(format!(
                "{len} {what} words for {n} values of {k} words"
            )));
        }
        Ok(())
    };
    if let Some((q, k)) = v.quality_words {
        size_ok("quality", q.len(), k)?;
        x.quality = Some(q);
        x.qes = k;
    }
    match v.notes {
        Some(TsNotes::Int {
            words,
            element_size,
        }) => {
            size_ok("integer note", words.len(), element_size)?;
            x.inotes = Some(words);
            x.nes = element_size;
        }
        Some(TsNotes::Char(c)) => {
            if c.len() != n {
                return Err(bad_input(format!(
                    "{} character notes for {n} values",
                    c.len()
                )));
            }
            let mut total = 0usize;
            for (i, s) in c.iter().enumerate() {
                if s.contains(&0) {
                    return Err(bad_input(format!(
                        "character note {i} holds a NUL byte (HEC's note separator)"
                    )));
                }
                // The block check (`check_block_budget`) covers notes that add up; a single
                // note over the limit is refused here, before anything is copied.
                check_block_budget(1, 1, 0, 0, s.len() as i64 + 1)?;
                total = total.saturating_add(s.len() + 1);
            }
            if total > i32::MAX as usize / 2 {
                return Err(bad_input("the character notes exceed HEC's int32 lengths"));
            }
            x.cnotes = Some(c);
        }
        None => {}
    }
    Ok(x)
}

/// The engine's reader refuses a block whose internal-header sizes or expanded arrays
/// exceed `MAX_OUT_WORDS` (`dss7/compress.rs`); the writer shares that limit, so it never
/// stores a block it cannot read back. `n` values of `value_size` words, `qes` quality and
/// `nes` integer-note words each, `cnotes` character-note bytes with their NULs.
pub(super) fn check_block_budget(
    n: i64,
    value_size: i64,
    qes: i64,
    nes: i64,
    cnotes: i64,
) -> Result<(), WriteError> {
    let limit = crate::dss7::compress::MAX_OUT_WORDS;
    for (what, size) in [
        ("values", n.saturating_mul(value_size)),
        ("quality", n.saturating_mul(qes)),
        ("integer notes", n.saturating_mul(nes)),
        ("character notes", cnotes),
    ] {
        if size > limit {
            return Err(bad_input(format!(
                "one block would hold {size} {what} words or bytes; the reader's limit is {limit}"
            )));
        }
    }
    Ok(())
}

/// `convertDataLength()` over an array: each element truncated or zero-filled to `to`
/// words. This is what heclib does to stored quality and notes read with another element
/// size (`Internal/ztsDisaggregate.c` lines 555 and 592).
pub(super) fn resize_elements(words: &[i32], from: usize, to: usize, n: usize) -> Vec<i32> {
    if from == to {
        return words.to_vec();
    }
    let mut out = vec![0i32; n * to];
    if from == 0 {
        return out;
    }
    for i in 0..n {
        let k = from.min(to);
        if (i + 1) * from <= words.len() {
            out[i * to..i * to + k].copy_from_slice(&words[i * from..i * from + k]);
        }
    }
    out
}

/// NUL-terminated notes, one per value (`copyLines()` layout, `Internal/ztsRegStoreBlock.c`).
pub(super) fn join_cnotes(notes: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::with_capacity(notes.iter().map(|s| s.len() + 1).sum());
    for s in notes {
        b.extend_from_slice(s);
        b.push(0);
    }
    b
}

/// Split stored notes into exactly `n` strings; absent notes are empty.
pub(super) fn split_cnotes(bytes: &[u8], n: usize) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = bytes
        .split(|&b| b == 0)
        .take(n)
        .map(<[u8]>::to_vec)
        .collect();
    v.resize(n, Vec::new());
    v
}

/// `ztsFirstLastValidFloat/Double()` with every array. A position is valid when its value
/// is not missing, or any word of its quality or integer note is non-zero, or the byte of
/// the character-note array at HEC's test position is not NUL: `cnotes[i]` going forward
/// and `cnotes[total - (n - i)]` with a positive position going backward. HEC tests single
/// bytes there, not the i-th note; this port does too.
pub(super) fn first_last_full(
    missing: &dyn Fn(usize) -> bool,
    n: usize,
    quality: Option<(&[i32], usize)>,
    inotes: Option<(&[i32], usize)>,
    cnotes: Option<&[u8]>,
) -> (bool, usize, usize) {
    let words_set = |a: Option<(&[i32], usize)>, i: usize| {
        a.is_some_and(|(w, k)| k > 0 && w[i * k..(i + 1) * k].iter().any(|&x| x != 0))
    };
    let first = (0..n).find(|&i| {
        !missing(i)
            || words_set(quality, i)
            || words_set(inotes, i)
            || cnotes.is_some_and(|c| i < c.len() && c[i] != 0)
    });
    let Some(first) = first else {
        return (true, 0, 0);
    };
    let last = (0..n)
        .rev()
        .find(|&i| {
            !missing(i)
                || words_set(quality, i)
                || words_set(inotes, i)
                || cnotes.is_some_and(|c| {
                    let p = c.len() as i64 - (n - i) as i64;
                    p > 0 && c[p as usize] != 0
                })
        })
        .unwrap_or(first);
    (false, first, last)
}

fn repeats(words: &[i32], n: usize, size: usize) -> usize {
    (1..n)
        .filter(|&i| words[i * size..(i + 1) * size] == words[(i - 1) * size..i * size])
        .count()
}

/// `Internal/ztsRegRepeatCompress.c:ztsRegRepeatCompress()`.
fn repeat_compress(words: &[i32], n: usize, size: usize, out: &mut Vec<i32>, header2: &mut [i32]) {
    out.extend_from_slice(&words[..size]);
    for i in 1..n {
        if words[i * size..(i + 1) * size] == words[(i - 1) * size..i * size] {
            header2[i / 32] |= 1i32.wrapping_shl((i % 32) as u32);
        } else {
            out.extend_from_slice(&words[i * size..(i + 1) * size]);
        }
    }
}

fn pad_even(v: &mut Vec<i32>) {
    if v.len() % 2 == 1 {
        v.push(0);
    }
}

/// Bytes as int32 words, little-endian, zero-filled (`charLong()` with `zeroEndFlag` −1 into
/// HEC's `calloc`ed array).
fn push_bytes(out: &mut Vec<i32>, bytes: &[u8]) {
    for c in bytes.chunks(4) {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        out.push(i32::from_le_bytes(b));
    }
}

/// The inputs of [`aggregate_full`].
pub(super) struct AggregateInput<'a> {
    pub n: usize,
    pub times: Option<&'a [i32]>,
    pub values: &'a [i32],
    pub value_size: usize,
    pub quality: &'a [i32],
    pub qes: usize,
    pub inotes: &'a [i32],
    pub nes: usize,
    pub cnotes: &'a [u8],
}

/// One array of the full-compression branch: where its words come from and which
/// internal-header words describe it.
struct CompressArea<'a> {
    words: &'a [i32],
    size: usize,
    compress: bool,
    repeats: usize,
    flag_at: usize,
    number_at: usize,
}

/// `ztsAggregate()` (7-HA layout), complete. Returns values1 and header2 and sets
/// internal-header words 6..=16 as HEC does.
pub(super) fn aggregate_full(a: &AggregateInput, h: &mut [i32]) -> (Vec<i32>, Vec<i32>) {
    let (n, vs, qes, nes) = (a.n, a.value_size, a.qes, a.nes);
    let cn = a.cnotes.len();
    // Lines 226-241.
    let inotes_uncompressed = if cn > 0 && nes == 0 {
        (cn - 1) / 4 + 1
    } else {
        n * nes
    };
    h[ih::VALUE_SIZE] = vs as i32;
    h[ih::VALUES_COMPRESSION] = 0;
    h[ih::QUALITY_ELEMENT_SIZE] = qes as i32;
    h[ih::QUALITY_COMPRESSION] = 0;
    h[ih::INOTES_ELEMENT_SIZE] = nes as i32;
    h[ih::INOTES_COMPRESSION] = 0;
    h[ih::CNOTES_LENGTH] = cn as i32;

    let (mut cv, mut cq, mut cnt) = (false, false, false);
    let (mut vr, mut qr, mut nr) = (0usize, 0usize, 0usize);
    // Lines 289-365.
    if n > 12 {
        vr = repeats(a.values, n, vs);
        if qes > 0 {
            qr = repeats(a.quality, n, qes);
        }
        if nes > 0 {
            nr = repeats(a.inotes, n, nes);
        }
        // `ratio` is a C float compared with the double constants 0.05 and 0.10.
        let ratio = |r: usize| f64::from(r as f32 / n as f32);
        cv = ratio(vr) > 0.05;
        cq = qes > 0 && ratio(qr) > 0.10;
        cnt = nes > 0 && ratio(nr) > 0.10;
    }
    let mut out: Vec<i32> = Vec::new();
    if let Some(t) = a.times {
        out.extend_from_slice(&t[..n]);
    }
    // Lines 368-465: no compression.
    if !cv && !cq && !cnt {
        out.extend_from_slice(&a.values[..n * vs]);
        h[ih::VALUES_NUMBER] = (n * vs) as i32;
        pad_even(&mut out);
        h[ih::QUALITY_NUMBER] = (n * qes) as i32;
        if qes > 0 {
            out.extend_from_slice(&a.quality[..n * qes]);
            pad_even(&mut out);
        }
        h[ih::INOTES_NUMBER] = (n * nes) as i32;
        if nes > 0 {
            out.extend_from_slice(&a.inotes[..n * nes]);
            pad_even(&mut out);
        }
        if cn > 0 {
            h[ih::INOTES_NUMBER] = inotes_uncompressed as i32;
            push_bytes(&mut out, a.cnotes);
            pad_even(&mut out);
        }
        return (out, Vec::new());
    }
    // Lines 471-518: every value repeats, nothing else stored.
    if vr + 1 == n && qes == 0 && nes == 0 && cn == 0 {
        h[ih::VALUES_COMPRESSION] = 2;
        h[ih::VALUES_NUMBER] = vs as i32;
        h[ih::QUALITY_NUMBER] = 0;
        h[ih::INOTES_NUMBER] = 0;
        out.extend_from_slice(&a.values[..vs]);
        pad_even(&mut out);
        return (out, Vec::new());
    }
    // Lines 531-741: full compression.
    let chn = (n - 1) / 32 + 1;
    let mut header2 = vec![0i32; (usize::from(cv) + usize::from(cq) + usize::from(cnt)) * chn];
    let mut pos = 0usize;
    // Flag 2 keeps the first element, flag 1 the bits and the changes, 0 everything.
    let mut area = |out: &mut Vec<i32>, c: CompressArea, h: &mut [i32]| {
        if !c.compress {
            h[c.number_at] = (n * c.size) as i32;
            out.extend_from_slice(&c.words[..n * c.size]);
        } else if c.repeats + 1 == n {
            h[c.flag_at] = 2;
            h[c.number_at] = c.size as i32;
            out.extend_from_slice(&c.words[..c.size]);
        } else {
            h[c.flag_at] = 1;
            h[c.number_at] = ((n - c.repeats) * c.size) as i32;
            repeat_compress(c.words, n, c.size, out, &mut header2[pos..pos + chn]);
            pos += chn;
        }
    };
    area(
        &mut out,
        CompressArea {
            words: a.values,
            size: vs,
            compress: cv,
            repeats: vr,
            flag_at: ih::VALUES_COMPRESSION,
            number_at: ih::VALUES_NUMBER,
        },
        h,
    );
    pad_even(&mut out);
    area(
        &mut out,
        CompressArea {
            words: a.quality,
            size: qes,
            compress: cq,
            repeats: qr,
            flag_at: ih::QUALITY_COMPRESSION,
            number_at: ih::QUALITY_NUMBER,
        },
        h,
    );
    if qes > 0 {
        pad_even(&mut out);
    }
    area(
        &mut out,
        CompressArea {
            words: a.inotes,
            size: nes,
            compress: cnt,
            repeats: nr,
            flag_at: ih::INOTES_COMPRESSION,
            number_at: ih::INOTES_NUMBER,
        },
        h,
    );
    if inotes_uncompressed > 0 {
        pad_even(&mut out);
    }
    if cn > 0 {
        h[ih::INOTES_NUMBER] = inotes_uncompressed as i32;
        push_bytes(&mut out, a.cnotes);
        pad_even(&mut out);
    }
    (out, header2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_truncates_and_fills() {
        assert_eq!(resize_elements(&[1, 2, 3, 4], 2, 1, 2), vec![1, 3]);
        assert_eq!(resize_elements(&[1, 3], 1, 2, 2), vec![1, 0, 3, 0]);
        assert_eq!(resize_elements(&[], 0, 2, 2), vec![0; 4]);
    }

    #[test]
    fn block_budget_is_the_readers_limit() {
        let limit = crate::dss7::compress::MAX_OUT_WORDS;
        assert!(check_block_budget(1, 1, 0, 0, limit).is_ok());
        assert!(check_block_budget(1, 1, 0, 0, limit + 1).is_err());
        assert!(check_block_budget(limit / 64, 1, 64, 0, 0).is_ok());
        assert!(check_block_budget(limit / 64 + 1, 1, 64, 0, 0).is_err());
        assert!(check_block_budget(limit / 2 + 1, 2, 0, 0, 0).is_err());
        assert!(check_block_budget(limit / 2 + 1, 1, 0, 2, 0).is_err());
    }

    #[test]
    fn notes_join_and_split() {
        let n = vec![b"ab".to_vec(), Vec::new(), b"c".to_vec()];
        let j = join_cnotes(&n);
        assert_eq!(j, b"ab\0\0c\0");
        assert_eq!(split_cnotes(&j, 3), n);
        assert_eq!(split_cnotes(b"a\0", 2), vec![b"a".to_vec(), Vec::new()]);
    }

    #[test]
    fn checks_refuse_bad_arrays() {
        let q = [1, 2, 3];
        let v = TsVariants {
            quality_words: Some((&q, 2)),
            notes: None,
        };
        assert!(check_extras(2, None, &v).is_err());
        assert!(check_extras(3, Some(&q), &v).is_err());
        let notes: [&[u8]; 1] = [b"a\0b"];
        let v = TsVariants {
            quality_words: None,
            notes: Some(TsNotes::Char(&notes)),
        };
        assert!(check_extras(1, None, &v).is_err());
    }
}

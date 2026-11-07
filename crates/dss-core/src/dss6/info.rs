//! DSS6 record information block.
//!
//! Layout: `heclib_f/src/zinit6.f:ZINIT6` lines 699-774 (`NINFO = 28` words plus the
//! pathname); flag `NPFLAG = -9753`. Validation follows `heclib_f/src/zrdinf6.f:ZRDINF6`
//! lines 56-70. Offsets are 0-based from the info address, `P` = pathname words
//! (dss6.md §5.1).

use super::header::latin1_trim;

/// Fixed words in an info block besides the pathname (`zinit6.f`, `NINFO`).
pub const NINFO: usize = 28;
/// Info-block flag (`NPFLAG`).
pub const INFO_FLAG: i32 = -9753;

/// A decoded info block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dss6Info {
    /// Word address of the block.
    pub address: i64,
    /// `KIFLAG`; `-9753` when valid.
    pub flag: i32,
    /// `KISTAT`: same codes as the bin, plus -1 = relocated.
    pub status: i32,
    /// `KINPAT` and `KIPATH`.
    pub pathname: Vec<u8>,
    /// `KIPASS`, 2 words. Nonzero means HEC denies access (`zrdinf6.f` lines 65-70).
    pub password: [i32; 2],
    /// `KIADAT`, `KINDAT`, `KILNDA`.
    pub data_address: i32,
    pub data_words: i32,
    pub logical_values: i32,
    /// `KIVER`.
    pub record_version: i32,
    /// `KIPROG` (6 characters), `KIDATE`, `KITIME`, `KITAG` as text.
    pub program: String,
    pub last_write_date: String,
    pub last_write_time: String,
    pub tag: String,
    /// `KITYPE`.
    pub record_type: i32,
    /// `KICOMP`, `KIQUAL`.
    pub compression: i32,
    pub quality: i32,
    /// `KIAIHE`/`KINIHE` internal header, `KIACHE`/`KINCHE` compression header,
    /// `KIAUHE`/`KINUHE` user header (address, length in words).
    pub internal_header: (i32, i32),
    pub compression_header: (i32, i32),
    pub user_header: (i32, i32),
    /// `KIPREC`, `KIUNUS`.
    pub precision: i32,
    pub unused_words: i32,
}

/// Words needed for the info block of a pathname of `npath` characters.
pub fn info_words(npath: i32) -> usize {
    NINFO + path_words(npath)
}

/// Pathname words `P = (npath-1)/4 + 1` (`NCPW = 4`, `zinit6.f` lines 238-244).
pub fn path_words(npath: i32) -> usize {
    if npath <= 0 {
        1
    } else {
        ((npath - 1) / 4 + 1) as usize
    }
}

/// Pathname bytes packed in logical words, in file byte order (dss6.md §1.5).
pub(crate) fn unpack_chars(words: &[i32], nchars: usize) -> Vec<u8> {
    let mut out: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    out.truncate(nchars);
    out
}

impl Dss6Info {
    /// Decode from `words` read at `address`. `None` if the slice is too short for the
    /// pathname length it declares, or the length is not in `1..=392`.
    pub fn decode(address: i64, words: &[i32]) -> Option<Self> {
        let npath = *words.get(2)?;
        if !(1..=super::MAX_PATHNAME).contains(&npath) {
            return None;
        }
        let p = path_words(npath);
        if words.len() < NINFO + p {
            return None;
        }
        let w = |i: usize| words[i + p];
        let text = |i: usize, n: usize| {
            latin1_trim(&unpack_chars(&words[i + p..i + p + n.div_ceil(4)], n))
        };
        Some(Self {
            address,
            flag: words[0],
            status: words[1],
            pathname: unpack_chars(&words[3..3 + p], npath as usize),
            password: [w(3), w(4)],
            data_address: w(5),
            data_words: w(6),
            logical_values: w(7),
            record_version: w(8),
            program: text(9, 6),
            last_write_date: text(11, 7),
            last_write_time: text(13, 8),
            tag: text(15, 8),
            record_type: w(17),
            compression: w(18),
            quality: w(19),
            internal_header: (w(20), w(21)),
            compression_header: (w(22), w(23)),
            user_header: (w(24), w(25)),
            precision: w(26),
            unused_words: w(27),
        })
    }

    pub fn has_valid_flag(&self) -> bool {
        self.flag == INFO_FLAG
    }

    /// HEC denies access when a password is set and the file is `6-E` or later
    /// (`zrdinf6.f:ZRDINF6` lines 65-70).
    pub fn is_password_protected(&self) -> bool {
        self.password != [0, 0]
    }
}

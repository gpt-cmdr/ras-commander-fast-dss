//! Record-level DSS6 writing: header, hash table, bins, info blocks, the block
//! transaction (dss6.md §9.3) and reconciliation of an interrupted native write.
//!
//! Sources (hec-dss tag `7-IU-15`, `heclib/heclib_f/src/`):
//! - file creation: `zopen6Int.f:ZOPEN6INT` lines 371-523, `zfsize6.f:ZFSIZE6` (size code
//!   6: 2048 hash codes, 32 bins of 127 words per block);
//! - lookup and the position of a new entry: `zcheck6.f:ZCHECK6` lines 212-326;
//! - a new record: `znwrit6.f:ZNWRIT6`; a rewritten record: `zowrit6.f:ZOWRIT6`; a new
//!   pathname bin or bin block: `znwbin6.f:ZNWBIN6`; tags: `zgetag6.f:ZGETAG6`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::io::Io;
use super::{
    BlockOutcome, BlockReport, WriteError, WriteOptions, WriteReport, WriteStep, NEW_FILE_VERSION6,
    PROGRAM_NAME6,
};
use crate::dss6::{zhash6, Dss6Header, EOF_FLAG, HEADER_BYTES, NPERM};
use crate::dss7::write::lockhold;
use crate::lock::{self, LockReason, LockRefusal};
use crate::time::julian_to_ymd;

// Header words, 0-based (dss6.md §3; pointer chain in zinit6.f:ZINIT6 lines 446-572).
pub(super) const K_NRECS: usize = 1;
pub(super) const K_SEQNO: usize = 2;
pub(super) const K_HSIZE: usize = 3;
pub(super) const K_VERS: usize = 4;
pub(super) const K_FSIZE: usize = 5;
pub(super) const K_DEAD: usize = 6;
pub(super) const K_CREAT: usize = 7;
pub(super) const K_LWDAT: usize = 9;
pub(super) const K_LWTIM: usize = 11;
pub(super) const K_HASH: usize = 14;
pub(super) const K_TABLE: usize = 15;
pub(super) const K_BNBLK: usize = 16;
pub(super) const K_BNREM: usize = 17;
pub(super) const K_BNSIZ: usize = 18;
pub(super) const K_AFBIN: usize = 19;
pub(super) const K_ANBIN: usize = 20;
pub(super) const K_BINS: usize = 37;
pub(super) const K_HUSED: usize = 38;
pub(super) const K_BOVER: usize = 39;
pub(super) const K_MAXPH: usize = 40;
pub(super) const K_MAXHC: usize = 41;
pub(super) const K_ITSIN: usize = 44;
pub(super) const K_ITSDA: usize = 45;
pub(super) const K_MXPRT: usize = 52;
/// In-progress marker of a native write (dss6.md §9.3): `KRES` words 13 and 14 (header
/// words 67 and 68). `KRES` is "unused area - 14 words" (`zinit6.f` line 100, lines
/// 566-569); HEC reads and writes the permanent section whole (`zrdprm6.f` line 51,
/// `zptrec6(..., NPERM, 1, ...)` in `znwrit6.f` line 216), so it keeps these words.
/// Word 68 holds the info address of the record being written, word 67 the address of
/// the info block it replaces (0 for a new record).
pub(super) const K_MARK_OLD: usize = 67;
pub(super) const K_MARK_NEW: usize = 68;

/// `NLBIN` (`zinit6.f` lines 654-696): words of a bin entry besides the pathname.
const NLBIN: usize = 11;
/// `NINFO` (`zinit6.f` lines 699-774): words of an info block besides the pathname.
const NINFO: usize = 28;
/// `NPFLAG` (`zinit6.f`): first word of an info block.
const INFO_FLAG: i32 = -9753;
/// `KBNSIZ` of every file this writer modifies (127 since 6-M, `zfsize6.f` line 187).
pub(super) const BIN: usize = 127;
const WORDS_PER_RECORD: i64 = 127;
/// `KVERNO` of `6-M`: all bins are 127 words from this version on (`zfsize6.f` line 187).
const KVERNO_6M: i32 = 60_000 + 100 * b'M' as i32;

/// Words of a new empty file (see [`super::new_file_words6`]).
pub(super) fn new_file_words(date: &[u8; 7], time: &[u8; 8]) -> Vec<i32> {
    // zfsize6.f lines 58-75 and 174-188: default size code 6.
    let khash: i64 = 2048;
    let kbnblk: i64 = 32;
    // zopen6Int.f lines 411-429: first bin on a record boundary after the table, with the
    // record before it kept for locking.
    let after_table = khash + NPERM as i64 + 1;
    let (rec, wrd) = rec_word(after_table);
    let lock_rec = if wrd != 1 { rec + 1 } else { rec };
    let kafbin = lock_rec * WORDS_PER_RECORD + 1;
    let kfsize = kafbin + kbnblk * BIN as i64;
    let nwords = (rec_word(kfsize).0 * WORDS_PER_RECORD) as usize;
    let mut w = vec![0i32; nwords];
    put_chars(&mut w[0..1], b"ZDSS");
    w[K_HSIZE] = 6;
    put_chars(&mut w[K_VERS..K_VERS + 1], NEW_FILE_VERSION6);
    w[K_FSIZE] = kfsize as i32;
    put_date(&mut w[K_CREAT..K_CREAT + 2], date);
    put_date(&mut w[K_LWDAT..K_LWDAT + 2], date);
    put_chars(&mut w[K_LWTIM..K_LWTIM + 2], time);
    w[K_HASH] = khash as i32;
    w[K_TABLE] = 1;
    w[K_BNBLK] = kbnblk as i32;
    w[K_BNREM] = kbnblk as i32;
    w[K_BNSIZ] = BIN as i32;
    w[K_AFBIN] = kafbin as i32;
    w[K_ANBIN] = kafbin as i32;
    // zopen6Int.f lines 470-475: irregular block growth and minimum sizes.
    w[K_ITSIN] = 100;
    for k in 0..5 {
        w[K_ITSDA + k] = 200;
    }
    // zopen6Int.f line 424 writes one word at the start of the lock record while it is
    // the last record of the file, so its end-of-file flag lands in the record's second
    // word (seen in a Monolith 3.3.27 empty file). Copied for word identity; nothing reads
    // it.
    let lock_first = (lock_rec - 1) * WORDS_PER_RECORD + 1;
    w[lock_first as usize] = EOF_FLAG;
    w[kfsize as usize - 1] = EOF_FLAG;
    w
}

/// 1-based (record, word) of a 1-based word address (`zgetrw6.f`).
pub(super) fn rec_word(addr: i64) -> (i64, i64) {
    (
        (addr - 1) / WORDS_PER_RECORD + 1,
        (addr - 1) % WORDS_PER_RECORD + 1,
    )
}

/// Pack characters into words (`CHRHOL`/`CH2HOL`, little-endian files, dss6.md §1.5).
pub(super) fn put_chars(dst: &mut [i32], s: &[u8]) {
    for (i, w) in dst.iter_mut().enumerate() {
        let mut b = [0u8; 4];
        for (k, c) in b.iter_mut().enumerate() {
            if let Some(&ch) = s.get(4 * i + k) {
                *c = ch;
            }
        }
        *w = i32::from_le_bytes(b);
    }
}

/// `n` characters blank-padded into words (Fortran `CHARACTER*n` = string).
pub(super) fn blank_padded(s: &[u8], words: usize) -> Vec<i32> {
    let mut b = vec![b' '; words * 4];
    b[..s.len().min(words * 4)].copy_from_slice(&s[..s.len().min(words * 4)]);
    let mut w = vec![0i32; words];
    put_chars(&mut w, &b);
    w
}

fn put_date(dst: &mut [i32], date: &[u8; 7]) {
    // CHRHOL(CDATE, 1, NDATEC=7, ...): 7 characters; the eighth byte is left as it was
    // (NUL in a zeroed buffer).
    put_chars(dst, date);
}

/// Current date and time as `WHEN` returns them (`Support/when.f`): `DDMMMYY` and
/// `HH:MM:SS`. HEC uses local time; the writer uses UTC (`dss-core` has no time-zone
/// database).
pub(super) fn when() -> ([u8; 7], [u8; 8]) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let julian = secs.div_euclid(86_400) + 25_568;
    let s = secs.rem_euclid(86_400);
    let (y, m, d) = julian_to_ymd(julian);
    const MON: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let date = format!("{d:02}{}{:02}", MON[(m - 1) as usize], y.rem_euclid(100));
    let time = format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60);
    let mut a = [b' '; 7];
    a.copy_from_slice(&date.as_bytes()[..7]);
    let mut b = [b' '; 8];
    b.copy_from_slice(&time.as_bytes()[..8]);
    (a, b)
}

fn corrupt(m: impl Into<String>) -> WriteError {
    WriteError::Corrupt(m.into())
}

/// Pathname words `NPPWRD` (`zcheck6.f` line 55).
pub(super) fn path_words(npath: usize) -> usize {
    (npath - 1) / 4 + 1
}

/// A record found by [`Engine::lookup`].
#[derive(Debug, Clone)]
pub(super) struct Existing {
    pub bin_addr: i64,
    /// 1-based word of the entry in its bin (`JPNBIN`).
    pub j: usize,
    pub nbw: usize,
    pub bin_type: i32,
    pub info_addr: i64,
    /// The `NINFO + NPPWRD` info words.
    pub info: Vec<i32>,
}

impl Existing {
    /// Info word `KIPASS + k` (0-based after the pathname: 0..1 password, 2 `KIADAT`,
    /// 3 `KINDAT`, 4 `KILNDA`, 5 `KIVER`, 6..7 `KIPROG`, 8..9 `KIDATE`, 10..11 `KITIME`,
    /// 12..13 `KITAG`, 14 `KITYPE`, 15 `KICOMP`, 16 `KIQUAL`, 17 `KIAIHE`, 18 `KINIHE`,
    /// 19 `KIACHE`, 20 `KINCHE`, 21 `KIAUHE`, 22 `KINUHE`, 23 `KIPREC`, 24 `KIUNUS`).
    pub fn f(&self, k: usize) -> i32 {
        self.info[3 + self.nbw + k]
    }
}

pub(super) const I_ADAT: usize = 2;
pub(super) const I_NDAT: usize = 3;
pub(super) const I_LNDA: usize = 4;
pub(super) const I_VER: usize = 5;
pub(super) const I_PROG: usize = 6;
pub(super) const I_DATE: usize = 8;
pub(super) const I_TIME: usize = 10;
pub(super) const I_TAG: usize = 12;
pub(super) const I_TYPE: usize = 14;
pub(super) const I_COMP: usize = 15;
pub(super) const I_QUAL: usize = 16;
pub(super) const I_AIHE: usize = 17;
pub(super) const I_NIHE: usize = 18;
pub(super) const I_ACHE: usize = 19;
pub(super) const I_NCHE: usize = 20;
pub(super) const I_AUHE: usize = 21;
pub(super) const I_NUHE: usize = 22;
pub(super) const I_PREC: usize = 23;
pub(super) const I_UNUS: usize = 24;

/// Where a new entry goes (`zcheck6.f` label 90).
#[derive(Debug, Clone)]
pub(super) struct Slot {
    pub hash: i32,
    /// `LINTAB`: the hash code has a bin.
    pub lintab: bool,
    /// Last bin of the chain (`IPBADD`) and the first free word in it (`JPNBIN`).
    pub bin_addr: i64,
    pub j: usize,
    /// `IPLOOP`: entries passed on the chain.
    pub iploop: i32,
}

/// A deleted entry with the same pathname, re-marked by `zcheck6.f` lines 294-312 when
/// writing (status + 2; the info block's status follows when it is 2).
#[derive(Debug, Clone)]
pub(super) struct Remark {
    pub bin_word: i64,
    pub status: i32,
    pub info_addr: i64,
}

#[derive(Debug, Clone)]
pub(super) enum Found {
    Yes(Existing),
    No(Slot),
}

#[derive(Debug, Clone)]
pub(super) struct Lookup {
    pub found: Found,
    pub remarks: Vec<Remark>,
}

/// The words of one record to store (what `zwritex6` receives).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Record6 {
    pub pathname: Vec<u8>,
    pub record_type: i32,
    pub internal_header: Vec<i32>,
    pub data: Vec<i32>,
    /// `NLDATA` (`KILNDA`, `KBLNDA`).
    pub logical: i32,
    pub quality: bool,
    pub precision: i32,
}

/// The stored parts of an existing record.
#[derive(Debug, Clone)]
pub(super) struct Stored {
    pub internal_header: Vec<i32>,
    pub compression_header: Vec<i32>,
    pub data: Vec<i32>,
}

pub(super) struct Engine<'a> {
    pub io: Io<'a>,
    pub h: [i32; NPERM],
    opts: &'a WriteOptions,
    pub block_index: usize,
    date: [u8; 7],
    time: [u8; 8],
    /// P38: `INTL_PSEUDO` of the records being stored (`KBPINTL`); 0 unless a pseudo-regular
    /// write sets it (`zsitsi6.f` `CALL zset6('PSEU', ...)`, `zsetfi6.f` line 91).
    pub pseudo_minutes: i32,
    /// P39: the user header of the next [`Engine::store`]; `None` stores none.
    pub next_user_header: Option<Vec<i32>>,
}

impl<'a> Engine<'a> {
    pub fn new(file: &'a File, opts: &'a WriteOptions) -> Result<Self, WriteError> {
        let io = Io::new(file);
        let len = io.len()?;
        if len < HEADER_BYTES as u64 {
            return Err(corrupt("file is shorter than a DSS6 header"));
        }
        let mut head = vec![0u8; HEADER_BYTES];
        {
            let mut f = file;
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut head)?;
        }
        let parsed = Dss6Header::parse(&head).map_err(|e| match e {
            crate::dss6::Dss6Error::NotVersion6 { .. } => {
                WriteError::Unsupported(format!("{e}; use the DSS7 writer for DSS7 files"))
            }
            other => corrupt(other.to_string()),
        })?;
        let mut h = [0i32; NPERM];
        for (i, w) in h.iter_mut().enumerate() {
            *w = i32::from_le_bytes([
                head[4 * i],
                head[4 * i + 1],
                head[4 * i + 2],
                head[4 * i + 3],
            ]);
        }
        if parsed.is_write_locked() {
            return Err(corrupt(
                "KFSIZE = -1357: the file was left in HEC's write-lock mode",
            ));
        }
        if parsed.version_number < KVERNO_6M {
            return Err(WriteError::Unsupported(format!(
                "DSS6 files before 6-M are not modified (version {}; bins of other sizes, \
                 lock record data)",
                parsed.version_string
            )));
        }
        // zgetag6.f lines 20-52: with a tag scheme (KTAGS, header words 21-36) HEC derives
        // each new record's tag from its pathname; the writer only implements the default
        // T<sequence> tags (lines 54-61).
        if h[21] > 0 {
            return Err(WriteError::Unsupported(
                "DSS6 files with a record-tag scheme (KTAGS) are not modified".into(),
            ));
        }
        if parsed.table_type != 1 {
            return Err(WriteError::Unsupported(
                "DSS6 files without a hash table (KTABLE = 2, 'STABLE') are not modified".into(),
            ));
        }
        if parsed.bin_size != BIN as i32 {
            return Err(corrupt(format!("KBNSIZ {} is not 127", parsed.bin_size)));
        }
        let phys = io.physical_words()?;
        let kfs = i64::from(h[K_FSIZE]);
        let kafbin = i64::from(h[K_AFBIN]);
        let kanbin = i64::from(h[K_ANBIN]);
        let khash = i64::from(h[K_HASH]);
        let kbnblk = i64::from(h[K_BNBLK]);
        let kbnrem = i64::from(h[K_BNREM]);
        let lock_end = parsed.lock_byte_range().1;
        if !(1..=8192).contains(&khash)
            || NPERM as i64 + khash >= kafbin
            || super::io::byte_of(kafbin) < lock_end
            || kfs - 1 > phys
            || kfs <= kafbin
            || !(1..=1024).contains(&kbnblk)
            || !(0..=kbnblk).contains(&kbnrem)
            || kanbin < kafbin
            || kanbin >= kfs
            || rec_word(kafbin).1 != 1
            || rec_word(kanbin).1 != 1
        {
            return Err(corrupt(format!(
                "header layout is not usable (KHASH {khash}, KAFBIN {kafbin}, KANBIN {kanbin}, \
                 KBNBLK {kbnblk}, KBNREM {kbnrem}, KFSIZE {kfs}, {phys} words in the file)"
            )));
        }
        let (date, time) = when();
        Ok(Engine {
            io,
            h,
            opts,
            block_index: 0,
            date,
            time,
            pseudo_minutes: 0,
            next_user_header: None,
        })
    }

    pub fn kverno(&self) -> i32 {
        let b = self.h[K_VERS].to_le_bytes();
        60_000 + 100 * i32::from(b[2]) + i32::from(b[3])
    }

    fn kfsize(&self) -> i64 {
        i64::from(self.h[K_FSIZE])
    }

    fn check_addr(&self, what: &str, addr: i64, n: usize) -> Result<(), WriteError> {
        if addr < 1 || addr + n as i64 > self.kfsize() {
            return Err(corrupt(format!(
                "{what} at address {addr} ({n} words) is outside KFSIZE {}",
                self.kfsize()
            )));
        }
        Ok(())
    }

    fn read_bin(&self, addr: i64) -> Result<Vec<i32>, WriteError> {
        if addr < i64::from(self.h[K_AFBIN]) || rec_word(addr).1 != 1 {
            return Err(corrupt(format!("bin address {addr} is not a bin")));
        }
        self.check_addr("bin", addr, BIN)?;
        self.io.read_words(addr, BIN)
    }

    /// `zcheck6` for a write: find `path` (already upper case), or where its entry goes.
    pub fn lookup(&self, path: &[u8]) -> Result<Lookup, WriteError> {
        let npath = path.len();
        let nbw_new = path_words(npath);
        if nbw_new + NLBIN + 2 > BIN {
            return Err(WriteError::BadPathname(
                "pathname does not fit a bin".into(),
            ));
        }
        let hash = zhash6(path, self.h[K_HASH]);
        let mut remarks = Vec::new();
        let slot_addr = NPERM as i64 + i64::from(hash);
        let mut bin_addr = i64::from(self.io.read_words(slot_addr, 1)?[0]);
        if bin_addr == 0 {
            return Ok(Lookup {
                found: Found::No(Slot {
                    hash,
                    lintab: false,
                    bin_addr: 0,
                    j: 0,
                    iploop: 0,
                }),
                remarks,
            });
        }
        let mut iploop = 0i32;
        let max_hops = (self.kfsize() / BIN as i64 + 2) as usize;
        let mut hops = 0usize;
        loop {
            hops += 1;
            if hops > max_hops {
                return Err(corrupt("overflow bin chain cycle"));
            }
            let bin = self.read_bin(bin_addr)?;
            let mut j = 1usize;
            loop {
                let st = bin[j - 1];
                if st == 0 {
                    return Ok(Lookup {
                        found: Found::No(Slot {
                            hash,
                            lintab: true,
                            bin_addr,
                            j,
                            iploop,
                        }),
                        remarks,
                    });
                }
                iploop += 1;
                if st == -1 {
                    bin_addr = i64::from(bin[BIN - 2]);
                    break;
                }
                if !(-1000..=1000).contains(&st) {
                    return Err(corrupt(format!("bin {bin_addr} word {j}: status {st}")));
                }
                let np = bin[j] as i64;
                if !(1..=392).contains(&np) {
                    return Err(corrupt(format!(
                        "bin {bin_addr} word {j}: pathname length {np}"
                    )));
                }
                let nbw = path_words(np as usize);
                if j + nbw + NLBIN - 1 > BIN {
                    return Err(corrupt(format!(
                        "bin {bin_addr} word {j}: entry passes the end of the bin"
                    )));
                }
                let same = np as usize == npath && entry_path(&bin, j, npath) == path;
                let ipstat = if st > 10 { st - 10 } else { st };
                if ipstat == 1 {
                    if same {
                        let info_addr = i64::from(bin[j + 1 + nbw]);
                        let nw = NINFO + nbw;
                        self.check_addr("info block", info_addr, nw)?;
                        let info = self.io.read_words(info_addr, nw)?;
                        // zowrit6.f lines 58-60: the info block must name the pathname.
                        if info[0] != INFO_FLAG
                            || info[2] as usize != npath
                            || chars(&info[3..3 + nbw], npath) != path
                        {
                            return Err(corrupt(format!(
                                "info block at {info_addr} does not match its bin entry"
                            )));
                        }
                        return Ok(Lookup {
                            found: Found::Yes(Existing {
                                bin_addr,
                                j,
                                nbw,
                                bin_type: bin[j + 5 + nbw],
                                info_addr,
                                info,
                            }),
                            remarks,
                        });
                    }
                } else if (st == 2 || st == 12) && same {
                    remarks.push(Remark {
                        bin_word: bin_addr + j as i64 - 1,
                        status: st + 2,
                        info_addr: i64::from(bin[j + 1 + nbw]),
                    });
                }
                j += nbw + NLBIN;
                if j > BIN - 2 {
                    return Ok(Lookup {
                        found: Found::No(Slot {
                            hash,
                            lintab: true,
                            bin_addr,
                            j,
                            iploop,
                        }),
                        remarks,
                    });
                }
            }
        }
    }

    /// Read the stored internal header, compression header and data of a record.
    pub fn read_stored(&self, ex: &Existing) -> Result<Stored, WriteError> {
        let read = |what: &str, a: i32, n: i32| -> Result<Vec<i32>, WriteError> {
            if !(0..=50_000_000).contains(&n) {
                return Err(corrupt(format!("{what} length {n}")));
            }
            if n == 0 {
                return Ok(Vec::new());
            }
            self.check_addr(what, i64::from(a), n as usize)?;
            self.io.read_words(i64::from(a), n as usize)
        };
        Ok(Stored {
            internal_header: read("internal header", ex.f(I_AIHE), ex.f(I_NIHE))?,
            compression_header: read("compression header", ex.f(I_ACHE), ex.f(I_NCHE))?,
            data: read("data", ex.f(I_ADAT), ex.f(I_NDAT))?,
        })
    }

    /// The stored user header (`KIAUHE`, `KINUHE`; dss6.md §14.1).
    pub fn read_user_header(&self, ex: &Existing) -> Result<Vec<i32>, WriteError> {
        let n = ex.f(I_NUHE);
        if !(0..=100_000).contains(&n) {
            return Err(corrupt(format!("user header length {n}")));
        }
        if n == 0 {
            return Ok(Vec::new());
        }
        self.check_addr("user header", i64::from(ex.f(I_AUHE)), n as usize)?;
        self.io.read_words(i64::from(ex.f(I_AUHE)), n as usize)
    }

    fn step(&self, step: WriteStep) -> Result<(), WriteError> {
        if self.io.planning() {
            return Ok(());
        }
        if let Some(fp) = self.opts.fault {
            if fp.block == self.block_index && fp.step == step {
                return Err(WriteError::InjectedFault {
                    block: self.block_index,
                    step,
                });
            }
        }
        Ok(())
    }

    fn write_header(&self, h: &[i32; NPERM]) -> Result<(), WriteError> {
        self.io.write_words(1, h)?;
        Ok(())
    }

    fn sync(&self) -> Result<(), WriteError> {
        self.io.sync(self.opts.sync)?;
        Ok(())
    }

    /// True when the stored record already holds exactly `rec` (P20 §3.3 item 11: an
    /// equal rewrite writes nothing).
    fn equal_stored(&self, ex: &Existing, rec: &Record6, uh: &[i32]) -> Result<bool, WriteError> {
        if ex.f(I_TYPE) != rec.record_type
            || ex.bin_type != rec.record_type
            || ex.f(I_COMP) != 0
            || ex.f(I_QUAL) != i32::from(rec.quality)
            || ex.f(I_PREC) != rec.precision
            || ex.f(I_LNDA) != rec.logical
            || ex.f(I_NUHE) as usize != uh.len()
            || ex.f(I_NCHE) != 0
            || ex.f(I_NIHE) as usize != rec.internal_header.len()
            || ex.f(I_NDAT) as usize != rec.data.len()
        {
            return Ok(false);
        }
        let s = self.read_stored(ex)?;
        Ok(s.internal_header == rec.internal_header
            && s.data == rec.data
            && self.read_user_header(ex)? == uh)
    }

    /// Store one record as one transaction (dss6.md §9.3; [`super::DSS6_STEP_ORDER`]).
    pub fn store(&mut self, rec: &Record6, look: Lookup) -> Result<BlockReport, WriteError> {
        let npath = rec.pathname.len();
        let nbw = path_words(npath);
        let ndata = rec.data.len();
        let nihead = rec.internal_header.len();
        // Read and checked before the first store (`store_all`, `location::store`). Every other
        // refusal below, for the whole write, is planned by `store_prepared`.
        let uh: Vec<i32> = self.next_user_header.take().unwrap_or_default();
        if ndata == 0 || ndata > i32::MAX as usize / 2 {
            return Err(WriteError::BadInput(format!(
                "record of {ndata} data words"
            )));
        }
        let mut nh = self.h;
        let old_kfs = self.kfsize();
        let mut kfs = old_kfs;
        let mut region: Vec<i32> = Vec::new();
        let put = |region: &mut Vec<i32>, addr: i64, w: &[i32]| {
            let off = (addr - old_kfs) as usize;
            if region.len() < off + w.len() {
                region.resize(off + w.len(), 0);
            }
            region[off..off + w.len()].copy_from_slice(w);
        };
        let mut chain: Option<(i64, i32)> = None;
        let mut link: Option<(i64, Vec<i32>)> = None;
        let mut publish: (i64, Vec<i32>) = (0, Vec::new());
        let mut retire: Option<i64> = None;
        let outcome;
        let info_addr;
        let old_info;
        // The info words common to both paths, filled below.
        let mut info: Vec<i32>;
        match &look.found {
            Found::Yes(ex) => {
                if self.equal_stored(ex, rec, &uh)? {
                    return Ok(BlockReport {
                        pathname: rec.pathname.clone(),
                        outcome: BlockOutcome::Unchanged,
                        record_type: 0,
                        info_address: ex.info_addr,
                        number_data: i64::from(ex.f(I_NDAT)),
                        logical_number: i64::from(ex.f(I_LNDA)),
                    });
                }
                // zowrit6.f lines 75-95 (new size larger: relocate). The writer always
                // relocates (dss6.md §9.3), with HEC's dead-space count for a relocation.
                let jsize = i64::from(ex.f(I_NDAT))
                    + i64::from(ex.f(I_NIHE))
                    + i64::from(ex.f(I_NCHE))
                    + i64::from(ex.f(I_NUHE))
                    + i64::from(ex.f(I_UNUS));
                nh[K_DEAD] = add_i32(nh[K_DEAD], jsize + (nbw + NINFO) as i64)?;
                info = ex.info.clone();
                info[1] = if ex.info[1] == 11 { 11 } else { 1 };
                let ver = (ex.f(I_VER).saturating_add(1)).min(999);
                info[3 + nbw + I_VER] = ver;
                info_addr = kfs;
                old_info = ex.info_addr;
                retire = Some(ex.info_addr);
                outcome = BlockOutcome::Replaced;
            }
            Found::No(slot) => {
                // znwrit6.f lines 40-73: statistics and part lengths.
                // znwrit6.f line 51 counts a hash code as used when the chain held no
                // entry (IPLOOP = 0). That also happens for a linked but empty bin, which
                // only an interrupted native write leaves (dss6.md §9.3); such a code is
                // already counted, so only a code without a bin counts here.
                if slot.iploop == 0 && !slot.lintab {
                    nh[K_HUSED] += 1;
                }
                if slot.iploop > nh[K_MAXPH] {
                    nh[K_MAXPH] = slot.iploop;
                    nh[K_MAXHC] = slot.hash;
                }
                let mut long = npath > 80;
                let inner = &rec.pathname[1..npath - 1];
                let mut mx = (nh[K_MXPRT] as u32 as u64) | ((nh[K_MXPRT + 1] as u32 as u64) << 32);
                for (i, p) in inner.split(|&c| c == b'/').enumerate().take(6) {
                    if p.len() > 32 {
                        long = true;
                    }
                    let cur = ((mx >> (8 * i)) & 0xff) as usize;
                    if p.len() > cur {
                        mx = (mx & !(0xffu64 << (8 * i))) | ((p.len() as u64 & 0xff) << (8 * i));
                    }
                }
                nh[K_MXPRT] = mx as u32 as i32;
                nh[K_MXPRT + 1] = (mx >> 32) as u32 as i32;
                // zgetag6.f lines 54-61: tag T<sequence number>.
                nh[K_SEQNO] = add_i32(nh[K_SEQNO], 1)?;
                let tag = format!("T{}", nh[K_SEQNO]);
                let tag_words = blank_padded(tag.as_bytes(), 2);
                nh[K_NRECS] = add_i32(nh[K_NRECS], 1)?;
                // Bin: znwrit6.f lines 77-104 (fit test line 98, -1 mark line 102).
                let (bin_addr, j) = if !slot.lintab {
                    let (b, c) = alloc_bin(&mut nh, &mut kfs, false)?;
                    chain = c;
                    link = Some((NPERM as i64 + i64::from(slot.hash), vec![b as i32]));
                    (b, 1usize)
                } else if slot.j + nbw + NLBIN + 2 > BIN {
                    if slot.j > BIN - 2 {
                        return Err(corrupt(format!(
                            "bin {} has no room for the overflow mark (word {})",
                            slot.bin_addr, slot.j
                        )));
                    }
                    let (b, c) = alloc_bin(&mut nh, &mut kfs, true)?;
                    chain = c;
                    // Status -1 at JPNBIN and the overflow address in word 126 (one write).
                    let mut w = vec![0i32; BIN - 1 - slot.j + 1];
                    w[0] = -1;
                    *w.last_mut().expect("non-empty") = b as i32;
                    link = Some((slot.bin_addr + slot.j as i64 - 1, w));
                    (b, 1usize)
                } else {
                    (slot.bin_addr, slot.j)
                };
                info_addr = kfs;
                old_info = 0;
                outcome = BlockOutcome::Added;
                let status = if long { 11 } else { 1 };
                // Bin entry: znwrit6.f lines 107-148.
                let mut entry = Vec::with_capacity(nbw + NLBIN);
                entry.push(status);
                entry.push(npath as i32);
                entry.extend(blank_padded(&rec.pathname, nbw));
                entry.push(info_addr as i32); // KBAINF
                entry.push(0); // KBNHEA
                entry.push(ndata as i32); // KBNDAT
                entry.push(rec.logical); // KBLNDA
                entry.push(rec.record_type); // KBTYPE
                entry.extend_from_slice(&tag_words); // KBTAG
                entry.push(slot.hash); // KBHASH
                entry.push(self.pseudo_minutes); // KBPINTL

                // Entry plus zeros to word 125 (the end of the entry area), one write.
                let mut w = entry;
                let tail = (BIN - 2).saturating_sub(j + w.len() - 1);
                w.extend(std::iter::repeat(0).take(tail));
                publish = (bin_addr + j as i64 - 1, w);
                // Info block: znwrit6.f lines 152-210.
                info = vec![0i32; NINFO + nbw];
                info[0] = INFO_FLAG;
                info[1] = status;
                info[2] = npath as i32;
                info[3..3 + nbw].copy_from_slice(&blank_padded(&rec.pathname, nbw));
                info[3 + nbw + I_VER] = 1; // IBVER, zinit6.f line 274
                info[3 + nbw + I_TAG] = tag_words[0];
                info[3 + nbw + I_TAG + 1] = tag_words[1];
            }
        }
        // Layout and the fields both paths set (znwrit6.f lines 154-205; zowrit6.f lines
        // 131-188).
        let aihe = info_addr + (NINFO + nbw) as i64;
        let auhe = aihe + nihead as i64;
        let adat = auhe + uh.len() as i64;
        let new_kfs = adat + ndata as i64;
        if new_kfs > i64::from(i32::MAX) - 2 * WORDS_PER_RECORD {
            return Err(WriteError::Unsupported(
                "the file would exceed DSS6's 32-bit word addresses".into(),
            ));
        }
        {
            let p = |k: usize| 3 + nbw + k;
            info[p(I_ADAT)] = adat as i32;
            info[p(I_NDAT)] = ndata as i32;
            info[p(I_LNDA)] = rec.logical;
            let mut prog = [0i32; 2];
            put_chars(&mut prog, PROGRAM_NAME6);
            info[p(I_PROG)..p(I_PROG) + 2].copy_from_slice(&prog);
            let mut d = [0i32; 2];
            put_chars(&mut d, &self.date);
            info[p(I_DATE)..p(I_DATE) + 2].copy_from_slice(&d);
            let mut t = [0i32; 2];
            put_chars(&mut t, &self.time);
            info[p(I_TIME)..p(I_TIME) + 2].copy_from_slice(&t);
            info[p(I_TYPE)] = rec.record_type;
            info[p(I_COMP)] = 0;
            info[p(I_QUAL)] = i32::from(rec.quality);
            info[p(I_AIHE)] = aihe as i32;
            info[p(I_NIHE)] = nihead as i32;
            info[p(I_ACHE)] = auhe as i32;
            info[p(I_NCHE)] = 0;
            info[p(I_AUHE)] = auhe as i32;
            info[p(I_NUHE)] = uh.len() as i32;
            info[p(I_PREC)] = rec.precision;
            info[p(I_UNUS)] = 0;
        }
        if let Found::Yes(ex) = &look.found {
            // Bin words KBAINF..KBTYPE (zowrit6.f lines 84-92), one write.
            let at = ex.bin_addr + (ex.j + 2 + ex.nbw) as i64 - 1;
            let mut words = vec![
                info_addr as i32,
                uh.len() as i32, // KBNHEA
                ndata as i32,
                rec.logical,
                rec.record_type,
            ];
            // P38: zsetfi6.f lines 91-99 set KBPINTL after the write; here in the same write
            // (the words between are unchanged).
            let rest = self.io.read_words(at + 5, 4)?;
            if rest[3] != self.pseudo_minutes {
                words.extend_from_slice(&rest[..3]);
                words.push(self.pseudo_minutes);
            }
            publish = (at, words);
        }
        put(&mut region, info_addr, &info);
        put(&mut region, aihe, &rec.internal_header);
        put(&mut region, auhe, &uh);
        put(&mut region, adat, &rec.data);
        if region.len() as i64 != new_kfs - old_kfs {
            region.resize((new_kfs - old_kfs) as usize, 0);
        }
        nh[K_FSIZE] = new_kfs as i32;
        // zptrec6.f lines 279-283: the first write after an open stamps the file's
        // last-written date (7 characters; the eighth byte is kept) and time.
        {
            let mut d = [0u8; 8];
            d.copy_from_slice(&[nh[K_LWDAT].to_le_bytes(), nh[K_LWDAT + 1].to_le_bytes()].concat());
            d[..7].copy_from_slice(&self.date);
            nh[K_LWDAT] = i32::from_le_bytes([d[0], d[1], d[2], d[3]]);
            nh[K_LWDAT + 1] = i32::from_le_bytes([d[4], d[5], d[6], d[7]]);
            put_chars(&mut nh[K_LWTIM..K_LWTIM + 2], &self.time);
        }

        // ---- the transaction ----
        // Data: the record and any new bin block, all at or after KFSIZE.
        self.io.write_words(old_kfs, &region)?;
        self.step(WriteStep::Data)?;
        // EndOfFile: -9630 at the new KFSIZE, the rest of its record zero (zptrec6.f lines
        // 197-210).
        let (_, w) = rec_word(new_kfs);
        let mut eof = vec![0i32; (WORDS_PER_RECORD - w + 1) as usize];
        eof[0] = EOF_FLAG;
        self.io.write_words(new_kfs, &eof)?;
        self.sync()?;
        self.step(WriteStep::EndOfFile)?;
        // Allocation: KFSIZE and the marker.
        let mut ha = self.h;
        ha[K_FSIZE] = new_kfs as i32;
        ha[K_MARK_OLD] = old_info as i32;
        ha[K_MARK_NEW] = info_addr as i32;
        self.write_header(&ha)?;
        self.sync()?;
        self.step(WriteStep::Allocation)?;
        // Chain: the new bin block from the last bin of the previous block (znwbin6.f
        // lines 76-82).
        if let Some((a, v)) = chain {
            self.io.write_words(a, &[v])?;
            self.sync()?;
        }
        self.step(WriteStep::Chain)?;
        // Counters and bin allocation, marker still set.
        nh[K_MARK_OLD] = old_info as i32;
        nh[K_MARK_NEW] = info_addr as i32;
        self.write_header(&nh)?;
        self.sync()?;
        self.step(WriteStep::Counters)?;
        // Link: table slot, or overflow status and pointer.
        if let Some((a, w)) = &link {
            self.io.write_words(*a, w)?;
            self.sync()?;
        }
        self.step(WriteStep::Link)?;
        // Publish.
        for r in &look.remarks {
            self.io.write_words(r.bin_word, &[r.status])?;
            if r.info_addr > 0 && r.info_addr + 1 < new_kfs {
                let st = self.io.read_words(r.info_addr, 2)?;
                if st[0] == INFO_FLAG && st[1] == 2 {
                    self.io.write_words(r.info_addr + 1, &[r.status])?;
                }
            }
        }
        self.io.write_words(publish.0, &publish.1)?;
        self.sync()?;
        self.step(WriteStep::Publish)?;
        // Retire: the old info block is marked relocated (zowrit6.f line 77).
        if let Some(a) = retire {
            self.io.write_words(a + 1, &[-1])?;
            self.sync()?;
        }
        self.step(WriteStep::Retire)?;
        // Finish.
        nh[K_MARK_OLD] = 0;
        nh[K_MARK_NEW] = 0;
        self.write_header(&nh)?;
        self.sync()?;
        self.step(WriteStep::Finish)?;
        self.h = nh;
        Ok(BlockReport {
            pathname: rec.pathname.clone(),
            outcome,
            record_type: rec.record_type,
            info_address: info_addr,
            number_data: ndata as i64,
            logical_number: i64::from(rec.logical),
        })
    }

    /// Store records built in full beforehand, each as its own transaction. Every check
    /// that can refuse a record runs before the first one is written (the builders validate
    /// the data, and [`Engine::store_prepared`] plans counters and address growth).
    pub fn store_all(&mut self, recs: &[Record6]) -> Result<Vec<BlockReport>, WriteError> {
        // P39 (deliberate difference from zsrtsi6.f, which drops it; P27: it is the DSS6
        // location's supplemental text): a rewrite of a time series keeps the stored user
        // header. Every header is read and checked here, before the first store, so a bad
        // one on a later block leaves the file unchanged.
        let mut headers = Vec::with_capacity(recs.len());
        for rec in recs {
            let look = self.lookup(&rec.pathname)?;
            headers.push(match &look.found {
                Found::Yes(ex) if (100..=119).contains(&rec.record_type) => {
                    self.read_user_header(ex)?
                }
                _ => Vec::new(),
            });
        }
        self.store_prepared(recs, headers)
    }

    /// Store records with the user headers already read and checked, one per record.
    ///
    /// P38 re-check: a multi-record write first runs with its writes held in memory
    /// ([`Io::set_planning`]), so counters, address growth and bins carried forward from the
    /// earlier records refuse with the file untouched, including actual growth past DSS6's
    /// 32-bit addresses (`store`). A single record refuses in `store` before its first write.
    pub fn store_prepared(
        &mut self,
        recs: &[Record6],
        headers: Vec<Vec<i32>>,
    ) -> Result<Vec<BlockReport>, WriteError> {
        if recs.len() > 1 {
            let saved = (self.h, self.block_index);
            self.io.set_planning(true);
            let planned = self.store_each(recs, headers.clone());
            self.io.set_planning(false);
            (self.h, self.block_index) = saved;
            self.next_user_header = None;
            planned?;
            return self
                .store_each(recs, headers)
                .map_err(|e| e.after_planning(recs.len()));
        }
        self.store_each(recs, headers)
    }

    fn store_each(
        &mut self,
        recs: &[Record6],
        headers: Vec<Vec<i32>>,
    ) -> Result<Vec<BlockReport>, WriteError> {
        let mut out = Vec::with_capacity(recs.len());
        for (i, (rec, header)) in recs.iter().zip(headers).enumerate() {
            self.block_index = i;
            // Look up again: earlier stores of this write moved bins and counters.
            let look = self.lookup(&rec.pathname)?;
            self.next_user_header = Some(header);
            out.push(self.store(rec, look)?);
        }
        Ok(out)
    }

    /// Repair a file whose marker shows an interrupted native write (dss6.md §9.3): retire
    /// the old copy of a rewrite whose new copy was published, rebuild `KNRECS`,
    /// `KHUSED`, `KBINS` and `KBOVER` from the bins, and clear the marker. Never clears a
    /// bin or moves a pointer. Returns whether there was anything to reconcile.
    pub fn reconcile(&mut self) -> Result<bool, WriteError> {
        let new = i64::from(self.h[K_MARK_NEW]);
        let old = i64::from(self.h[K_MARK_OLD]);
        if new == 0 && old == 0 {
            return Ok(false);
        }
        let kfs = self.kfsize();
        if new > 0 && new + 3 < kfs {
            let head = self.io.read_words(new, 3)?;
            let np = head[2] as i64;
            if head[0] == INFO_FLAG && (1..=392).contains(&np) {
                let nbw = path_words(np as usize);
                let words = self.io.read_words(new + 3, nbw)?;
                let path = chars(&words, np as usize);
                // The old copy of a rewrite is dead once the live entry of its pathname
                // names another info block: the native copy (published), or a copy HEC
                // wrote after the interruption. It is retired as zowrit6.f line 77 retires
                // a relocated block, after checking that it names the same pathname.
                if let Found::Yes(ex) = self.lookup(&path)?.found {
                    if old > 0 && ex.info_addr != old && old + 3 + nbw as i64 <= kfs {
                        let o = self.io.read_words(old, 3 + nbw)?;
                        if o[0] == INFO_FLAG
                            && o[1] != -1
                            && o[2] as usize == path.len()
                            && chars(&o[3..3 + nbw], path.len()) == path
                        {
                            self.io.write_words(old + 1, &[-1])?;
                        }
                    }
                }
            }
        }
        self.rebuild_counts()?;
        self.h[K_MARK_OLD] = 0;
        self.h[K_MARK_NEW] = 0;
        let h = self.h;
        self.write_header(&h)?;
        self.sync()?;
        Ok(true)
    }

    /// `KNRECS` = live entries in the bin blocks (what `zcatfi6` counts), `KHUSED` =
    /// nonzero table slots, `KBOVER` = bins with an overflow pointer, `KBINS` = bins
    /// handed out (`znwbin6.f` line 30).
    fn rebuild_counts(&mut self) -> Result<(), WriteError> {
        let khash = self.h[K_HASH] as usize;
        let table = self.io.read_words(NPERM as i64 + 1, khash)?;
        let used = table.iter().filter(|&&a| a != 0).count();
        let kbnblk = i64::from(self.h[K_BNBLK]);
        let mut addr = i64::from(self.h[K_AFBIN]);
        let (mut live, mut over, mut blocks) = (0i64, 0i64, 0i64);
        // Blocks up to the one holding KANBIN are allocated; a block chained after it is
        // one an interrupted write added before its Counters step (empty, never used).
        let kanbin = i64::from(self.h[K_ANBIN]);
        let mut allocated_blocks = 0i64;
        let max_blocks = self.kfsize() / (kbnblk * BIN as i64) + 2;
        loop {
            blocks += 1;
            if blocks > max_blocks {
                return Err(corrupt("bin block chain cycle"));
            }
            if (addr..addr + kbnblk * BIN as i64).contains(&kanbin) {
                allocated_blocks = blocks;
            }
            let mut last = Vec::new();
            for k in 0..kbnblk {
                let b = self.read_bin(addr + k * BIN as i64)?;
                let mut j = 1usize;
                while j <= BIN - 2 {
                    let st = b[j - 1];
                    if st == 0 || st == -1 {
                        break;
                    }
                    if st == 1 || st == 11 {
                        live += 1;
                    }
                    let np = b[j] as usize;
                    if !(1..=392).contains(&np) {
                        break;
                    }
                    j += path_words(np) + NLBIN;
                }
                if b[BIN - 2] != 0 {
                    over += 1;
                }
                last = b;
            }
            let next = i64::from(last[BIN - 1]);
            if next == 0 {
                break;
            }
            addr = next;
        }
        self.h[K_NRECS] = live as i32;
        self.h[K_HUSED] = used as i32;
        self.h[K_BOVER] = over as i32;
        let allocated = if allocated_blocks > 0 {
            allocated_blocks
        } else {
            blocks
        };
        self.h[K_BINS] = (allocated * kbnblk - i64::from(self.h[K_BNREM])) as i32;
        Ok(())
    }
}

/// Keep the stored record's time-zone and coordinate words in a rewritten internal
/// header (P22 spec §5, deviation): HEC's store routines write their own defaults there
/// (`IWTZONE`/`CWTZONE`, zero coordinates; `zsrtsi6.f` lines 876-887, `zsitsi6.f` lines
/// 927-940), dropping what the record held; the DSS6 writer takes neither as input, so it
/// keeps them. `zone` and `coords` are the 0-based word ranges.
pub(super) fn keep_metadata(
    new: &mut [i32],
    stored: &[i32],
    zone: std::ops::Range<usize>,
    coords: std::ops::Range<usize>,
) {
    for r in [zone, coords] {
        for i in r {
            if let (Some(n), Some(&s)) = (new.get_mut(i), stored.get(i)) {
                *n = s;
            }
        }
    }
}

fn add_i32(a: i32, b: i64) -> Result<i32, WriteError> {
    i32::try_from(i64::from(a) + b)
        .map_err(|_| WriteError::Unsupported("a DSS6 header counter would overflow".into()))
}

fn chars(words: &[i32], n: usize) -> Vec<u8> {
    let mut v: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    v.truncate(n);
    v
}

fn entry_path(bin: &[i32], j: usize, n: usize) -> Vec<u8> {
    let nbw = path_words(n);
    chars(&bin[j + 1..j + 1 + nbw], n)
}

/// `znwbin6`: hand out the next pathname bin, starting a new bin block at the end of the
/// file when the current one is used up. Returns the bin address and, for a new block,
/// the chain write (address of word 127 of the previous block's last bin, new block
/// address). `overflow` counts an overflow bin (`KBOVER`, line 64).
fn alloc_bin(
    nh: &mut [i32; NPERM],
    kfs: &mut i64,
    overflow: bool,
) -> Result<(i64, Option<(i64, i32)>), WriteError> {
    nh[K_BINS] = add_i32(nh[K_BINS], 1)?;
    let mut chain = None;
    if nh[K_BNREM] <= 0 {
        // Lines 38-50: keep bin blocks on record boundaries.
        let (rec, w) = rec_word(*kfs);
        if w != 1 {
            *kfs = rec * WORDS_PER_RECORD + 1;
        }
        // Lines 67-75: the last bin of the current block points to the new block.
        chain = Some((i64::from(nh[K_ANBIN]) + BIN as i64 - 1, *kfs as i32));
        nh[K_BNREM] = nh[K_BNBLK] - 1;
        nh[K_ANBIN] = *kfs as i32;
        *kfs += i64::from(nh[K_BNBLK]) * BIN as i64;
    } else {
        nh[K_BNREM] -= 1;
    }
    if overflow {
        nh[K_BOVER] = add_i32(nh[K_BOVER], 1)?;
    }
    let addr = i64::from(nh[K_ANBIN]);
    if nh[K_BNREM] > 0 {
        nh[K_ANBIN] += BIN as i32;
    }
    Ok((addr, chain))
}

/// Run a write: lock guard (P19a policy), held write lock at `KLOCKB`, header re-read,
/// reconciliation, then `body` block by block.
pub(super) fn write_with<F>(
    file: &File,
    opts: &WriteOptions,
    body: F,
) -> Result<WriteReport, WriteError>
where
    F: FnOnce(&mut Engine) -> Result<Vec<BlockReport>, WriteError>,
{
    let report = lock::lock_status_of(file).map_err(WriteError::Lock)?;
    let guard =
        lock::ensure_unlocked_for_write(&report, &opts.lock_policy).map_err(WriteError::Locked)?;
    // Header checks before taking the lock (a non-DSS6 file is refused cleanly).
    let klockb = {
        let eng = Engine::new(file, opts)?;
        let mut head = vec![0u8; HEADER_BYTES];
        for (i, w) in eng.h.iter().enumerate() {
            head[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        Dss6Header::parse(&head)
            .map_err(|e| corrupt(e.to_string()))?
            .lock_byte_range()
            .0
    };
    let mut lock_method = "none";
    let held = if opts.lock_policy.hold_primary_lock_during_write {
        // HEC's DSS6 write lock is the word at KLOCKB (zmultu6.f lines 113-114); POSIX
        // holds KLOCKB..+8 (write and request words), Windows the whole file
        // (P20 lockhold).
        match lockhold::take(file, klockb, &[]) {
            Ok(l) => {
                lock_method = l.method;
                Some(l)
            }
            Err(lockhold::HoldError::Busy) => {
                return Err(WriteError::Locked(LockRefusal {
                    reasons: vec![LockReason::PrimaryWrite],
                    readers_undetectable: false,
                    holder_pids: Vec::new(),
                }))
            }
            Err(lockhold::HoldError::Io(e)) => return Err(e.into()),
        }
    } else {
        None
    };
    // HEC re-reads the header once it holds the lock (zmultu6 -> zrdprm6).
    let mut eng = Engine::new(file, opts)?;
    let reconciled = eng.reconcile()?;
    let blocks = body(&mut eng)?;
    if opts.sync {
        file.sync_all()?;
    }
    drop(held);
    Ok(WriteReport {
        blocks,
        readers_undetected: guard.readers_undetected,
        reconciled_interrupted_write: reconciled,
        lock_method,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_file_matches_monolith_layout() {
        let w = new_file_words(b"30OCT26", b"15:53:46");
        assert_eq!(w.len(), 6477);
        let nonzero: Vec<(usize, i32)> = w
            .iter()
            .enumerate()
            .filter(|&(i, &v)| v != 0 && ![0, 4, 7, 8, 9, 10, 11, 12].contains(&i))
            .map(|(i, &v)| (i, v))
            .collect();
        // Non-zero words of an empty file created by Monolith 3.3.27 (6-YO), other than
        // the signature, version and date/time words.
        let expect: &[(usize, i32)] = &[
            (3, 6),
            (5, 6351),
            (14, 2048),
            (15, 1),
            (16, 32),
            (17, 32),
            (18, 127),
            (19, 2287),
            (20, 2287),
            (44, 100),
            (45, 200),
            (46, 200),
            (47, 200),
            (48, 200),
            (49, 200),
            (2160, -9630),
            (6350, -9630),
        ];
        assert_eq!(nonzero, expect);
        assert_eq!(&w[0].to_le_bytes(), b"ZDSS");
        assert_eq!(&w[4].to_le_bytes(), b"6-YO");
        let mut d = [0i32; 2];
        put_chars(&mut d, b"30OCT26");
        assert_eq!(&w[7..9], &d);
    }

    #[test]
    fn when_format() {
        let (d, t) = when();
        assert!(d[2..5].iter().all(u8::is_ascii_uppercase));
        assert_eq!(t[2], b':');
    }
}

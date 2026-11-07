//! DSS6 point lookup through the hash table (dss6.md §4.6).
//!
//! Sources: `heclib_f/src/zcheck6.f:ZCHECK6` lines 57-97 (length check, normalization,
//! pseudo-regular conversion), 162-206 (hash and table address), 211-322 (bin chain
//! scan); `heclib_f/src/zrdinf6.f:ZRDINF6` lines 56-62 (info block cross-check).

use super::hash::{hash_path, normalize_pathname, zhash6};
use super::info::{path_words, unpack_chars, Dss6Info};
use super::pathname::pseudo_to_stored;
use super::walk::Dss6BinEntry;
use super::{Dss6Error, Dss6File, ReadAt, MAX_PATHNAME, NLBIN, NPERM};

/// A record found by [`Dss6File::lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dss6Record {
    pub entry: Dss6BinEntry,
    pub info: Dss6Info,
}

impl<S: ReadAt> Dss6File<S> {
    /// Find a live record by pathname. The pathname is normalized as HEC does (upper
    /// case, control characters to `?`, a `~<interval>` E part to its stored `IR-*`
    /// form). Returns `Ok(None)` when the record is not in the file, and an error when
    /// the structures on the lookup path are damaged.
    pub fn lookup(&self, pathname: &[u8]) -> Result<Option<Dss6Record>, Dss6Error> {
        let h = &self.header;
        let mut path = normalize_pathname(pathname);
        // zcheck6.f lines 88-97: `/~` after position 5 marks a pseudo-regular path.
        if path
            .windows(2)
            .position(|w| w == b"/~")
            .is_some_and(|i| i + 1 > 5)
        {
            if let Some((stored, _)) = pseudo_to_stored(&path) {
                path = stored;
            }
        }
        let npath = path.len();
        // zcheck6.f lines 57-64: the entry must fit in a bin.
        if npath == 0
            || npath > MAX_PATHNAME as usize
            || path_words(npath as i32) + NLBIN + 2 > h.bin_size as usize
        {
            return Ok(None);
        }
        let ihash = zhash6(&hash_path(&path), h.max_hash);
        let bnsiz = h.bin_size as usize;
        let mut bin_addr = if h.table_type == 1 {
            i64::from(self.read_words(NPERM as i64 + ihash as i64, 1)?[0])
        } else {
            i64::from(h.first_bin_address) + i64::from(ihash - 1) * i64::from(h.bin_size)
        };
        let max_hops = (self.limit_words / i64::from(h.bin_size) + 2) as usize;
        let mut hops = 0usize;
        'chain: while bin_addr != 0 {
            hops += 1;
            if hops > max_hops {
                return Err(Dss6Error::Corrupt {
                    address: Some(bin_addr),
                    detail: "overflow chain cycle".into(),
                });
            }
            let bin = self.read_words(bin_addr, bnsiz)?;
            let mut k = 0usize;
            while k < bnsiz {
                let status = bin[k];
                if status == -1 {
                    // Bin full: continue at the overflow bin in word KBNSIZ-1.
                    bin_addr = i64::from(bin[bnsiz - 2]);
                    continue 'chain;
                }
                if status <= 0 {
                    return Ok(None);
                }
                let corrupt = |detail: String| Dss6Error::Corrupt {
                    address: Some(bin_addr + k as i64),
                    detail,
                };
                let n = *bin
                    .get(k + 1)
                    .ok_or_else(|| corrupt("entry crosses bin end".into()))?;
                if !(1..=MAX_PATHNAME).contains(&n) {
                    return Err(corrupt(format!("pathname length {n}")));
                }
                let p = path_words(n);
                if k + p + NLBIN > bnsiz {
                    return Err(corrupt("entry crosses bin end".into()));
                }
                // zcheck6.f lines 255-260: statuses 11..14 compare as 1..4.
                let s = if status > 10 { status - 10 } else { status };
                if s == 1
                    && n as usize == npath
                    && unpack_chars(&bin[k + 2..k + 2 + p], npath) == path
                {
                    return self.finish_lookup(&bin, bin_addr, k, p).map(Some);
                }
                k += p + NLBIN;
                if k + 1 > bnsiz - 2 {
                    return Ok(None);
                }
            }
            return Ok(None);
        }
        Ok(None)
    }

    fn finish_lookup(
        &self,
        bin: &[i32],
        bin_addr: i64,
        k: usize,
        p: usize,
    ) -> Result<Dss6Record, Dss6Error> {
        let o = k + 2 + p;
        let npath = bin[k + 1];
        let mut tag = [0u8; 8];
        tag[..4].copy_from_slice(&bin[o + 5].to_le_bytes());
        tag[4..].copy_from_slice(&bin[o + 6].to_le_bytes());
        let entry = Dss6BinEntry {
            status: bin[k],
            pathname: unpack_chars(&bin[k + 2..k + 2 + p], npath as usize),
            info_address: bin[o],
            user_header_words: bin[o + 1],
            data_words: bin[o + 2],
            logical_values: bin[o + 3],
            record_type: bin[o + 4],
            tag,
            hash: bin[o + 7],
            pseudo_interval: bin[o + 8],
            bin_address: bin_addr,
            entry_address: bin_addr + k as i64,
        };
        let info = self.read_info(entry.info_address.into(), npath)?;
        // zrdinf6.f lines 56-62 check length and pathname; the flag check is dss6.md §4.6.
        if !info.has_valid_flag() || info.pathname != entry.pathname {
            return Err(Dss6Error::Corrupt {
                address: Some(entry.info_address.into()),
                detail: format!("info block does not match bin entry (flag {})", info.flag),
            });
        }
        Ok(Dss6Record { entry, info })
    }
}

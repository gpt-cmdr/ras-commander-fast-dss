//! Planning overlay shared by the DSS7 and DSS6 writers: before the first store
//! of a multi-record write, the commit code runs once with its writes held here and its reads
//! seeing them, so every refusal that depends on the file, including bin allocation, chain
//! links, counters and address growth carried forward from earlier records, happens with the
//! file untouched.
//!
//! Not a general interval store: a large write that overlaps an earlier large one would shadow
//! its tail. The commit paths never do that (record and bin regions are appended once; later
//! pointer and status changes are small writes).

use std::collections::{BTreeMap, HashMap};

/// Writes up to this many words are kept word by word; longer ones (record regions, written
/// once at the end of the file) as one array.
const SMALL_WRITE_WORDS: usize = 1024;

pub(crate) struct Overlay<W: Copy> {
    small: HashMap<i64, W>,
    big: BTreeMap<i64, Vec<W>>,
    /// One past the highest word written.
    end: i64,
}

impl<W: Copy> Overlay<W> {
    pub fn new() -> Self {
        Overlay {
            small: HashMap::new(),
            big: BTreeMap::new(),
            end: 0,
        }
    }

    pub fn end(&self) -> i64 {
        self.end
    }

    pub fn put(&mut self, addr: i64, words: &[W]) {
        let stop = addr + words.len() as i64;
        self.end = self.end.max(stop);
        if words.len() <= SMALL_WRITE_WORDS {
            for (i, &w) in words.iter().enumerate() {
                self.small.insert(addr + i as i64, w);
            }
        } else {
            // Later than any small write in its range, which lookups would otherwise prefer.
            if self.small.len() < words.len() {
                self.small.retain(|a, _| !(addr..stop).contains(a));
            } else {
                for a in addr..stop {
                    self.small.remove(&a);
                }
            }
            self.big.insert(addr, words.to_vec());
        }
    }

    fn get(&self, addr: i64) -> Option<W> {
        if let Some(&w) = self.small.get(&addr) {
            return Some(w);
        }
        let (start, v) = self.big.range(..=addr).next_back()?;
        v.get((addr - start) as usize).copied()
    }

    /// `n` words at `addr`: held words, and the file's for each run of the others.
    pub fn read<E>(
        &self,
        addr: i64,
        n: usize,
        mut file: impl FnMut(i64, usize) -> Result<Vec<W>, E>,
    ) -> Result<Vec<W>, E> {
        let mut out = Vec::with_capacity(n);
        let stop = addr + n as i64;
        let mut a = addr;
        while a < stop {
            if let Some(w) = self.get(a) {
                out.push(w);
                a += 1;
            } else {
                let mut run = a + 1;
                while run < stop && self.get(run).is_none() {
                    run += 1;
                }
                out.extend(file(a, (run - a) as usize)?);
                a = run;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_see_writes_and_the_file_around_them() {
        let mut o: Overlay<i32> = Overlay::new();
        o.put(10, &[1, 2]);
        let big = vec![7; 2000];
        o.put(100, &big);
        o.put(150, &[9]);
        let r = o.read::<()>(9, 4, |a, n| Ok(vec![-(a as i32); n])).unwrap();
        assert_eq!(r, vec![-9, 1, 2, -12]);
        assert_eq!(o.read::<()>(149, 3, |_, _| Err(())).unwrap(), vec![7, 9, 7]);
        assert_eq!(o.end(), 2100);
        // A big write replaces an earlier small one in its range.
        let mut o: Overlay<i32> = Overlay::new();
        o.put(5, &[1]);
        o.put(5, &big);
        assert_eq!(o.read::<()>(5, 1, |_, _| Err(())).unwrap(), vec![7]);
        // The same when more small words are held than the big write has.
        let mut o: Overlay<i32> = Overlay::new();
        for a in 0..3001 {
            o.put(a, &[1]);
        }
        o.put(1000, &big);
        let r = o.read::<()>(999, 2002, |_, _| Err(())).unwrap();
        assert_eq!((r[0], r[1], r[2000], r[2001]), (1, 7, 7, 1));
    }
}

//! Dataset date-range index for whole-dataset DSS7 reads.
//!
//! A whole-dataset read (`ztsGetDateRange(path, boolFullSet = 1)`,
//! `heclib_c/src/Public/ztsGetDateRange.c`) first finds the first and last block
//! pathnames with `ztsGetFirstLastPathnames()`
//! (`heclib_c/src/SemiPublic/ztsGetFirstLastPathnames.c`): a catalog with the D part
//! wild and record types 100..199, keeping the smallest and largest D-part date. The
//! engine does that with a linear scan of a caller's [`Dss7Catalog`]
//! ([`dataset_extrema_scan`]): O(N) per dataset, O(D·N) for an enumeration of D
//! datasets.
//!
//! [`IndexedDss7Catalog`] owns a catalog snapshot that cannot be edited after it is
//! wrapped, and groups it once, in O(N), on the first whole-dataset read; each dataset
//! is then answered in O(1). Readers that cache one catalog per open handle (the Python
//! and WebAssembly bindings) cache this wrapper instead and drop the index with the
//! snapshot. Every API that takes a caller's `&Dss7Catalog` keeps the scan, so edits to
//! its public fields are honoured exactly as before.
//!
//! The index reproduces the scan's candidate rules exactly:
//!
//! - every physical entry is a candidate, in catalog order, including recovered entries
//!   of a recovery-mode catalog, when its `raw_status` (from `meta`; `-1` when `meta` is
//!   shorter than `entries`) is 0..=2 and its record type is 100..199;
//! - its stored pathname must split into six parts (`path_parts`);
//! - the dataset key is the A, B, C, E and F parts compared ASCII-case-insensitively
//!   (`eq_ignore_ascii_case`). The stored E part is used as stored: it is not
//!   canonicalised, so it matches the caller's canonical E part only when the scan
//!   would;
//! - a D part that is not a date contributes nothing (HEC's `dateToJulian()` yields
//!   `UNDEFINED_TIME`).
//!
//! Only the extrema are kept: the scan's result depends on nothing else (candidate
//! order, duplicates and deleted copies only ever feed `min`/`max`). The bin-date and
//! block-length steps that follow are unchanged in `Dss7File::dataset_date_range`.

use super::interval as iv;
use super::ts::path_parts;
use super::Dss7Catalog;
use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// The A, B, C, E, F key of a pathname split by `path_parts`: lower-cased parts joined
/// by `/`. No part contains `/`, so the join is injective, and byte-wise lower-case
/// equality is exactly `eq_ignore_ascii_case` on every part.
fn key(parts: &[&[u8]; 6]) -> Vec<u8> {
    let n: usize = [0usize, 1, 2, 4, 5]
        .iter()
        .map(|&k| parts[k].len() + 1)
        .sum();
    let mut v = Vec::with_capacity(n);
    for k in [0usize, 1, 2, 4, 5] {
        v.extend(parts[k].iter().map(u8::to_ascii_lowercase));
        v.push(b'/');
    }
    v
}

/// The scan's candidate test and date for catalog entry `i`.
fn candidate(cat: &Dss7Catalog, i: usize) -> Option<([&[u8]; 6], i64)> {
    let e = &cat.catalog.entries[i];
    let st = cat.meta.get(i).map(|m| m.raw_status).unwrap_or(-1);
    if !(0..=2).contains(&st) || !(100..200).contains(&e.record_type) {
        return None;
    }
    let p = path_parts(&e.pathname)?;
    // HEC's dateToJulian() would yield UNDEFINED_TIME for a non-date D part; such
    // entries are skipped.
    let j = iv::parse_date(p[3])?;
    Some((p, j))
}

/// `ztsGetFirstLastPathnames()` as a linear scan of `cat`: the smallest and largest
/// D-part dates of the dataset of `want` (its D part is ignored).
pub(crate) fn dataset_extrema_scan(cat: &Dss7Catalog, want: &[&[u8]; 6]) -> Option<(i64, i64)> {
    let mut range: Option<(i64, i64)> = None;
    for i in 0..cat.catalog.entries.len() {
        let Some((p, j)) = candidate(cat, i) else {
            continue;
        };
        let same = [0usize, 1, 2, 4, 5]
            .iter()
            .all(|&k| p[k].eq_ignore_ascii_case(want[k]));
        if !same {
            continue;
        }
        range = Some(match range {
            None => (j, j),
            Some((a, b)) => (a.min(j), b.max(j)),
        });
    }
    range
}

/// Smallest and largest D-part julian date per dataset key.
#[derive(Clone, Default)]
struct DatasetRangeIndex {
    extrema: HashMap<Vec<u8>, (i64, i64)>,
}

impl DatasetRangeIndex {
    fn build(cat: &Dss7Catalog) -> Self {
        let mut extrema: HashMap<Vec<u8>, (i64, i64)> = HashMap::new();
        for i in 0..cat.catalog.entries.len() {
            let Some((p, j)) = candidate(cat, i) else {
                continue;
            };
            extrema
                .entry(key(&p))
                .and_modify(|r| *r = (r.0.min(j), r.1.max(j)))
                .or_insert((j, j));
        }
        DatasetRangeIndex { extrema }
    }
}

/// A [`Dss7Catalog`] snapshot that answers whole-dataset ranges from an index.
///
/// Wrapping takes ownership; the catalog is then only readable ([`catalog`](Self::catalog)),
/// so the index, built on the first whole-dataset read
/// ([`Dss7File::read_ts_with_indexed_catalog`](super::Dss7File::read_ts_with_indexed_catalog)),
/// always describes it. [`into_catalog`](Self::into_catalog) gives the catalog back and
/// drops the index. Results are those of the same reads through
/// [`Dss7File::read_ts_with_catalog`](super::Dss7File::read_ts_with_catalog) with the
/// wrapped catalog.
#[derive(Clone)]
pub struct IndexedDss7Catalog {
    catalog: Dss7Catalog,
    index: OnceLock<DatasetRangeIndex>,
}

impl IndexedDss7Catalog {
    pub fn new(catalog: Dss7Catalog) -> Self {
        IndexedDss7Catalog {
            catalog,
            index: OnceLock::new(),
        }
    }

    /// The wrapped catalog (read-only).
    pub fn catalog(&self) -> &Dss7Catalog {
        &self.catalog
    }

    pub fn into_catalog(self) -> Dss7Catalog {
        self.catalog
    }

    /// Whether the range index has been built (it is built on first use).
    pub fn index_built(&self) -> bool {
        self.index.get().is_some()
    }

    /// Index-backed [`dataset_extrema_scan`].
    pub(crate) fn dataset_extrema(&self, want: &[&[u8]; 6]) -> Option<(i64, i64)> {
        self.index
            .get_or_init(|| DatasetRangeIndex::build(&self.catalog))
            .extrema
            .get(&key(want))
            .copied()
    }

    #[cfg(test)]
    fn index_len(&self) -> Option<usize> {
        self.index.get().map(|ix| ix.extrema.len())
    }

    #[cfg(test)]
    fn index_ptr(&self) -> Option<*const DatasetRangeIndex> {
        self.index.get().map(|ix| ix as *const _)
    }
}

impl From<Dss7Catalog> for IndexedDss7Catalog {
    fn from(c: Dss7Catalog) -> Self {
        IndexedDss7Catalog::new(c)
    }
}

impl fmt::Debug for IndexedDss7Catalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexedDss7Catalog")
            .field("entries", &self.catalog.catalog.entries.len())
            .field("index_built", &self.index_built())
            .finish()
    }
}

/// Where a whole-dataset read takes its first/last D-part dates from.
#[derive(Clone, Copy)]
pub(crate) enum RangeFrom<'a> {
    /// A caller's catalog, scanned on every read (its fields may have been edited).
    Scan(&'a Dss7Catalog),
    /// An immutable snapshot with its index.
    Indexed(&'a IndexedDss7Catalog),
}

impl RangeFrom<'_> {
    pub(crate) fn extrema(self, want: &[&[u8]; 6]) -> Option<(i64, i64)> {
        match self {
            RangeFrom::Scan(c) => dataset_extrema_scan(c, want),
            RangeFrom::Indexed(c) => c.dataset_extrema(want),
        }
    }
}

#[cfg(test)]
mod tests;

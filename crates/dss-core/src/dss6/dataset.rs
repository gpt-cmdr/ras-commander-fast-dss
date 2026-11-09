//! DSS6 time-series dataset read: stitch the blocks of one series and
//! trim to a time window, as HEC's `zrrtsx6`/`zritsx6` family does.
//!
//! A dataset is a pathname with any D part: the D part is ignored and replaced by the
//! block dates HEC would read.
//!
//! Regular data (`zrrtsi.f:ZRRTSI6` time-window path, lines 208-614; `zrrtsb6.f`):
//! - The blocks are those holding the window, by the interval's block convention
//!   (`zbegdt6.f`, stepped with `zincbk6.f`). Their D part is the block start date
//!   (`YMDDAT` style 104, line 261).
//! - Every value slot on the standard grid gets a value: from the block when the block
//!   exists (`NPOS = NOPERS(block start, stamp)`, `zrrtsb6.f` line 43) and the position
//!   is within its `KILNDA`, otherwise HEC's "no data" marker -902.0 (`zrrtsb6.f` lines
//!   225-236).
//! - A record time offset (internal header word 1) moves each value stamp to
//!   `standard - interval + offset` (`zofset6.f` lines 142-154). The window selects
//!   standard stamps (HEC adjusts the window start to a standard boundary ignoring the
//!   offset), so a value whose moved stamp precedes the window start can be returned.
//! - Window start and end are both moved up to the next value stamp (`zrrtsi.f` lines
//!   219-227: `ZOFSET6(..., 1, ...)` adjusts the start, and the value count runs to the
//!   stamp at or after the end). Verified against Monolith: a window ending 13:13 on daily
//!   data includes that day's 24:00 value.
//!
//! Irregular data (`zritsi6.f:ZRITSI6` lines 282-704): blocks from the one holding the
//! window start (`zirbeg6.f`) to the one holding the end; values with `start <= t <= end`
//! (`ICTIME` comparisons, lines 642-697).
//!
//! The "whole dataset" read (no window) is what ras-commander's `RasDss` gets from
//! Monolith `HecDss.get(path, true)`: every block from the earliest to the latest D
//! part in the catalog, with leading and trailing missing values removed (regular data;
//! `heclib_c/src/Dss6/ztsRetrieveReg6.c` header comment, `retrieveFlag = -1`: "Remove
//! missing values at the beginning and end of data set (not inside)").

use super::interval::{self, BlockSpan, EPart, DAY_MINUTES};
use super::pathname::split_parts;
use super::ts::{all_hec_no_data, NoDataPolicy, NO_DATA_STATUS};
use super::ts::{lossy, Dss6TsRecord, TsError, TsReadOptions};
use super::walk::CatalogOptions;
use super::{Dss6File, ReadAt};
use crate::{is_hec_missing, TimeSeries, TsKind, TsValues};
use std::collections::{BTreeMap, HashMap};

/// One block of a regular dataset read: block date (y, m, d), position base, record.
type BlockSlot = ((i64, i64, i64), i64, Option<Dss6TsRecord>);

/// Most values one dataset read may produce.
pub const MAX_DATASET_VALUES: u64 = 50_000_000;
/// Most blocks one dataset read may look up.
pub const MAX_DATASET_BLOCKS: u64 = 5_000_000;

/// Inclusive time window in seconds since 1899-12-31 00:00 (see [`crate::time`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub start: i64,
    pub end: i64,
}

impl TimeWindow {
    pub fn new(start: i64, end: i64) -> Self {
        Self { start, end }
    }
}

/// Dataset read options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatasetOptions {
    /// Time window; `None` reads the whole dataset (needs a catalog walk, or a prebuilt
    /// [`DatasetIndex`]).
    pub window: Option<TimeWindow>,
    /// Remove leading and trailing missing values (regular data only), with `ztsTrim`'s
    /// test: -901, -902 and -FLT_MAX are trimmed, a stored NaN is kept (see
    /// [`trim_missing`]). On for the whole dataset read. Off for windowed reads, which
    /// keep every slot of the window as HEC's `zrrtsx6` returns them; Monolith's `HecDss.get(path, start, end)` (the Java
    /// layer `RasDss` uses) trims windowed reads too, so set this to match it.
    pub trim_missing: bool,
    /// Regular data only: read the blocks the catalog actually holds (any D part, any
    /// value count) and place their values by their own stamps, instead of HEC's block
    /// convention. Use for files whose blocks do not follow the convention
    /// (dss6.md §12 O21: 15- and 30-minute data in daily blocks from old writers),
    /// which HEC's own dataset read misses. Needs a catalog.
    pub catalog_blocks: bool,
    pub read: TsReadOptions,
}

impl DatasetOptions {
    /// The whole dataset, trimmed of leading and trailing missing values.
    pub fn whole() -> Self {
        Self {
            window: None,
            trim_missing: true,
            catalog_blocks: false,
            read: TsReadOptions::default(),
        }
    }

    /// Values within `[start, end]` (seconds), untrimmed.
    pub fn window(start: i64, end: i64) -> Self {
        Self {
            window: Some(TimeWindow::new(start, end)),
            trim_missing: false,
            catalog_blocks: false,
            read: TsReadOptions::default(),
        }
    }
}

impl Default for DatasetOptions {
    fn default() -> Self {
        Self::whole()
    }
}

/// What a dataset read found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatasetReport {
    /// Block pathnames looked up and found / absent.
    pub blocks_found: usize,
    pub blocks_missing: usize,
    /// Values filled with -902.0 because their block is absent or shorter than its slot.
    pub filled_no_data: usize,
    /// Values removed by `trim_missing`.
    pub trimmed: usize,
    /// Record time offset in minutes applied to every stamp (regular data; the last
    /// block found, as HEC returns it).
    pub time_offset_minutes: Option<i32>,
    /// Blocks that differ in units, data type, offset or record type from the first one,
    /// or that do not follow the block convention.
    pub warnings: Vec<String>,
    /// Time-origin facts when the dataset is a pattern record.
    pub pattern_origin: Option<crate::timeseries::PatternOrigin>,
    /// User-header words of the block whose header HEC returns for the dataset
    /// (dss6.md §14.4): regular data, the last block found (`zrrtsi.f` lines 345-349 read
    /// `IUHEAD` for every block found); irregular data, the last block looked up, which
    /// leaves the header empty when that block is absent (`zreadx6.f` line 72).
    pub user_header: Vec<i32>,
    /// Pathname of the block `user_header` comes from (`None` when no block supplies it).
    pub user_header_block: Option<Vec<u8>>,
    /// Blocks read for the dataset whose user-header text differs from the reported one
    /// (`user_header::user_header_text_bytes`); 0 when they all agree. With
    /// `user_header_block == None` (no header reported) it counts the blocks that do
    /// carry text: "no header reported", not "two headers disagree".
    pub user_header_conflicts: usize,
}

/// The reported user header and the count of blocks that disagree with it.
fn report_user_header<'a>(
    report: &mut DatasetReport,
    blocks: impl Iterator<Item = &'a Dss6TsRecord>,
    reported: Option<&Dss6TsRecord>,
) {
    let text = |r: &Dss6TsRecord| crate::user_header::user_header_text_bytes(&r.user_header);
    let reference = reported.map(text).unwrap_or_default();
    report.user_header_conflicts = blocks.filter(|r| text(r) != reference).count();
    if let Some(r) = reported {
        report.user_header = r.user_header.clone();
        report.user_header_block = Some(r.pathname.clone());
    }
}

/// A stitched dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct Dss6Dataset {
    pub series: TimeSeries,
    pub report: DatasetReport,
}

/// Dataset key: the pathname with an empty D part, upper case.
fn dataset_key(path: &[u8]) -> Option<Vec<u8>> {
    let p = split_parts(path)?;
    let mut out = Vec::with_capacity(path.len());
    out.extend_from_slice(&path[..p[3].start]);
    out.extend_from_slice(&path[p[3].end..]);
    Some(out.iter().map(u8::to_ascii_uppercase).collect())
}

/// One stored block of a dataset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedBlock {
    /// Block date from the D part (julian), when it parses.
    pub julian: Option<i64>,
    pub pathname: Vec<u8>,
    pub record_type: i32,
}

/// Blocks of every time-series dataset in a file, from one catalog walk.
#[derive(Debug, Clone, Default)]
pub struct DatasetIndex {
    map: HashMap<Vec<u8>, Vec<IndexedBlock>>,
}

impl DatasetIndex {
    /// Walk the catalog once (bins only, no verification) and group live time-series
    /// records (types 100-119) by dataset, sorted by block date.
    pub fn build<S: ReadAt>(file: &Dss6File<S>) -> Result<Self, TsError> {
        let opts = CatalogOptions {
            stop_at_knrecs: false,
            verify_hashes: false,
            verify_table_chains: false,
            check_duplicates: false,
            verify_info: super::InfoCheck::None,
        };
        let walk = file.walk_bins(&opts)?;
        let mut map: HashMap<Vec<u8>, Vec<IndexedBlock>> = HashMap::new();
        for e in walk.live() {
            if !(100..120).contains(&e.record_type) {
                continue;
            }
            let Some(key) = dataset_key(&e.pathname) else {
                continue;
            };
            let parts = split_parts(&e.pathname).expect("key parsed");
            let julian = interval::parse_d_part(&e.pathname[parts[3].clone()]);
            map.entry(key).or_default().push(IndexedBlock {
                julian,
                pathname: e.pathname.clone(),
                record_type: e.record_type,
            });
        }
        for v in map.values_mut() {
            v.sort_by(|a, b| a.julian.cmp(&b.julian).then(a.pathname.cmp(&b.pathname)));
        }
        Ok(Self { map })
    }

    /// Blocks of the dataset `pathname` belongs to (its D part is ignored).
    pub fn blocks(&self, pathname: &[u8]) -> &[IndexedBlock] {
        dataset_key(pathname)
            .and_then(|k| self.map.get(&k))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Dataset keys (pathnames with an empty D part).
    pub fn datasets(&self) -> impl Iterator<Item = &[u8]> {
        self.map.keys().map(Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Value buffer in the dataset's width.
enum Buf {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl Buf {
    fn push_missing(&mut self) {
        match self {
            Buf::F32(v) => v.push(-902.0),
            Buf::F64(v) => v.push(-902.0),
        }
    }
    fn push_from(&mut self, src: &TsValues, i: usize) {
        match (self, src) {
            (Buf::F32(v), TsValues::F32(s)) => v.push(s[i]),
            (Buf::F64(v), TsValues::F64(s)) => v.push(s[i]),
            (Buf::F64(v), TsValues::F32(s)) => v.push(f64::from(s[i])),
            // Mixed widths start as F64 (see `width`), so this does not occur.
            (Buf::F32(v), TsValues::F64(s)) => v.push(s[i] as f32),
        }
    }
    fn into_values(self) -> TsValues {
        match self {
            Buf::F32(v) => TsValues::F32(v),
            Buf::F64(v) => TsValues::F64(v),
        }
    }
}

fn path_with_d(parts_src: &[u8], d: &[u8]) -> Option<Vec<u8>> {
    let p = split_parts(parts_src)?;
    let mut out = Vec::with_capacity(parts_src.len() + 9);
    out.extend_from_slice(&parts_src[..p[3].start]);
    out.extend_from_slice(d);
    out.extend_from_slice(&parts_src[p[3].end..]);
    Some(out)
}

fn path_with_e(path: &[u8], e: &[u8]) -> Option<Vec<u8>> {
    let p = split_parts(path)?;
    let mut out = Vec::with_capacity(path.len() + 8);
    out.extend_from_slice(&path[..p[4].start]);
    out.extend_from_slice(e);
    out.extend_from_slice(&path[p[4].end..]);
    Some(out)
}

fn too_large(values: u64) -> TsError {
    TsError::TooLarge {
        count: values,
        limit: MAX_DATASET_VALUES,
    }
}

/// HEC's window rule. `zrrtsi.f:ZRRTSI6` checks the
/// values of the whole request after every block is read (lines 618-651, hec-dss fork
/// tag `7-IU-15`), absent-block fills (-902) and all-missing blocks alike. An
/// all-missing block inside a window that holds data is read as found: it counts in
/// `blocks_found` and supplies the units, type, offset and user header when it is the
/// last block found (lines 340-349, 575-603), exactly as without the option.
fn check_no_data(path: &[u8], series: &TimeSeries) -> Result<(), TsError> {
    if all_hec_no_data(&series.values) {
        return Err(TsError::NoData {
            pathname: path.to_vec(),
            status: NO_DATA_STATUS,
        });
    }
    Ok(())
}

/// Seconds to (julian, minutes 1..=1440), rounding up to a whole minute.
fn ceil_minutes(t: i64) -> i64 {
    t.div_euclid(60) + i64::from(t.rem_euclid(60) != 0)
}

impl<S: ReadAt> Dss6File<S> {
    /// Read a time-series dataset. See [`DatasetOptions`]; the D part of `pathname` is
    /// ignored. The whole-dataset read walks the catalog; use
    /// [`Self::read_ts_dataset_indexed`] to reuse one walk for many datasets.
    pub fn read_ts_dataset(
        &self,
        pathname: &[u8],
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        let needs_index = opts.window.is_none() || opts.catalog_blocks;
        if needs_index {
            let index = DatasetIndex::build(self)?;
            self.read_ts_dataset_indexed(&index, pathname, opts)
        } else {
            self.read_ts_dataset_inner(None, pathname, opts)
        }
    }

    /// [`Self::read_ts_dataset`] with a prebuilt index.
    pub fn read_ts_dataset_indexed(
        &self,
        index: &DatasetIndex,
        pathname: &[u8],
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        self.read_ts_dataset_inner(Some(index), pathname, opts)
    }

    fn read_ts_dataset_inner(
        &self,
        index: Option<&DatasetIndex>,
        pathname: &[u8],
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        // A dataset refusal names the pathname as requested (the reads
        // below use the upper-cased form).
        self.read_ts_dataset_upper(index, pathname, opts)
            .map_err(|e| match e {
                TsError::NoData { status, .. } => TsError::NoData {
                    pathname: pathname.to_vec(),
                    status,
                },
                other => other,
            })
    }

    fn read_ts_dataset_upper(
        &self,
        index: Option<&DatasetIndex>,
        pathname: &[u8],
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        let bad = |detail: String| TsError::BadPathname {
            pathname: pathname.to_vec(),
            detail,
        };
        let path: Vec<u8> = pathname.iter().map(u8::to_ascii_uppercase).collect();
        let parts = split_parts(&path).ok_or_else(|| bad("not /A/B/C/D/E/F/".into()))?;
        let d = &path[parts[3].clone()];
        if super::ts::is_pattern_d_part(d) {
            // Pattern data has no blocks: the dataset is the record.
            let rec = self.read_ts_record_with(&path, &opts.read)?;
            let mut report = DatasetReport {
                blocks_found: 1,
                pattern_origin: rec.pattern_origin(),
                ..Default::default()
            };
            report_user_header(&mut report, std::iter::once(&rec), Some(&rec));
            return Ok(Dss6Dataset {
                series: rec.to_timeseries(),
                report,
            });
        }
        let e = path[parts[4].clone()].to_vec();
        match interval::parse_e_part(&e) {
            Some(EPart::Regular { minutes, .. }) => {
                if opts.catalog_blocks {
                    let index = index.ok_or_else(|| bad("catalog_blocks needs an index".into()))?;
                    self.regular_from_catalog(index, &path, i64::from(minutes), opts)
                } else {
                    self.regular_dataset(index, &path, i64::from(minutes), opts)
                }
            }
            Some(EPart::Irregular { block }) => {
                self.irregular_dataset(index, &path, block, None, opts)
            }
            Some(EPart::Pseudo { minutes, block }) => {
                // zritsi6.f lines 139-145: a `~interval` E part is read as its stored
                // IR-* form (zpseudorts6.f).
                let stored = super::pathname::pseudo_to_stored(&path)
                    .map(|(p, _)| p)
                    .ok_or_else(|| bad("pseudo-regular E part".into()))?;
                self.irregular_dataset(index, &stored, block, Some(minutes), opts)
            }
            None => Err(bad(format!(
                "E part '{}' is not a time-series interval",
                lossy(&e)
            ))),
        }
    }

    fn lookup_block(
        &self,
        path: &[u8],
        julian: i64,
        opts: &TsReadOptions,
    ) -> Result<Option<Dss6TsRecord>, TsError> {
        let p = path_with_d(path, &interval::d_part(julian)).expect("path has parts");
        match self.lookup(&p)? {
            None => Ok(None),
            Some(rec) => self.decode_ts_info(&rec.info, opts).map(Some),
        }
    }

    fn regular_dataset(
        &self,
        index: Option<&DatasetIndex>,
        path: &[u8],
        intl: i64,
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        let span = interval::regular_block_for(intl as i32, self.header().version_number);
        let mut report = DatasetReport::default();
        let block_read = opts.read.with_no_data(NoDataPolicy::Return);
        // Block range, and the window (minutes) the moved stamps must fall in.
        let (first_ymd, last_ymd, filter) = match opts.window {
            Some(w) => {
                if w.end < w.start {
                    return Err(TsError::BadPathname {
                        pathname: path.to_vec(),
                        detail: "time window ends before it starts".into(),
                    });
                }
                // zrrtsi.f:ZRRTSI6 (219-227): both bounds advance to the next
                // standard stamp. Preserve a seconds remainder before applying
                // the minute-based interval rules, including equal off-grid bounds.
                let (ws, we) = (ceil_minutes(w.start), ceil_minutes(w.end));
                // A standard stamp is within one period of its moved stamp, so pad the
                // block range by a period on each side.
                let (pj, pm) =
                    interval::datcln(ws.div_euclid(DAY_MINUTES), ws.rem_euclid(DAY_MINUTES));
                let (qj, qm) =
                    interval::datcln(we.div_euclid(DAY_MINUTES), we.rem_euclid(DAY_MINUTES));
                // Refuse oversized windows before any lookup.
                let slots = (we - ws) / intl.max(1) + 1;
                if slots as u64 > MAX_DATASET_VALUES {
                    return Err(too_large(slots as u64));
                }
                let (pj, _) = interval::inctim(intl, -1, pj, pm);
                let (qj, _) = interval::inctim(intl, 1, qj, qm);
                (
                    interval::block_start_ymd(pj, span),
                    interval::block_start_ymd(qj, span),
                    Some((ws, we)),
                )
            }
            None => {
                let blocks = index.map(|i| i.blocks(path)).unwrap_or(&[]);
                let dates: Vec<i64> = blocks.iter().filter_map(|b| b.julian).collect();
                let (Some(&first), Some(&last)) = (dates.iter().min(), dates.iter().max()) else {
                    return Err(TsError::NotFound {
                        pathname: path.to_vec(),
                    });
                };
                let slots = (last - first + 400) * DAY_MINUTES / intl.max(1);
                if slots as u64 > MAX_DATASET_VALUES {
                    return Err(too_large(slots as u64));
                }
                (
                    interval::block_start_ymd(first, span),
                    interval::block_start_ymd(last, span),
                    None,
                )
            }
        };
        let mut blocks: Vec<BlockSlot> = Vec::new();
        let mut ymd = first_ymd;
        let mut nblocks = 0u64;
        loop {
            nblocks += 1;
            if nblocks > MAX_DATASET_BLOCKS {
                return Err(too_large(nblocks));
            }
            let base = interval::position_base(intl, ymd);
            let jul = interval::iymdjl(ymd.0, ymd.1, ymd.2);
            // Blocks are never refused one by one; HEC's rule is for the window.
            let rec = self.lookup_block(path, jul, &block_read)?;
            blocks.push((ymd, base, rec));
            if ymd >= last_ymd {
                break;
            }
            ymd = interval::next_block_ymd(span, ymd);
        }
        let Some(first) = blocks.iter().find_map(|(_, _, r)| r.as_ref()) else {
            return Err(TsError::NotFound {
                pathname: path.to_vec(),
            });
        };
        let found = || blocks.iter().filter_map(|(_, _, r)| r.as_ref());
        // zrrtsi.f line 576: IOFSET is taken from each block read, so the offset HEC
        // returns (and applies to every stamp) is the last block's. Blocks with another
        // offset are reported in `warnings`.
        let last_offset = found().next_back().map_or(0, |r| r.time_offset_minutes);
        // zrrtsi.f re-reads the internal header for every block it reads, so the units,
        // data type and time zone HEC returns are the last block's (`dss6.md` §14.4;
        // `tests/dss6_b1_multiblock.rs` and `fixtures/public/b1_multiblock6.dss`).
        let header = found().next_back().unwrap_or(first);
        // zrrtsi.f lines 345-349 read the user header of every block found.
        report_user_header(&mut report, found(), found().next_back());
        let (units, data_type, time_zone) = (
            header.units.clone(),
            header.data_type.clone(),
            header.time_zone_name.clone(),
        );
        let offset = i64::from(last_offset);
        report.time_offset_minutes = Some(last_offset);
        let wide = found().any(|r| matches!(r.values, TsValues::F64(_)));
        let with_quality = found().any(|r| r.quality.is_some());
        for r in found() {
            if r.units != first.units
                || r.data_type != first.data_type
                || r.time_offset_minutes != first.time_offset_minutes
                || r.record_type != first.record_type
            {
                report.warnings.push(format!(
                    "block {} differs from the first block in units, type, offset or record type",
                    lossy(&r.pathname)
                ));
            }
            if r.nonstandard_block {
                report.warnings.push(format!(
                    "block {} holds {} values, not the standard block count",
                    lossy(&r.pathname),
                    r.logical_values
                ));
            }
        }
        let mut buf = if wide {
            Buf::F64(Vec::new())
        } else {
            Buf::F32(Vec::new())
        };
        let mut quality: Option<Vec<i32>> = with_quality.then(Vec::new);
        let mut times = Vec::new();
        let mut prev_stamp: Option<i64> = None;
        for (ymd_b, base, rec) in &blocks {
            // Standard slots of this block: positions 1.. up to the next block start
            // (weekly bases are moved back to a Sunday, so step from the block date).
            let jb = *base;
            let next = interval::position_base(intl, interval::next_block_ymd(span, *ymd_b));
            let slots = interval::nopers(intl, jb, 0, next, 0).max(0);
            if times.len() as u64 + slots as u64 > MAX_DATASET_VALUES {
                return Err(too_large(times.len() as u64 + slots as u64));
            }
            let stamps = super::ts::regular_times(intl, jb, slots as usize, offset);
            let standard = if offset == 0 {
                None
            } else {
                Some(super::ts::regular_times(intl, jb, slots as usize, 0))
            };
            let mut used = false;
            for (k, &t) in stamps.iter().enumerate() {
                // The window selects standard stamps; an offset then moves them
                // (zrrtsi.f: ZOFSET6 IFLAG=1 on the window start, no offset applied;
                // verified on Monolith-written offset data).
                let tm = standard.as_ref().map_or(t, |s| s[k]) / 60;
                let prev = prev_stamp.replace(tm);
                if let Some((ws, we)) = filter {
                    // Start and end both move up to the next grid stamp (zrrtsi.f lines
                    // 219-222, ZOFSET6 IFLAG=1): a stamp past the end is still read when
                    // the previous stamp is before the end.
                    let past_end = tm > we && prev.map_or(true, |p| p >= we);
                    if tm < ws || past_end {
                        continue;
                    }
                }
                used = true;
                times.push(t);
                match rec {
                    Some(r) if k < r.values.len() => {
                        buf.push_from(&r.values, k);
                        if let Some(q) = quality.as_mut() {
                            q.push(r.quality.as_ref().map_or(0, |rq| rq[k]));
                        }
                    }
                    _ => {
                        buf.push_missing();
                        report.filled_no_data += 1;
                        if let Some(q) = quality.as_mut() {
                            q.push(0);
                        }
                    }
                }
            }
            if used {
                if rec.is_some() {
                    report.blocks_found += 1;
                } else {
                    report.blocks_missing += 1;
                }
            }
        }
        let mut series = TimeSeries {
            pathname: first.pathname.clone(),
            record_type: first.record_type,
            kind: TsKind::Regular {
                interval_seconds: intl * 60,
            },
            times,
            values: buf.into_values(),
            quality,
            units,
            data_type,
            time_zone,
        };
        if wide {
            series.record_type = 105;
        }
        if opts.read.no_data == NoDataPolicy::Refuse {
            // zrrtsi.f lines 653-658: no block of the window read -> ISTAT 5
            // ("no pathname(s) found"; a block found only in the padding is not read).
            if report.blocks_found == 0 {
                return Err(TsError::NotFound {
                    pathname: path.to_vec(),
                });
            }
            check_no_data(path, &series)?;
        }
        if opts.trim_missing {
            report.trimmed = trim_missing(&mut series);
        }
        Ok(Dss6Dataset { series, report })
    }

    /// `catalog_blocks` mode: every stored block, placed by its own stamps on one grid.
    fn regular_from_catalog(
        &self,
        index: &DatasetIndex,
        path: &[u8],
        intl: i64,
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        let mut report = DatasetReport::default();
        let mut recs = Vec::new();
        for b in index.blocks(path) {
            if b.julian.is_none() {
                continue;
            }
            // Same E part spelling as requested (the index key compares upper case).
            let p = path_with_e(
                &b.pathname,
                &path[split_parts(path).expect("parts")[4].clone()],
            )
            .unwrap_or_else(|| b.pathname.clone());
            let rec = match self.lookup(&b.pathname)? {
                Some(r) => r,
                None => match self.lookup(&p)? {
                    Some(r) => r,
                    None => continue,
                },
            };
            recs.push(
                self.decode_ts_info(&rec.info, &opts.read.with_no_data(NoDataPolicy::Return))?,
            );
        }
        let Some(first) = recs.first().cloned() else {
            return Err(TsError::NotFound {
                pathname: path.to_vec(),
            });
        };
        // zrrtsi.f re-reads the internal header for every block it reads, so the units,
        // data type and time zone HEC returns are the last block's (`dss6.md` §14.4;
        // `tests/dss6_b1_multiblock.rs` and `fixtures/public/b1_multiblock6.dss`).
        let (units, data_type, time_zone) = recs
            .last()
            .map(|r| {
                (
                    r.units.clone(),
                    r.data_type.clone(),
                    r.time_zone_name.clone(),
                )
            })
            .unwrap_or_default();
        report.blocks_found = recs.len();
        // The last block read supplies the user header, as for the other headers.
        report_user_header(&mut report, recs.iter(), recs.last());
        report.time_offset_minutes = Some(first.time_offset_minutes);
        let wide = recs.iter().any(|r| matches!(r.values, TsValues::F64(_)));
        let with_quality = recs.iter().any(|r| r.quality.is_some());
        // Later blocks win where stamps overlap.
        let mut by_time: BTreeMap<i64, (usize, usize)> = BTreeMap::new();
        for (ri, r) in recs.iter().enumerate() {
            if r.units != first.units || r.data_type != first.data_type {
                report.warnings.push(format!(
                    "block {} differs from the first block in units or type",
                    lossy(&r.pathname)
                ));
            }
            for (k, &t) in r.times.iter().enumerate() {
                by_time.insert(t, (ri, k));
            }
        }
        let (Some(&t0), Some(&t1)) = (by_time.keys().next(), by_time.keys().next_back()) else {
            return Err(TsError::NotFound {
                pathname: path.to_vec(),
            });
        };
        let (lo, hi) = match opts.window {
            Some(w) => (w.start.max(t0), w.end.min(t1)),
            None => (t0, t1),
        };
        let mut buf = if wide {
            Buf::F64(Vec::new())
        } else {
            Buf::F32(Vec::new())
        };
        let mut quality: Option<Vec<i32>> = with_quality.then(Vec::new);
        let mut times = Vec::new();
        // Walk the grid from the first stored stamp.
        let (mut j, mut m) = interval::datcln(
            (t0 / 60).div_euclid(DAY_MINUTES),
            (t0 / 60).rem_euclid(DAY_MINUTES),
        );
        let mut prev_t: Option<i64> = None;
        loop {
            let t = interval::abs_minutes(j, m) * 60;
            if t > hi && prev_t.map_or(true, |p| p >= hi) {
                break;
            }
            if times.len() as u64 >= MAX_DATASET_VALUES {
                return Err(too_large(times.len() as u64 + 1));
            }
            // End moves up to the next stamp, as in the block-convention read.
            let include = t >= lo && (t <= hi || prev_t.map_or(true, |p| p < hi));
            prev_t = Some(t);
            if include {
                times.push(t);
                match by_time.get(&t) {
                    Some(&(ri, k)) => {
                        buf.push_from(&recs[ri].values, k);
                        if let Some(q) = quality.as_mut() {
                            q.push(recs[ri].quality.as_ref().map_or(0, |rq| rq[k]));
                        }
                    }
                    None => {
                        buf.push_missing();
                        report.filled_no_data += 1;
                        if let Some(q) = quality.as_mut() {
                            q.push(0);
                        }
                    }
                }
            }
            (j, m) = interval::inctim(intl, 1, j, m);
        }
        let mut series = TimeSeries {
            pathname: first.pathname.clone(),
            record_type: if wide { 105 } else { first.record_type },
            kind: TsKind::Regular {
                interval_seconds: intl * 60,
            },
            times,
            values: buf.into_values(),
            quality,
            units,
            data_type,
            time_zone,
        };
        if opts.read.no_data == NoDataPolicy::Refuse {
            // The same window rule (no HEC path reads catalog blocks).
            check_no_data(path, &series)?;
        }
        if opts.trim_missing {
            report.trimmed = trim_missing(&mut series);
        }
        Ok(Dss6Dataset { series, report })
    }

    fn irregular_dataset(
        &self,
        index: Option<&DatasetIndex>,
        path: &[u8],
        span: BlockSpan,
        _pseudo: Option<i32>,
        opts: &DatasetOptions,
    ) -> Result<Dss6Dataset, TsError> {
        let mut report = DatasetReport::default();
        let mut recs: Vec<Dss6TsRecord> = Vec::new();
        let mut last_slot_found = true;
        match opts.window {
            Some(w) => {
                if w.end < w.start {
                    return Err(TsError::BadPathname {
                        pathname: path.to_vec(),
                        detail: "time window ends before it starts".into(),
                    });
                }
                // zritsi6.f lines 297-354: from the block of the start date to the block
                // past the end date.
                let jstart = w.start.div_euclid(60 * DAY_MINUTES);
                let jend = w.end.div_euclid(60 * DAY_MINUTES);
                // A day block per day is the most blocks a window can need.
                if (jend - jstart) as u64 > MAX_DATASET_BLOCKS {
                    return Err(too_large((jend - jstart) as u64));
                }
                let mut ymd = interval::block_start_ymd(jstart, span);
                let mut n = 0u64;
                loop {
                    let jul = interval::iymdjl(ymd.0, ymd.1, ymd.2);
                    if jul > jend {
                        break;
                    }
                    n += 1;
                    if n > MAX_DATASET_BLOCKS {
                        return Err(too_large(n));
                    }
                    match self.lookup_block(
                        path,
                        jul,
                        &opts.read.with_no_data(NoDataPolicy::Return),
                    )? {
                        Some(r) => {
                            recs.push(r);
                            last_slot_found = true;
                        }
                        None => {
                            report.blocks_missing += 1;
                            last_slot_found = false;
                        }
                    }
                    ymd = interval::next_block_ymd(span, ymd);
                }
            }
            None => {
                let blocks = index.map(|i| i.blocks(path)).unwrap_or(&[]);
                for b in blocks {
                    let rec = match self.lookup(&b.pathname)? {
                        Some(r) => r,
                        None => continue,
                    };
                    recs.push(self.decode_ts_info(
                        &rec.info,
                        &opts.read.with_no_data(NoDataPolicy::Return),
                    )?);
                }
            }
        }
        let Some(first) = recs.first().cloned() else {
            return Err(TsError::NotFound {
                pathname: path.to_vec(),
            });
        };
        // zrrtsi.f re-reads the internal header for every block it reads, so the units,
        // data type and time zone HEC returns are the last block's (`dss6.md` §14.4;
        // `tests/dss6_b1_multiblock.rs` and `fixtures/public/b1_multiblock6.dss`).
        let (units, data_type, time_zone) = recs
            .last()
            .map(|r| {
                (
                    r.units.clone(),
                    r.data_type.clone(),
                    r.time_zone_name.clone(),
                )
            })
            .unwrap_or_default();
        report.blocks_found = recs.len();
        // zritsi6.f reads every block in the window with zreadx6, which sets NUHEAD
        // to 0 for an absent block (zreadx6.f line 72), so the last block looked up
        // supplies the user header; whole-dataset reads end at a stored block.
        let reported = if last_slot_found { recs.last() } else { None };
        report_user_header(&mut report, recs.iter(), reported);
        let wide = recs.iter().any(|r| matches!(r.values, TsValues::F64(_)));
        let with_quality = recs.iter().any(|r| r.quality.is_some());
        let mut buf = if wide {
            Buf::F64(Vec::new())
        } else {
            Buf::F32(Vec::new())
        };
        let mut quality: Option<Vec<i32>> = with_quality.then(Vec::new);
        let mut times = Vec::new();
        for r in &recs {
            if r.units != first.units || r.data_type != first.data_type {
                report.warnings.push(format!(
                    "block {} differs from the first block in units or type",
                    lossy(&r.pathname)
                ));
            }
            for (k, &t) in r.times.iter().enumerate() {
                if let Some(w) = opts.window {
                    if t < w.start || t > w.end {
                        continue;
                    }
                }
                times.push(t);
                buf.push_from(&r.values, k);
                if let Some(q) = quality.as_mut() {
                    q.push(r.quality.as_ref().map_or(0, |rq| rq[k]));
                }
            }
        }
        let series = TimeSeries {
            pathname: first.pathname.clone(),
            record_type: if wide { 115 } else { first.record_type },
            kind: TsKind::Irregular {
                block: span.to_irregular(),
            },
            times,
            values: buf.into_values(),
            quality,
            units,
            data_type,
            time_zone,
        };
        Ok(Dss6Dataset { series, report })
    }
}

/// Remove leading and trailing missing values as heclib's `ztsTrim` does
/// (`heclib_c/src/Internal/ztsTrim.c` lines 83-170, hec-dss fork tag `7-IU-15`): a value
/// is missing when `zisMissingFloat` / `zisMissingDouble` say so, i.e. -901, -902 or
/// -FLT_MAX ([`is_hec_missing`]). A stored NaN is a value and is kept, also at either
/// end (maintainer decision 2026-10-29, "Match HEC behavior"). HEC trims regular
/// data only (`Public/ztsRetrieve.c` lines 618-626); irregular datasets are not trimmed.
/// Returns the number removed.
pub fn trim_missing(ts: &mut TimeSeries) -> usize {
    let vals = ts.values.to_f64();
    let first = vals.iter().position(|&v| !is_hec_missing(v));
    let Some(first) = first else {
        let n = ts.times.len();
        ts.times.clear();
        ts.values = match ts.values {
            TsValues::F32(_) => TsValues::F32(Vec::new()),
            TsValues::F64(_) => TsValues::F64(Vec::new()),
        };
        if let Some(q) = ts.quality.as_mut() {
            q.clear();
        }
        return n;
    };
    let last = vals
        .iter()
        .rposition(|&v| !is_hec_missing(v))
        .unwrap_or(first);
    let n = ts.times.len();
    ts.times = ts.times[first..=last].to_vec();
    ts.values = match &ts.values {
        TsValues::F32(v) => TsValues::F32(v[first..=last].to_vec()),
        TsValues::F64(v) => TsValues::F64(v[first..=last].to_vec()),
    };
    if let Some(q) = ts.quality.as_mut() {
        *q = q[first..=last].to_vec();
    }
    n - (last - first + 1)
}

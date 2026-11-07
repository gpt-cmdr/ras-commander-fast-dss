//! P40 read routes behind the JavaScript exports: regular profiles, location records and
//! the catalog-index (forensic) route. These follow the Python binding
//! (`crates/dss-py/src/lib.rs`: P14 `read_profile`, P15 `location_read*`, P16
//! `provenance7`, `provenance6`, `read_ts_at`) and the DSS6 dataset fallback of its
//! Python layer (`_file.py::read_associated_location`). `dss-core` does all decoding and
//! cites the heclib sources. The only addition is [`DssBuffer`]'s profile guard, which
//! reads sizes before `dss-core` allocates, because a trap in WASM ends the instance.

use super::*;
use dss_core::dss7::interval as iv;
use dss_core::dss7::{ts as ts7, ChainState, Dss7EntryMeta, InfoProblem, TsInternalHeader};
use dss_core::{CatalogEntry, Location, LocationSource};
use std::collections::BTreeMap;

pub(super) type Ts6Blocks = BTreeMap<[String; 5], Vec<String>>;

/// Default limit on the cells (rows x depths) a profile read may assemble: the
/// default grid limit (`GridReadOptions::max_cells`).
pub const DEFAULT_MAX_PROFILE_CELLS: u64 = 16_777_216;

/// Default limit on the rows a time-series dataset read may assemble: 2^24 rows are 128
/// MiB of times plus 64 or 128 MiB of values, and 32 years of 1-minute data.
pub const DEFAULT_MAX_ROWS: u64 = 16_777_216;

/// Depth counts above this are refused by `dss-core` before it allocates
/// (`Dss7File::read_blocks`), so the guard leaves them to its typed error.
const CORE_MAX_DEPTHS: u64 = 100_000;

/// Quality and note element sizes above this are refused the same way.
const CORE_MAX_ELEMENT: u64 = 64;

/// Largest internal-header area the guard reads (`INT_HEAD_SIZE` in `dss7/ts.rs`).
const INTERNAL_HEADER_INTS: i64 = 200;

// ---------------------------------------------------------------------------------
// Profiles (P14)
// ---------------------------------------------------------------------------------

/// A decoded DSS7 regular profile, with `values` row-major (`times.len()` rows of
/// `depths.len()` values).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileData {
    pub profile: dss7::Dss7Profile,
}

/// `map_ts7` for the profile reads; the Python binding's `map_profile7` hints.
fn map_profile7(e: dss7::TsError) -> ReadError {
    use dss7::TsError as E;
    if let E::UnsupportedRecordType {
        pathname,
        record_type,
    } = &e
    {
        return unsupported_profile(pathname, *record_type);
    }
    map_ts7(e)
}

fn unsupported_profile(pathname: &[u8], t: i32) -> ReadError {
    let hint = if t == ts7::ITS_PROFILE || t == ts7::ITD_PROFILE {
        "irregular-interval profiles are not decoded"
    } else if matches!(
        t,
        ts7::RTS_PATTERN | ts7::RTD_PATTERN | ts7::ITS_PATTERN | ts7::ITD_PATTERN
    ) {
        "it is a time-series pattern record, not a profile; read it with readDataset()"
    } else if record_is_ts(t) {
        "it is not a profile record; read it with readDataset()"
    } else {
        "it is not a profile record"
    };
    ReadError::new(
        ErrorKind::Unsupported,
        format!("record {} has type {t}: {hint}", lossy(pathname)),
    )
}

/// A record's internal header, read with `dss-core`'s decoder; `None` when the area is absent
/// or outside the file (`dss-core` then raises its own error).
fn stored_header(bytes: &[u8], rec: &RecordRef) -> Option<TsInternalHeader> {
    let a = rec.info.internal_header;
    let n = a.number_ints.clamp(0, INTERNAL_HEADER_INTS) as usize;
    let start = u64::try_from(a.address)
        .ok()
        .and_then(|w| w.checked_mul(8))
        .and_then(|b| usize::try_from(b).ok())?;
    let area = bytes.get(start..start.checked_add(n * 4)?)?;
    let words: Vec<i32> = area
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Some(TsInternalHeader::decode(&words))
}

/// What `Dss7File::read_blocks` assembles for every output row, whether or not the block is
/// stored: values (one per depth), quality and integer notes at their element sizes
/// (`ztsGetSizesInternal`: the largest seen), and one `String` per row when any block has
/// character notes.
#[derive(Default, Clone, Copy)]
struct RowShape {
    depths: u64,
    quality: u64,
    inotes: u64,
    cnotes: bool,
}

/// Cells one `String` costs on wasm32 (12 bytes; one cell is 4 bytes).
const CNOTE_CELLS: u64 = 3;

impl RowShape {
    fn add(&mut self, h: &TsInternalHeader, profile: bool) {
        let u = |v: i32| u64::try_from(v).unwrap_or(0);
        if profile {
            self.depths = self.depths.max(u(h.profile_depths));
        }
        self.quality = self.quality.max(u(h.quality_element_size));
        self.inotes = self.inotes.max(u(h.inotes_element_size));
        self.cnotes |= h.cnotes_length > 0;
    }

    /// `dss-core` refuses these sizes as corrupt before it allocates (`read_blocks`), so the
    /// guard leaves them to its typed error.
    fn core_refuses(&self) -> bool {
        self.depths > CORE_MAX_DEPTHS
            || self.quality > CORE_MAX_ELEMENT
            || self.inotes > CORE_MAX_ELEMENT
    }

    fn cells_per_row(&self) -> u64 {
        self.depths.max(1) + self.quality + self.inotes + if self.cnotes { CNOTE_CELLS } else { 0 }
    }
}

fn check_cells(
    pathname: &[u8],
    rows: i64,
    shape: RowShape,
    limit: u64,
    limit_name: &str,
) -> ReadResult<()> {
    if shape.core_refuses() {
        return Ok(());
    }
    let rows = u64::try_from(rows).unwrap_or(0);
    let per_row = shape.cells_per_row();
    let cells = rows.saturating_mul(per_row);
    if cells > limit {
        return Err(ReadError::new(
            ErrorKind::InvalidArgument,
            format!(
                "{} needs up to {cells} cells ({rows} rows x {per_row} elements per row, counting \
                 depths, quality and notes; limit {limit}); narrow the window or raise \
                 {limit_name}",
                lossy(pathname)
            ),
        ));
    }
    Ok(())
}

/// Pattern records (`isTsPattern()`: D part starting with "TS-P") have no block range.
fn is_pattern_d(d: &[u8]) -> bool {
    d.len() > 4 && d[..4].eq_ignore_ascii_case(b"TS-P")
}

fn check_blocks(shown: &str, lo: i64, hi: i64, max_blocks: u64) -> ReadResult<()> {
    let n = window_blocks(shown, lo, hi);
    if n > max_blocks {
        return Err(ReadError::new(
            ErrorKind::InvalidArgument,
            format!(
                "range spans up to {n} block periods of {shown} (limit {max_blocks}); read a \
                 narrower window or raise maxBlocks"
            ),
        ));
    }
    Ok(())
}

/// The D-part and bin dates of every catalog entry of the dataset: a range covering the
/// one `dss-core` derives (`Dss7File::dataset_date_range`, `ztsGetFirstLastPathnames`),
/// which reads the same `kbinDates` for the first and last blocks.
fn catalog_range(cat: &Dss7Catalog, want: &[&[u8]], interval_code: i64) -> Option<(i64, i64)> {
    let mut range: Option<(i64, i64)> = None;
    for (e, m) in cat.catalog.entries.iter().zip(&cat.meta) {
        if !(0..=2).contains(&m.raw_status) || !record_is_ts(e.record_type) {
            continue;
        }
        let parts: Vec<&[u8]> = e.pathname.split(|&c| c == b'/').collect();
        if parts.len() != 8
            || ![1, 2, 3, 5, 6]
                .iter()
                .all(|&k| parts[k].eq_ignore_ascii_case(want[k]))
        {
            continue;
        }
        let Some(j) = iv::parse_date(parts[4]) else {
            continue;
        };
        let block = if interval_code > 0 {
            iv::reg_block_start(j, interval_code).1
        } else {
            -interval_code
        };
        let (mut lo, mut hi) = (j, iv::increment_block(j, block) - 1);
        // A date beyond `dss-core`'s limit fails its read as a bad window; one in another
        // entry must not switch the guard off.
        let in_limit = |v: &i64| v.abs() <= iv::JULIAN_LIMIT;
        if let Some(f) = m.julian_first.map(i64::from).filter(in_limit) {
            lo = lo.min(f);
        }
        if let Some(l) = m.julian_last.map(i64::from).filter(in_limit) {
            hi = hi.max(l);
        }
        range = Some(match range {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        });
    }
    range
}

impl DssBuffer {
    /// Refuse a time-series dataset read before `dss-core` expands its range: every regular
    /// period from the first to the last block gets a row, stored or not (DSS7
    /// `Dss7File::read_blocks`, `ztsRetrieveReg7`; DSS6 `regular_dataset`, `zrrtsi.f`), and
    /// every row carries its quality and notes. The range of a whole read comes from the
    /// same stored dates `dss-core` uses, which a sparse or damaged file can make wide.
    /// `max_rows` limits rows x elements per row. An upper estimate, counted before the trim.
    pub(super) fn dataset_time_guard(
        &mut self,
        pathname: &[u8],
        window: Option<(i64, i64)>,
        max_blocks: u64,
        max_rows: u64,
    ) -> ReadResult<()> {
        if let Inner::V6 { .. } = self.inner {
            return self.dataset_guard6(pathname, window, max_blocks, max_rows);
        }
        self.guard7(pathname, window, max_blocks, max_rows, "maxRows", false)
    }

    /// The DSS6 half of [`Self::dataset_time_guard`]. `dss-core` looks blocks up under the
    /// pathname as given (upper-cased), so an E part that matches nothing finds nothing.
    fn dataset_guard6(
        &mut self,
        pathname: &[u8],
        window: Option<(i64, i64)>,
        max_blocks: u64,
        max_rows: u64,
    ) -> ReadResult<()> {
        use dss6::interval::{parse_e_part, regular_block_for, BlockSpan, EPart};
        let parts: Vec<&[u8]> = pathname.split(|&c| c == b'/').collect();
        if parts.len() != 8 || is_pattern_d(parts[4]) {
            return Ok(());
        }
        let Inner::V6 { file, index, .. } = &mut self.inner else {
            return Ok(());
        };
        let upper = parts[5].to_ascii_uppercase();
        let version = file.header().version_number;
        let (step, span) = match parse_e_part(&upper) {
            Some(EPart::Regular { minutes, .. }) => (
                Some(i64::from(minutes) * 60),
                regular_block_for(minutes, version),
            ),
            Some(EPart::Irregular { block }) | Some(EPart::Pseudo { block, .. }) => (None, block),
            None => return Ok(()),
        };
        if index.is_none() {
            // A failure here is `dss-core`'s to report, from the read itself.
            let Ok(built) = DatasetIndex::build(file.as_ref()) else {
                return Ok(());
            };
            *index = Some(built);
        }
        let path = pathname.to_ascii_uppercase();
        let blocks = index.as_ref().expect("index just set").blocks(&path);
        let day = iv::SECS_IN_1_DAY;
        let shown = lossy(pathname);
        let (lo, hi) = match window {
            Some(w) => w,
            None => {
                let dates = || blocks.iter().filter_map(|b| b.julian);
                // Longest length of the last block, in days.
                let last_block = match span {
                    BlockSpan::Day => 1,
                    BlockSpan::Month => 31,
                    BlockSpan::Year => 366,
                    BlockSpan::Decade => 3_653,
                    BlockSpan::Century => 36_525,
                };
                let (Some(lo), Some(hi)) = (dates().min(), dates().max()) else {
                    return Ok(());
                };
                let (lo, hi) = (
                    lo.saturating_mul(day),
                    (hi + last_block).saturating_mul(day),
                );
                check_blocks(&shown, lo, hi, max_blocks)?;
                (lo, hi)
            }
        };
        if let Some(step) = step.filter(|s| *s > 0) {
            // zrrtsi.f: a quality word per row once any block found has quality.
            let quality = blocks
                .iter()
                .any(|b| matches!(file.lookup(&b.pathname), Ok(Some(r)) if r.info.quality != 0));
            let rows = (i128::from(hi) - i128::from(lo)).max(0) / i128::from(step) + 2;
            let shape = RowShape {
                quality: u64::from(quality),
                ..RowShape::default()
            };
            check_cells(
                pathname,
                i64::try_from(rows).unwrap_or(i64::MAX),
                shape,
                max_rows,
                "maxRows",
            )?;
        }
        Ok(())
    }

    /// Refuse a profile dataset read beyond `max_blocks` block periods or `max_cells` cells.
    fn profile_guard(
        &mut self,
        pathname: &[u8],
        window: Option<(i64, i64)>,
        max_blocks: u64,
        max_cells: u64,
    ) -> ReadResult<()> {
        self.guard7(pathname, window, max_blocks, max_cells, "maxCells", true)
    }

    /// The DSS7 guard of both dataset routes. It resolves the pathname exactly as `dss-core`
    /// does (`standard_interval`, `canonical_e`: any spelling of an interval reads the
    /// canonical E part), walks the blocks of the range by lookup, reads their headers, and
    /// refuses before `read_blocks` allocates. A time-series read of a profile is refused
    /// here with `dss-core`'s own error, which it would raise only after assembling. Anything
    /// else wrong with the file is left to `dss-core`, so its typed errors are unchanged.
    fn guard7(
        &mut self,
        pathname: &[u8],
        window: Option<(i64, i64)>,
        max_blocks: u64,
        limit: u64,
        limit_name: &str,
        profile_route: bool,
    ) -> ReadResult<()> {
        let recover = self.recover;
        let Inner::V7 {
            file,
            bytes,
            catalog,
            recover_catalog,
        } = &mut self.inner
        else {
            return Ok(());
        };
        let parts: Vec<&[u8]> = pathname.split(|&c| c == b'/').collect();
        if parts.len() != 8 || is_pattern_d(parts[4]) {
            return Ok(());
        }
        let Some(interval) = iv::standard_interval(parts[5]) else {
            return Ok(());
        };
        let e_part = if interval.pseudo_regular {
            format!("~{}", interval.name)
        } else {
            interval.name.to_string()
        };
        let mut canon = parts.clone();
        canon[5] = e_part.as_bytes();
        let code = interval.code;
        let day = iv::SECS_IN_1_DAY;
        let (start, end) = match window {
            Some((s, e)) => (
                (s.div_euclid(day), s.rem_euclid(day)),
                (e.div_euclid(day), e.rem_euclid(day)),
            ),
            None => {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover)?;
                match catalog_range(cat, &canon, code) {
                    Some((lo, hi)) => ((lo, 0), (hi, day)),
                    None => return Ok(()),
                }
            }
        };
        // `dss-core` refuses dates beyond its limit (`resolve_window`) as a bad window.
        if start.0.abs() > iv::JULIAN_LIMIT || end.0.abs() > iv::JULIAN_LIMIT {
            return Ok(());
        }
        let shown = lossy(&canon.join(&b'/'));
        check_blocks(
            &shown,
            start.0.saturating_mul(day),
            end.0.saturating_mul(day),
            max_blocks,
        )?;
        // The block walk of `read_blocks`: every block period, found by lookup.
        let (mut j, block) = if code > 0 {
            iv::reg_block_start(start.0, code)
        } else {
            let Some(j) = iv::irreg_block_start(start.0, -code) else {
                return Ok(());
            };
            (j, -code)
        };
        let mut shape = RowShape::default();
        let mut stored = 0i64;
        // First pathname found, and the type and depth count of the last block found.
        let mut found: Option<(Vec<u8>, i32, i32)> = None;
        while j <= end.0 {
            let d = iv::julian_to_d_part(j);
            let mut block_parts = canon.clone();
            block_parts[4] = d.as_bytes();
            let path = block_parts.join(&b'/');
            if let Ok(Some(r)) = file.lookup(&path) {
                if record_is_ts(r.record_type) {
                    let h = stored_header(bytes.as_ref(), &r).unwrap_or_default();
                    shape.add(&h, record_is_profile(r.record_type));
                    stored = stored.saturating_add(r.info.number_data.max(0));
                    let first = found.take().map(|f| f.0);
                    found = Some((
                        first.unwrap_or_else(|| r.pathname.clone()),
                        r.record_type,
                        h.profile_depths,
                    ));
                }
            }
            let next = iv::increment_block(j, block);
            if next <= j {
                break;
            }
            j = next;
        }
        let Some((first, last_type, last_depths)) = found else {
            return Ok(());
        };
        if !profile_route && record_is_profile(last_type) && last_depths > 0 {
            if shape.core_refuses() {
                return Ok(());
            }
            return Err(map_ts7(dss7::TsError::UnsupportedRecordType {
                pathname: first,
                record_type: last_type,
            }));
        }
        // Regular data gets a row for every period; irregular rows are what is stored.
        let rows = if code > 0 {
            iv::number_periods(code, start.0, start.1, end.0, end.1).saturating_add(2)
        } else {
            stored
        };
        check_cells(&canon.join(&b'/'), rows, shape, limit, limit_name)
    }

    /// Read a regular profile dataset (types 102/107, any D part), stitched from its
    /// blocks, as the Python binding's `read_profile`. `window` is as for
    /// [`DssBuffer::read_dataset`]. Unlike a whole time-series read, a whole profile read
    /// is also bounded by `max_blocks` and `max_cells` ([`DssBuffer::profile_guard`]).
    pub fn read_profile(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim_missing: bool,
        max_blocks: u64,
        max_cells: u64,
    ) -> ReadResult<ProfileData> {
        let r = self.read_profile_inner(pathname, window, trim_missing, max_blocks, max_cells);
        r.map_err(|e| self.refine(e, pathname))
    }

    fn read_profile_inner(
        &mut self,
        pathname: &str,
        window: Option<(i64, i64)>,
        trim_missing: bool,
        max_blocks: u64,
        max_cells: u64,
    ) -> ReadResult<ProfileData> {
        if let Some((s, e)) = window {
            if e < s {
                return Err(ReadError::new(
                    ErrorKind::InvalidArgument,
                    "time window end is before its start",
                ));
            }
        }
        if let Inner::V6 { .. } = self.inner {
            // heclib Public/ztsStore.c: "Profiles are not implemented in version 6". A DSS6
            // dataset that exists is therefore never a profile.
            self.read_dataset_limited(pathname, window, trim_missing, max_blocks, max_cells)?;
            return Err(ReadError::new(
                ErrorKind::Unsupported,
                format!(
                    "record {pathname} is a time series, not a profile (DSS6 files cannot store \
                     profiles); read it with readDataset()"
                ),
            ));
        }
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            self.profile_guard(&p, window, max_blocks, max_cells)?;
            let Inner::V7 {
                file,
                catalog,
                recover_catalog,
                ..
            } = &mut self.inner
            else {
                unreachable!("DSS6 returned above");
            };
            let opts = TsReadOptions {
                window: window.map(|(start, end)| TsWindow { start, end }),
                trim_missing,
            };
            let r = if window.is_none() {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover)?;
                file.read_ts_profile_with_catalog(cat, &p, &opts)
            } else {
                file.read_ts_profile(&p, &opts)
            };
            match r
                .map(|profile| ProfileData { profile })
                .map_err(map_profile7)
            {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }

    /// Read one stored profile record (one block) by its full pathname. Refused when the
    /// block's rows times its header's depth count exceed `max_cells`, before decoding.
    pub fn read_profile_record(
        &mut self,
        pathname: &str,
        max_cells: u64,
    ) -> ReadResult<ProfileData> {
        let r = self.read_profile_record_inner(pathname, max_cells);
        r.map_err(|e| self.refine(e, pathname))
    }

    fn read_profile_record_inner(
        &mut self,
        pathname: &str,
        max_cells: u64,
    ) -> ReadResult<ProfileData> {
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            let r = match &mut self.inner {
                Inner::V7 {
                    file,
                    bytes,
                    recover_catalog,
                    ..
                } => match v7_record(file, recover_catalog, recover, &p, map_lookup7)? {
                    Some(rec) => {
                        if matches!(rec.record_type, ts7::RTS_PROFILE | ts7::RTD_PROFILE) {
                            let rows = block_rows(&rec.pathname);
                            let mut shape = RowShape::default();
                            if let Some(h) = stored_header(bytes.as_ref(), &rec) {
                                shape.add(&h, true);
                            }
                            check_cells(&rec.pathname, rows, shape, max_cells, "maxCells")?;
                        }
                        file.read_ts_profile_record(&rec)
                            .map(|profile| ProfileData { profile })
                            .map_err(map_profile7)
                    }
                    None => Err(not_found(pathname)),
                },
                // heclib Public/ztsStore.c: "Profiles are not implemented in version 6".
                Inner::V6 { file, .. } => match file.lookup(&p) {
                    Ok(Some(r)) => Err(ReadError::new(
                        ErrorKind::Unsupported,
                        format!(
                            "record {pathname} has type {}: DSS6 files cannot store profiles",
                            r.entry.record_type
                        ),
                    )),
                    Ok(None) => Err(not_found(pathname)),
                    Err(e) => Err(map_dss6(e)),
                },
            };
            match r {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }
}

/// Times in the block named by a regular profile pathname (`ztsGetPathTimeWindow()`
/// reads the whole block); 0 when the pathname does not name one (`dss-core` refuses it).
fn block_rows(pathname: &[u8]) -> i64 {
    let parts: Vec<&[u8]> = pathname.split(|&c| c == b'/').collect();
    if parts.len() != 8 {
        return 0;
    }
    let (Some(i), Some(j)) = (iv::standard_interval(parts[5]), iv::parse_date(parts[4])) else {
        return 0;
    };
    if i.code <= 0 || j.abs() > iv::JULIAN_LIMIT {
        return 0;
    }
    let (start, block) = iv::reg_block_start(j, i.code);
    let day = iv::SECS_IN_1_DAY;
    iv::number_periods(
        i.code,
        start - 1,
        day,
        iv::increment_block(start, block) - 1,
        day,
    )
    .saturating_add(1)
}

// ---------------------------------------------------------------------------------
// Locations (P15, P27)
// ---------------------------------------------------------------------------------

/// A decoded location: a DSS7 type-20 record, or DSS6 time-series header coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationData {
    pub version: u8,
    pub location: Location,
    /// Type of the record read: 20, or the DSS6 time-series type.
    pub record_type: i32,
    /// DSS7: values1 as stored.
    pub values1: Option<Vec<i32>>,
    /// DSS7: the program that wrote the record.
    pub program: Option<String>,
    /// DSS7: last write, milliseconds since 1970-01-01 UTC.
    pub last_write_ms: Option<i64>,
    /// DSS6: julian block date from the D part.
    pub block_julian: Option<i64>,
}

fn from_dss7_location(r: dss7::Dss7Location) -> LocationData {
    LocationData {
        version: 7,
        location: r.location,
        record_type: dss_core::location::LOCATION,
        values1: Some(r.values1),
        program: Some(r.program),
        last_write_ms: Some(r.last_write_ms),
        block_julian: None,
    }
}

fn map_loc7(e: dss7::LocationError) -> ReadError {
    use dss7::LocationError as E;
    let kind = match e {
        E::Dss7(inner) => return map_lookup7(inner),
        E::NotFound { .. } | E::BadPathname { .. } => ErrorKind::NotFound,
        E::UnsupportedRecordType { .. } => ErrorKind::Unsupported,
        E::Corrupt { .. } => ErrorKind::Format,
    };
    ReadError::new(kind, e.to_string())
}

/// Upper-cased (A, B, C, E, F) of `/A/B/C/D/E/F/`; the Python layer's `_key_without_d`.
fn key_without_d(pathname: &str) -> Option<[String; 5]> {
    let parts: Vec<&str> = pathname.split('/').collect();
    if parts.len() != 8 || !parts[0].is_empty() || !parts[7].is_empty() {
        return None;
    }
    Some([1, 2, 3, 5, 6].map(|i| parts[i].to_uppercase()))
}

/// Coordinate bits and the six codes (`_file.py::_location_key`).
fn location_key(l: &LocationData) -> ([u64; 3], [i32; 6]) {
    let c = &l.location;
    (
        [c.x.to_bits(), c.y.to_bits(), c.z.to_bits()],
        [
            c.coordinate_system_code,
            c.coordinate_id,
            c.horizontal_units_code,
            c.horizontal_datum_code,
            c.vertical_units_code,
            c.vertical_datum_code,
        ],
    )
}

impl DssBuffer {
    /// Read the location record (type 20) under `pathname`. DSS6 has none
    /// (`zlocationRetrieve()` returns INCOMPATIBLE_VERSION): `Unsupported`.
    pub fn read_location(&mut self, pathname: &str) -> ReadResult<LocationData> {
        let recover = self.recover;
        let mut last = None;
        for p in encodings(pathname) {
            let r = match &mut self.inner {
                Inner::V7 {
                    file,
                    recover_catalog,
                    ..
                } => match v7_record(file, recover_catalog, recover, &p, map_lookup7)? {
                    Some(rec) => file
                        .read_location_record(&rec)
                        .map(from_dss7_location)
                        .map_err(map_loc7),
                    None => Err(not_found(pathname)),
                },
                Inner::V6 { .. } => Err(ReadError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "{pathname}: DSS6 files have no location records (type 20 is DSS7 \
                         only); use readAssociatedLocation for time-series coordinates"
                    ),
                )),
            };
            match r {
                Err(e) if e.kind == ErrorKind::NotFound => last = Some(e),
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }

    /// The location associated with a pathname, or `None`, as the Python
    /// `DssFile.read_associated_location`.
    ///
    /// DSS7: the record `/A/B/Location Info////` (`zlocationPath()`); the data record need
    /// not exist. DSS6: the coordinates in the time-series record's header, with the
    /// supplemental text from its user header (P27). A DSS6 pathname that names no stored
    /// block reads every block of the dataset: blocks that store different coordinates
    /// raise `LocationConflict`; otherwise the result comes from the block HEC reports for
    /// the whole dataset, the last by D-part date (dss6.md section 14.4).
    pub fn read_associated_location(&mut self, pathname: &str) -> ReadResult<Option<LocationData>> {
        if let Inner::V7 { .. } = self.inner {
            return self.associated7(pathname);
        }
        match self.associated6_block(pathname) {
            Err(e) if e.kind == ErrorKind::NotFound => self.associated6_dataset(pathname, e),
            other => other,
        }
    }

    fn associated7(&mut self, pathname: &str) -> ReadResult<Option<LocationData>> {
        let recover = self.recover;
        let Inner::V7 {
            file,
            recover_catalog,
            ..
        } = &mut self.inner
        else {
            return Ok(None);
        };
        let lp = dss_core::location::location_pathname(pathname.as_bytes()).ok_or_else(|| {
            ReadError::new(
                ErrorKind::NotFound,
                format!("pathname {pathname} has no A and B parts"),
            )
        })?;
        for p in encodings(&lossy(&lp)) {
            if p.is_empty() || p.len() > dss7::MAX_PATHNAME_BYTES {
                return Err(ReadError::new(
                    ErrorKind::NotFound,
                    format!(
                        "location pathname for {pathname} is {} bytes; DSS7 pathnames are \
                         1..393 bytes",
                        p.len()
                    ),
                ));
            }
            if let Some(r) = v7_record(file, recover_catalog, recover, &p, map_dss7)? {
                return file
                    .read_location_record(&r)
                    .map(|l| Some(from_dss7_location(l)))
                    .map_err(map_loc7);
            }
        }
        Ok(None)
    }

    /// One DSS6 block by its full pathname.
    fn associated6_block(&mut self, pathname: &str) -> ReadResult<Option<LocationData>> {
        let Inner::V6 { file, .. } = &self.inner else {
            return Ok(None);
        };
        let mut last = None;
        for p in encodings(pathname) {
            match file.read_ts_record(&p) {
                Ok(ts) => {
                    let block_julian = dss6::pathname::split_parts(&ts.pathname)
                        .and_then(|q| dss6::interval::parse_d_part(&ts.pathname[q[3].clone()]));
                    let loc = Location::from_dss6_ts(
                        &ts.pathname,
                        ts.coordinates,
                        ts.coordinate_descriptors,
                    );
                    return Ok(loc.map(|l| LocationData {
                        version: 6,
                        // P27: supplemental text from this record's user header.
                        location: l.with_dss6_user_header(&ts.user_header),
                        record_type: ts.record_type,
                        values1: None,
                        program: None,
                        last_write_ms: None,
                        block_julian,
                    }));
                }
                Err(dss6::TsError::UnsupportedRecordType { .. }) => return Ok(None),
                Err(e) => match map_ts6(e) {
                    e if e.kind == ErrorKind::NotFound => last = Some(e),
                    e => return Err(e),
                },
            }
        }
        Err(last.unwrap_or_else(|| not_found(pathname)))
    }

    /// The dataset fallback of `_file.py::read_associated_location` (P15, P27).
    fn associated6_dataset(
        &mut self,
        pathname: &str,
        not_found_err: ReadError,
    ) -> ReadResult<Option<LocationData>> {
        let cat = match &self.inner {
            Inner::V6 {
                ts_blocks: None, ..
            } => Some(self.catalog()?),
            _ => None,
        };
        let Inner::V6 { ts_blocks, .. } = &mut self.inner else {
            return Err(not_found_err);
        };
        let blocks = ts_blocks.get_or_insert_with(|| {
            let mut m = Ts6Blocks::new();
            if let Some(c) = cat {
                for i in 0..c.len() {
                    // `dss6::DatasetIndex::build`: live records of types 100-119.
                    if matches!(c.status[i], "live" | "alias")
                        && (100..120).contains(&c.record_type[i])
                    {
                        if let Some(k) = key_without_d(&c.pathname[i]) {
                            m.entry(k).or_default().push(c.pathname[i].clone());
                        }
                    }
                }
            }
            m
        });
        let matches = key_without_d(pathname)
            .and_then(|k| blocks.get(&k))
            .cloned()
            .unwrap_or_default();
        if matches.is_empty() {
            return Err(not_found_err);
        }
        let mut found = Vec::with_capacity(matches.len());
        for p in &matches {
            found.push((p.as_str(), self.associated6_block(p)?));
        }
        let reference = found[0].1.as_ref().map(location_key);
        for (p, other) in &found[1..] {
            if other.as_ref().map(location_key) != reference {
                return Err(ReadError::new(
                    ErrorKind::LocationConflict,
                    format!(
                        "blocks of {pathname} store different coordinates ({} and {p}); read \
                         one block by its full pathname",
                        found[0].0
                    ),
                ));
            }
        }
        // The last block by D-part date, ties by pathname bytes, as `DatasetIndex`.
        let order = |p: &str, d: &Option<LocationData>| {
            let j = d.as_ref().and_then(|l| l.block_julian);
            let raw = encodings(p).pop().unwrap_or_default();
            (j.is_some(), j.unwrap_or(0), raw)
        };
        let mut best = 0;
        for i in 1..found.len() {
            if order(found[i].0, &found[i].1) > order(found[best].0, &found[best].1) {
                best = i;
            }
        }
        Ok(found.swap_remove(best).1)
    }
}

// ---------------------------------------------------------------------------------
// Catalog-index route (P16)
// ---------------------------------------------------------------------------------

/// Where a catalog entry sits and whether an ordinary pathname read reaches it (the
/// Python binding's `EntryProvenance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryProvenance {
    pub index: usize,
    pub version: u8,
    pub pathname: String,
    pub record_type: i32,
    pub status: &'static str,
    pub raw_status: i64,
    pub recover_mode: bool,
    pub recovered: bool,
    pub duplicate: bool,
    pub duplicate_of: Option<usize>,
    pub hash_chain: &'static str,
    /// Byte offsets, as in the catalog anomalies.
    pub bin_entry_address: u64,
    pub info_address: u64,
    pub reachable_by_lookup: bool,
    pub selected_by_pathname: bool,
    pub info_problem: Option<&'static str>,
    pub info_detail: Option<String>,
    pub ts_supported: bool,
    pub password_protected: bool,
}

impl EntryProvenance {
    /// The typed outcome (`_file.py::_entry_outcome`, P16). Precedence: `not_live`,
    /// `unavailable`, `unsupported_record_type`, `recovered`, `unreachable_by_pathname`,
    /// `ordinary`.
    pub fn outcome(&self) -> &'static str {
        if !matches!(self.status, "live" | "alias") {
            "not_live"
        } else if self.info_problem.is_some() {
            "unavailable"
        } else if !self.ts_supported || self.password_protected {
            "unsupported_record_type"
        } else if self.recovered {
            "recovered"
        } else if !self.selected_by_pathname {
            "unreachable_by_pathname"
        } else {
            "ordinary"
        }
    }

    fn unavailable(&self) -> ReadError {
        ReadError {
            kind: ErrorKind::Unavailable,
            message: format!(
                "catalog entry {} ({}) is unavailable: {} (info block at byte {}); no data is \
                 returned",
                self.index,
                self.pathname,
                self.info_detail.as_deref().unwrap_or("invalid info block"),
                self.info_address
            ),
            unavailable: Some(UnavailableInfo {
                problem: self.info_problem.unwrap_or("unreadable"),
                index: self.index,
                info_address: self.info_address,
            }),
        }
    }

    fn unsupported(&self) -> ReadError {
        ReadError::new(
            ErrorKind::Unsupported,
            format!(
                "catalog entry {} ({}) has record type {}, which the time-series record reader \
                 does not decode (decision D29)",
                self.index, self.pathname, self.record_type
            ),
        )
    }

    fn not_live(&self) -> ReadError {
        ReadError::new(
            ErrorKind::Index,
            format!(
                "catalog entry {} is not a live record (status {})",
                self.index, self.status
            ),
        )
    }
}

fn index_out_of_range(index: usize) -> ReadError {
    ReadError::new(
        ErrorKind::Index,
        format!("catalog index {index} out of range"),
    )
}

fn chain_name(c: ChainState) -> &'static str {
    match c {
        ChainState::NotChecked => "not_checked",
        ChainState::OwnChain(_) => "own_chain",
        ChainState::OtherSlot => "other_slot",
        ChainState::Orphan => "orphan",
    }
}

fn info_problem_code(p: InfoProblem) -> &'static str {
    match p {
        InfoProblem::Zeroed => "zeroed",
        InfoProblem::BadFlag(_) => "bad_flag",
        InfoProblem::HashMismatch => "hash_mismatch",
        InfoProblem::Status(_) => "status",
        InfoProblem::PathnameMismatch => "pathname_mismatch",
        InfoProblem::OutOfBounds => "out_of_bounds",
    }
}

/// DSS7 types `read_ts_record` decodes (`headers/zdssKeys.h`, the Python `ts_supported7`).
fn ts_supported7(t: i32) -> bool {
    matches!(t, 100 | 101 | 105 | 106 | 110 | 111 | 115 | 116)
}

/// DSS6 types `decode_ts_info` decodes (`zrrtsx6.f`, `zritsx6.f`; `ts_supported6`).
fn ts_supported6(t: i32) -> bool {
    matches!(t, 100 | 101 | 105 | 110 | 111 | 115)
}

fn is_live_or_alias7(s: i64) -> bool {
    (dss7::status::VALID..=dss7::status::PRIMARY).contains(&s) || s == dss7::status::ALIAS
}

/// DSS7 provenance of catalog entry `index`, and the verified record when its info block
/// passes `record_at`'s checks.
fn provenance7(
    file: &mut Dss7File<Cursor<SharedBytes>>,
    cat: &Dss7Catalog,
    index: usize,
    recover: bool,
) -> ReadResult<(EntryProvenance, Option<RecordRef>)> {
    let (Some(e), Some(m)): (Option<&CatalogEntry>, Option<&Dss7EntryMeta>) =
        (cat.catalog.entries.get(index), cat.meta.get(index))
    else {
        return Err(index_out_of_range(index));
    };
    let live = is_live_or_alias7(m.raw_status);
    let entry_word = m.bin_address + i64::from(m.entry_offset);
    // Hash lookup of the stored pathname (`zcheck()`). An entry whose info block is bad
    // is still reached when the lookup fails on its info block.
    let reachable_by_lookup = live
        && match file.lookup(&e.pathname) {
            Ok(Some(r)) => r.bin_entry_address == entry_word,
            Ok(None) => false,
            Err(dss7::Dss7Error::BadInfoBlock { info_address, .. }) => {
                info_address == m.info_address
            }
            Err(dss7::Dss7Error::Io(io)) => {
                return Err(ReadError::new(ErrorKind::Format, io.to_string()))
            }
            Err(_) => false,
        };
    // Recovery mode: `find` returns the one entry of a duplicate group whose
    // `duplicate_of` is unset (walk.rs `duplicate_groups`).
    let selected_by_pathname = live
        && if recover {
            m.duplicate_of.is_none()
        } else {
            reachable_by_lookup
        };
    let (mut info_problem, mut info_detail, mut rec) = (None, None, None);
    if live {
        match file.record_at(cat, index) {
            Ok(r) => rec = Some(r),
            Err(dss7::Dss7Error::BadInfoBlock { problem, .. }) => {
                info_problem = Some(info_problem_code(problem));
                info_detail = Some(problem.to_string());
            }
            Err(dss7::Dss7Error::Io(io)) => {
                return Err(ReadError::new(ErrorKind::Format, io.to_string()))
            }
            Err(other) => {
                info_problem = Some("unreadable");
                info_detail = Some(other.to_string());
            }
        }
    }
    Ok((
        EntryProvenance {
            index,
            version: 7,
            pathname: lossy(&e.pathname),
            record_type: e.record_type,
            status: status_name(e.status),
            raw_status: m.raw_status,
            recover_mode: recover,
            recovered: m.recovered,
            duplicate: m.duplicate_of.is_some(),
            duplicate_of: m.duplicate_of,
            hash_chain: chain_name(m.chain),
            bin_entry_address: e.bin_address,
            info_address: e.info_address,
            reachable_by_lookup,
            selected_by_pathname,
            info_problem,
            info_detail,
            ts_supported: ts_supported7(e.record_type),
            password_protected: false,
        },
        rec,
    ))
}

/// DSS6 provenance of catalog entry `index`, and the catalog entry when its info block
/// verifies. DSS6 has no recovery mode and no orphan check.
fn provenance6(
    file: &Dss6File<Vec<u8>>,
    walk: &dss6::BinWalk,
    index: usize,
) -> ReadResult<(EntryProvenance, Option<CatalogEntry>)> {
    let entry = walk
        .entries
        .get(index)
        .ok_or_else(|| index_out_of_range(index))?;
    let st = entry.record_status();
    let live = matches!(st, RecordStatus::Live | RecordStatus::Alias);
    let duplicate_of = if st == RecordStatus::Live {
        walk.entries[..index]
            .iter()
            .position(|o| o.record_status() == RecordStatus::Live && o.pathname == entry.pathname)
    } else {
        None
    };
    let reachable_by_lookup = live
        && match file.lookup(&entry.pathname) {
            Ok(Some(r)) => r.entry.entry_address == entry.entry_address,
            Ok(None) => false,
            Err(dss6::Dss6Error::Io(io)) => {
                return Err(ReadError::new(ErrorKind::Format, io.to_string()))
            }
            Err(_) => false,
        };
    let ce = entry.to_catalog_entry();
    let (mut info_problem, mut info_detail) = (None, None);
    let (mut info_type, mut password_protected) = (None, false);
    if live {
        let npath = i32::try_from(entry.pathname.len()).unwrap_or(i32::MAX);
        // Same checks as `Dss6File::read_ts_entry` (zrdinf6.f: flag and pathname).
        match file.read_info(i64::from(entry.info_address), npath) {
            Ok(info) if !info.has_valid_flag() => {
                info_problem = Some(if info.flag == 0 { "zeroed" } else { "bad_flag" });
                info_detail = Some(format!("info flag {}", info.flag));
            }
            Ok(info) if info.pathname != entry.pathname => {
                info_problem = Some("pathname_mismatch");
                info_detail = Some("info pathname differs from bin pathname".to_string());
            }
            Ok(info) => {
                info_type = Some(info.record_type);
                password_protected = file.is_record_denied(&info);
            }
            Err(dss6::Dss6Error::Io(io)) => {
                return Err(ReadError::new(ErrorKind::Format, io.to_string()))
            }
            Err(e @ (dss6::Dss6Error::OutOfBounds { .. } | dss6::Dss6Error::LockRecord { .. })) => {
                info_problem = Some("out_of_bounds");
                info_detail = Some(e.to_string());
            }
            Err(e) => {
                // An all-zero block fails decoding on its zero pathname length (`KINPAT`);
                // report it as zeroed, like the DSS7 flag check.
                let zero = file
                    .read_words(i64::from(entry.info_address), 3)
                    .is_ok_and(|w| w.iter().all(|&x| x == 0));
                info_problem = Some(if zero { "zeroed" } else { "unreadable" });
                info_detail = Some(if zero {
                    "info block is zeroed".to_string()
                } else {
                    e.to_string()
                });
            }
        }
    }
    let p = EntryProvenance {
        index,
        version: 6,
        pathname: lossy(&entry.pathname),
        record_type: entry.record_type,
        status: status_name(st),
        raw_status: i64::from(entry.status),
        recover_mode: false,
        recovered: false,
        duplicate: duplicate_of.is_some(),
        duplicate_of,
        hash_chain: chain_name(ChainState::NotChecked),
        bin_entry_address: ce.bin_address,
        info_address: ce.info_address,
        reachable_by_lookup,
        selected_by_pathname: reachable_by_lookup,
        info_problem,
        info_detail,
        ts_supported: ts_supported6(info_type.unwrap_or(entry.record_type)),
        password_protected,
    };
    let ok = (live && p.info_problem.is_none()).then_some(ce);
    Ok((p, ok))
}

/// A catalog index from JavaScript: a whole number. A negative or too-large value is an
/// `Index` error, like Python's `IndexError`; anything else not whole is invalid.
pub fn index_arg(v: f64) -> ReadResult<usize> {
    if !v.is_finite() || v.fract() != 0.0 {
        return Err(ReadError::new(
            ErrorKind::InvalidArgument,
            format!("catalog index must be a whole number, got {v}"),
        ));
    }
    if v < 0.0 || v > (u32::MAX as f64) {
        return Err(ReadError::new(
            ErrorKind::Index,
            format!("catalog index {v} out of range"),
        ));
    }
    Ok(v as usize)
}

impl DssBuffer {
    /// Provenance of a row of [`DssBuffer::catalog`] (all statuses, in this file's open
    /// mode), without decoding values.
    pub fn entry_provenance(&mut self, index: usize) -> ReadResult<EntryProvenance> {
        let recover = self.recover;
        match &mut self.inner {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
                ..
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover)?;
                Ok(provenance7(file, cat, index, recover)?.0)
            }
            Inner::V6 { file, walk, .. } => {
                let w = v6_walk(file, walk)?;
                Ok(provenance6(file, w, index)?.0)
            }
        }
    }

    /// Forensic read of the stored time-series record behind a catalog row. Reaches
    /// entries an ordinary pathname read cannot: later copies of duplicated pathnames,
    /// DSS7 orphan-bin entries, and (in recovery mode) entries in unlinked bins. The info
    /// flag and hash checks apply as for any read; a failed check is `Unavailable` and no
    /// data is returned. Non-time-series types are `Unsupported`; a row out of range or
    /// not live is `Index`.
    pub fn read_record_at(&mut self, index: usize) -> ReadResult<(EntryProvenance, TsData)> {
        let recover = self.recover;
        match &mut self.inner {
            Inner::V7 {
                file,
                catalog,
                recover_catalog,
                ..
            } => {
                let slot = if recover { recover_catalog } else { catalog };
                let cat = v7_catalog(file, slot, recover)?;
                let (p, rec) = provenance7(file, cat, index, recover)?;
                if !is_live_or_alias7(p.raw_status) {
                    return Err(p.not_live());
                }
                let Some(rec) = rec else {
                    return Err(p.unavailable());
                };
                if !p.ts_supported {
                    return Err(p.unsupported());
                }
                let ts = file
                    .read_ts_record(&rec)
                    .map(from_dss7_ts)
                    .map_err(map_ts7)?;
                Ok((p, ts))
            }
            Inner::V6 { file, walk, .. } => {
                let w = v6_walk(file, walk)?;
                let (p, ce) = provenance6(file, w, index)?;
                if !matches!(p.status, "live" | "alias") {
                    return Err(p.not_live());
                }
                let Some(ce) = ce else {
                    return Err(p.unavailable());
                };
                // Password-protected records fall through to the decoder, which raises
                // the documented `Dss` error (D32 table).
                if !p.ts_supported {
                    return Err(p.unsupported());
                }
                let ts = file
                    .read_ts_entry(&ce, &dss6::TsReadOptions::default())
                    .map(from_dss6_record)
                    .map_err(|e| {
                        let m = map_ts6(e);
                        if m.kind == ErrorKind::Unsupported {
                            ReadError::new(m.kind, format!("{} ({})", m.message, p.pathname))
                        } else {
                            m
                        }
                    })?;
                Ok((p, ts))
            }
        }
    }
}

fn v6_walk<'a>(
    file: &Dss6File<Vec<u8>>,
    slot: &'a mut Option<Box<dss6::BinWalk>>,
) -> ReadResult<&'a dss6::BinWalk> {
    if slot.is_none() {
        let w = file
            .walk_bins(&dss6::CatalogOptions::default())
            .map_err(map_dss6)?;
        *slot = Some(Box::new(w));
    }
    Ok(slot.as_deref().expect("walk just set"))
}

/// Whether a location came from a DSS7 record or a DSS6 time-series header.
pub fn location_source_name(s: LocationSource) -> &'static str {
    match s {
        LocationSource::Dss7Record => "dss7_location_record",
        LocationSource::Dss6TsHeader => "dss6_ts_header",
    }
}

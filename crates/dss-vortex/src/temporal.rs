//! Gridded time tools: the time shifter and the time-step resampler.
//!
//! Sources, Vortex 1.1.0 (0.13.3 has the same arithmetic; the differences are in error handling and
//! logging): `math/TimeShifter`, `math/BatchTimeStepResampler`, `math/TimeStep`,
//! `io/TemporalDataReader`, `io/TemporalDataCalculator`, `io/VortexDataInterval`, `io/IntervalTree`,
//! `io/PeriodRecordIndexQuery`, `io/InstantaneousRecordIndexQuery`, `io/DssDataReader.dssToDto`,
//! `util/DssUtil.condenseVariables`. No Java runs here; Vortex is used only as a test
//! oracle.
//!
//! **Times** have no zone. A DSS grid's stamps are local clock stamps with no daylight saving
//! (`DssDataReader` reads them as UTC), so a shift is plain arithmetic on whole seconds, and a shift
//! across a date on which some place changes its clocks moves the stamp by exactly the shift.
//!
//! **2400.** `01JAN2024:2400` and `02JAN2024:0000` are one instant on reading, in a D or an E part
//! (`DssDataReader.getEndTime`; `java.time`'s smart resolver for a D part). On writing, a period's D
//! part says `0000` and its E part `2400`, and an INST-VAL record's only part says `2400`
//! ([`crate::sink_dss::time_parts`]). Two records in one series that state one window are refused.
//!
//! **CRS.** The record's CRS comes from `grid_tools::dss_source::crs_of`, which the grid tools share, as
//! `WktFactory.fromGridInfo` gives it. SHG stays an Albers record. **Any other Albers grid in metres, on
//! NAD83 or NAD27, and an HRAP grid become specified grids**, holding the text Vortex stores, whose HRAP
//! false easting and northing state HRAP's own origin: the name `unnamed` does not say Albers. These stay
//! refused: Albers units other than metres, whatever the other parameters, since the cell size and origin
//! keep their numbers across a tool; an Albers datum that names none; an undefined grid; a projection the
//! CRS module cannot read, such as Mercator; and a cell zero other than the CRS's origin. Transverse
//! Mercator is read since P53b, and its grids keep the text they state.
//!
//! **Alignment.** Resampled windows start at midnight of the day holding the series' first time,
//! never at that first time, and step by a fixed length (`BatchTimeStepResampler.process`). So:
//!
//! - A window that begins before the first record is written with **every cell missing**
//!   (`TemporalDataCalculator.hasNoTargetData`). It holds no partial volume.
//! - A window that would end after the series is **not written**. The time it leaves out is reported
//!   as [`TemporalReport::dropped_tail_seconds`].
//! - `WEEK`, `MONTH` and `YEAR` are 7, 30 and 365 days (`TimeStep`), not calendar periods.
//!
//! **Cells**, per `TemporalDataCalculator`, in Vortex's order and float widths:
//!
//! - PER-CUM: the sum of each source's cell times the share of **that source** inside the window.
//! - PER-AVER: the sum of each source's cell times the share of **the window** it covers.
//! - INST-VAL over a period: the trapezoid between neighbours. At an instant: linear between the two.
//! - A cell that is `-FLT_MAX` in a source a window uses is missing in that window, also for a source
//!   whose overlap comes to nothing. Missing is never counted as zero. NaN flows through the
//!   arithmetic and the writer stores it as missing.
//! - **-901 and -902.** This engine's reader takes both as missing (`dss_core::grid::Grid::is_missing`),
//!   and Vortex treats both as numbers. A result stored as either is refused before anything is
//!   written, naming the record and its time: a mean or an accumulation that lands there through the
//!   `f32` cast, a unit conversion that does, and a shifted cell that already holds one. The whole run
//!   is computed first for this, so a resampler run reads and computes each window twice. Under
//!   [`Rule::Strict`], a source cell holding either is refused where a window would use it, and a result
//!   that overflows `f32` is refused, since the writer would store the infinity as missing. Under
//!   [`Rule::Vortex`] such a source cell is a number, as there.
//! - A series typed INST-CUM takes its rule from `VortexGrid.inferDataType`, on its first record: an
//!   instant is INST-VAL's rule, and a period is PER-CUM's under PRECIPITATION and PER-AVER's otherwise.
//!   This follows 1.1.0. Vortex 0.13.3 infers INST-VAL's rule for a period of most variables, and then
//!   drops every record, so it writes nothing there.
//!
//! Observed behaviour we did not implement, each on Vortex 1.1.0 and 0.13.3 unless [`Rule::Vortex`]
//! is stated:
//!
//! - **A gap inside a window.** The sources present are summed, so a PER-CUM window with one of six
//!   hours absent holds five hours' depth, and a PER-AVER window is averaged over the whole window
//!   with nothing for the gap. This library refuses the run while planning, naming the window. A gap
//!   touching a window's start or end is not this case: `hasNoTargetData` makes every cell missing.
//! - **A missing cell in a period of INST-VAL.** `calculateAverageOfTwoGrids` carries the marker as a
//!   number and the total is divided by the window's minutes, so the cell holds about
//!   `-3.4e38 / minutes`, which readers take as a value. This library stores it as missing.
//! - **A period series read at an instant** through [`read_window`]: PER-CUM gives zeros and PER-AVER
//!   NaN. This library refuses.
//! - **A source cell of -901 or -902.** Vortex uses it as a number. This library refuses.
//! - **A specified grid's own null value in a cell.** `DssDataReader` uses it as a number, while this
//!   engine's reader takes it as missing. The shifter refuses, under every rule, since writing it would
//!   store a value in place of a missing cell. The resampler refuses under [`Rule::Strict`].
//!
//! Observed behaviour we did not implement under either rule:
//!
//! - **One window stated twice in a series**, for example with both `:2400` and `:0000`. Vortex keeps
//!   the later record in catalog order. This library refuses.
//! - **A series whose records differ** in grid, CRS, units or data type. Vortex takes all of these
//!   from the catalog's first record. This library refuses.
//! - **No complete window.** Vortex finishes with no output and no message. This library refuses.
//! - **A selection matching nothing, and a shift that ends a record before it starts.** Refused.
//! - **A selected record that does not exist**: `DataReader` logs it and goes on. Refused.
//! - **Writing to the input file.** Refused, by the file's identity and not its path, so a hard link or
//!   a symlink to the input is refused too: a replace could feed a record back into its own run.
//! - **A result of -901 or -902.** Vortex stores it, and this engine's reader then calls it missing.
//!   Refused.
//! - **An Albers grid whose units do not state metres.** Vortex treats units holding `foot` or `us` as
//!   `Foot_US` and everything else as metres, and its SHG test does not read the units. Refused.
//! - **`partA` or `partB` of `*`.** Vortex leaves such a part empty, as it does an absent one: neither
//!   the shifter nor the resampler keeps a source's A or B part. Refused, so that nobody takes `*` to
//!   mean "keep". `F` of `*` does keep the source's F part, as there.
//!
//! **Rerunning over an existing output.** Each record written replaces the record at that pathname. A
//! record outside this run's windows stays as it was, cell for cell, and nothing marks it as coming from
//! an earlier run: rerunning a shorter series over a longer one leaves the earlier run's later windows
//! there. Vortex does the same. Use a new output to have only this run's records.
//!
//! **An input that changes during a run.** Its length and modified time are noted before planning, and
//! checked after planning and before each write. A change stops the run, with `completed` as usual and
//! nothing in doubt. A change that keeps both is not seen. Nothing here holds a snapshot of the input.
//!
//! **A run that stops.** [`TemporalError::completed`] lists the writes the writer acknowledged.
//! [`TemporalError::in_doubt`] names the one being written if the writer failed: it may or may not be
//! in the file, whole.

// `TemporalError` carries the writer's whole lock report and every finished record, as `ImportError`
// does. It is built once, on the way out of a run.
#![allow(clippy::result_large_err)]

use crate::error::VortexError;
use crate::file_identity;
use crate::frame::{Crs, DataType, FrameTime, GeoTransform, GridFrameParts, RowOrder};
use crate::grid_tools::dss_source;
use crate::import::{self, Failure};
use crate::sink_dss::{self, Parts, PlannedRecord, SinkCrs, SinkOptions, WrittenRecord};
use crate::units::{self, Overrides};
use crate::warp::Align;
use dss_core::dss6::{Dss6File, FileSource};
use dss_core::dss7::write::{FaultPoint, WriteError};
use dss_core::dss7::Dss7File;
use dss_core::grid::{AlbersProjection, Grid, GridType};
use dss_core::grid_write::{self, GRID_NODATA};
use dss_core::lock::{self, LockPolicy};
use dss_core::{DssVersion, FileSignature};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

const DAY: i64 = 86_400;

/// `math/TimeStep.java`, in minutes: 1 to 30 minutes, 1 to 24 hours, then `WEEK`, `MONTH`, `YEAR`.
pub const TIME_STEP_MINUTES: [i64; 21] = [
    1, 2, 3, 4, 5, 6, 10, 15, 20, 30, 60, 120, 180, 240, 360, 480, 720, 1440, 10080, 43200, 525600,
];

/// `Heclib.UNDEFINED_DOUBLE`: `-FLT_MAX` widened, not `-DBL_MAX` (`javap -constants`, Monolith 3.3.28).
const UNDEFINED_DOUBLE: f64 = GRID_NODATA as f64;

/// Whether Vortex's observed behaviours listed in the module's notes are reproduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Refuse a window with a gap and a period series read at an instant; store a missing INST-VAL
    /// cell as missing. Every other result is Vortex's.
    Strict,
    /// Vortex's results, for parity.
    Vortex,
}

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

/// `(julian, seconds)` as whole seconds. 86400 seconds of a day is 0 of the next.
fn seconds_of((julian, seconds): (i64, i64)) -> Result<i64, VortexError> {
    julian
        .checked_mul(DAY)
        .and_then(|d| d.checked_add(seconds))
        .ok_or_else(|| refused("time", format!("julian day {julian} is out of range")))
}

/// Whole seconds as `(julian, seconds)`, with seconds in `0..86400`.
fn pair_of(total: i64) -> (i64, i64) {
    (total.div_euclid(DAY), total.rem_euclid(DAY))
}

/// A frame's time moved by whole seconds (`TimeShifter.shift`): start by `shift_start`, end by
/// `shift_end`. An instant is a window whose ends are equal, so moving them apart makes a period, and
/// a period whose ends meet becomes an instant, as there. A period ending before it starts is refused.
pub fn shift_time(
    time: &FrameTime,
    shift_start: i64,
    shift_end: i64,
) -> Result<FrameTime, VortexError> {
    let (start, end) = match *time {
        FrameTime::Invariant => return Ok(FrameTime::Invariant),
        FrameTime::Instant(t) => (seconds_of(t)?, seconds_of(t)?),
        FrameTime::Period { start, end } => (seconds_of(start)?, seconds_of(end)?),
    };
    let moved = |t: i64, by: i64| {
        t.checked_add(by)
            .ok_or_else(|| refused("time shift", format!("{by} s is out of range")))
    };
    let (start, end) = (moved(start, shift_start)?, moved(end, shift_end)?);
    if end < start {
        return Err(refused(
            "time shift",
            format!(
                "it would end a record {} s before it starts. Observed behaviour we did not \
                 implement: Vortex 1.1.0 writes such a window",
                start - end
            ),
        ));
    }
    Ok(if start == end {
        FrameTime::Instant(pair_of(start))
    } else {
        FrameTime::Period {
            start: pair_of(start),
            end: pair_of(end),
        }
    })
}

/// `VortexDataType` of a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesKind {
    Accumulation,
    Average,
    Instantaneous,
}

impl SeriesKind {
    /// `VortexGrid.dataType()`. `VortexDataType.fromString` has no INST-CUM, so that type is
    /// `inferDataType`'s, from the first record's window and its C part (Vortex 1.1.0).
    pub fn of(data_type: DataType, c_part: &str, first: Window) -> SeriesKind {
        match data_type {
            DataType::PerCum => SeriesKind::Accumulation,
            DataType::PerAver => SeriesKind::Average,
            DataType::InstVal => SeriesKind::Instantaneous,
            DataType::InstCum if first.start == first.end => SeriesKind::Instantaneous,
            DataType::InstCum => match units::variable_from_name(c_part) {
                Some(units::Variable::Precipitation) => SeriesKind::Accumulation,
                _ => SeriesKind::Average,
            },
        }
    }
}

/// One record's window, in whole seconds. An instant has `start == end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

impl Window {
    /// `VortexDataInterval.getOverlapDuration`.
    fn overlap(self, other: Window) -> i64 {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if end > start {
            end - start
        } else {
            0
        }
    }

    /// `Interval.overlaps`: closed-open.
    fn overlaps(self, other: Window) -> bool {
        self.end > other.start && other.end > self.start
    }
}

/// What a window reads and how, before any cell is touched.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowPlan {
    /// `hasNoTargetData`: the series does not span the window. Every cell is missing.
    Missing,
    /// `calculateWeightedData`, sources in this order.
    Weighted {
        sources: Vec<(usize, f64)>,
        /// Seconds of the window no selected source covers. Always 0 for an INST-VAL instant.
        gap_seconds: i64,
    },
    /// `calculatePeriodInstant`, instants in time order.
    Trapezoid { sources: Vec<usize> },
}

impl WindowPlan {
    pub fn sources(&self) -> Vec<usize> {
        match self {
            WindowPlan::Missing => Vec::new(),
            WindowPlan::Weighted { sources, .. } => sources.iter().map(|s| s.0).collect(),
            WindowPlan::Trapezoid { sources } => sources.clone(),
        }
    }
}

/// A series' windows, indexed the way `RecordIndexQuery` does. Metadata only.
#[derive(Debug, Clone)]
pub struct TemporalSeries {
    kind: SeriesKind,
    windows: Vec<Window>,
    // Indices by start time, stably: the order `IntervalTree` builds from.
    by_start: Vec<usize>,
    // INST-VAL only: (time, index), by time. `InstantaneousRecordIndexQuery` ignores spanning records.
    instants: Vec<(i64, usize)>,
}

impl TemporalSeries {
    /// `windows` in the order records are listed. A window stated twice is refused: see the module.
    pub fn new(kind: SeriesKind, windows: Vec<Window>) -> Result<Self, VortexError> {
        let mut seen = HashSet::new();
        for w in &windows {
            if w.end < w.start {
                return Err(refused(
                    "series",
                    format!("a record ends at {} before it starts at {}", w.end, w.start),
                ));
            }
            if !seen.insert(*w) {
                return Err(refused(
                    "series",
                    format!(
                        "two records state the window {:?}, for example with :2400 and with :0000. \
                         Observed behaviour we did not implement: Vortex 1.1.0 and 0.13.3 use the \
                         later record in catalog order",
                        w
                    ),
                ));
            }
        }
        let mut by_start: Vec<usize> = (0..windows.len()).collect();
        by_start.sort_by_key(|&i| windows[i].start);
        let mut instants: Vec<(i64, usize)> = match kind {
            SeriesKind::Instantaneous => windows
                .iter()
                .enumerate()
                .filter(|(_, w)| w.start == w.end)
                .map(|(i, w)| (w.start, i))
                .collect(),
            _ => Vec::new(),
        };
        instants.sort_by_key(|e| e.0);
        Ok(TemporalSeries {
            kind,
            windows,
            by_start,
            instants,
        })
    }

    pub fn kind(&self) -> SeriesKind {
        self.kind
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// `getEarliestStartTime`.
    pub fn start_time(&self) -> Option<i64> {
        match self.kind {
            SeriesKind::Instantaneous => self.instants.first().map(|e| e.0),
            _ => self.by_start.first().map(|&i| self.windows[i].start),
        }
    }

    /// `getLatestEndTime`. **Not always the latest end.** `IntervalTree.findMaximum` walks right while
    /// the right subtree holds the largest end, then returns the node it stands on, whose own end can
    /// be earlier when the largest end is to its left: `[0,24) [1,2) [2,3)` gives 2. Ported as it is,
    /// because it decides how many windows the resampler writes. Tree shape: `buildTree`, `mid = (lo + hi) / 2`.
    pub fn end_time(&self) -> Option<i64> {
        if self.kind == SeriesKind::Instantaneous {
            return self.instants.last().map(|e| e.0);
        }
        if self.by_start.is_empty() {
            return None;
        }
        let end_at = |i: usize| self.windows[self.by_start[i]].end;
        let (mut lo, hi) = (0usize, self.by_start.len() - 1);
        let largest = (lo..=hi).map(end_at).max()?;
        loop {
            let mid = (lo + hi) / 2;
            if mid < hi && (mid + 1..=hi).map(end_at).max() == Some(largest) {
                lo = mid + 1;
            } else {
                return Some(end_at(mid));
            }
        }
    }

    /// `PeriodRecordIndexQuery.query`. The tree only prunes, so a scan finds the same records.
    fn query_period(&self, target: Window) -> Vec<usize> {
        self.by_start
            .iter()
            .copied()
            .filter(|&i| self.windows[i].overlaps(target))
            .collect()
    }

    /// `InstantaneousRecordIndexQuery.queryPoint`.
    fn query_point(&self, t: i64) -> Vec<usize> {
        let floor = self.instants.iter().rev().find(|e| e.0 <= t);
        let ceiling = self.instants.iter().find(|e| e.0 >= t);
        match (floor, ceiling) {
            (Some(f), Some(c)) if f.1 == c.1 => vec![f.1],
            (Some(f), Some(c)) => vec![f.1, c.1],
            _ => Vec::new(),
        }
    }

    /// `InstantaneousRecordIndexQuery.queryPeriod`: from the instant at or before the start to the
    /// instant at or after the end, both included.
    fn query_span(&self, start: i64, end: i64) -> Vec<usize> {
        let from = self
            .instants
            .iter()
            .rev()
            .find(|e| e.0 <= start)
            .map_or(start, |e| e.0);
        let to = self
            .instants
            .iter()
            .find(|e| e.0 >= end)
            .map_or(end, |e| e.0);
        self.instants
            .iter()
            .filter(|e| e.0 >= from && e.0 <= to)
            .map(|e| e.1)
            .collect()
    }

    /// `TemporalDataReader.getGridsForPeriod`: by end time, then shorter first; a record is used only
    /// if it does not overlap what is covered so far. The third comparator, less missing data, breaks
    /// ties between records stating one window, and `TemporalSeries::new` refuses those.
    fn select_period(&self, target: Window) -> Vec<usize> {
        let mut found: Vec<usize> = self
            .query_period(target)
            .into_iter()
            .filter(|&i| self.windows[i].end > self.windows[i].start)
            .collect();
        found.sort_by_key(|&i| {
            let w = self.windows[i];
            (w.end, w.end - w.start)
        });
        let mut covered: Option<Window> = None;
        let mut used = Vec::new();
        for i in found {
            let w = self.windows[i];
            match covered {
                None => {
                    used.push(i);
                    covered = Some(w);
                }
                Some(c) if !c.overlaps(w) => {
                    used.push(i);
                    covered = Some(Window {
                        start: c.start,
                        end: w.end,
                    });
                }
                Some(_) => {}
            }
        }
        used
    }

    /// `hasNoTargetData`, on the first and last sources in their list order.
    fn spans(&self, sources: &[usize], target: Window) -> bool {
        match (sources.first(), sources.last()) {
            (Some(&first), Some(&last)) => {
                target.start >= self.windows[first].start && target.end <= self.windows[last].end
            }
            _ => false,
        }
    }

    /// What `TemporalDataReader.read` would read for a window, and with what weights.
    pub fn plan(&self, target: Window) -> WindowPlan {
        match self.kind {
            SeriesKind::Accumulation | SeriesKind::Average => {
                let used = self.select_period(target);
                if !self.spans(&used, target) {
                    return WindowPlan::Missing;
                }
                let total = target.end - target.start;
                let mut covered = 0i64;
                let mut sources = Vec::with_capacity(used.len());
                for &i in &used {
                    let w = self.windows[i];
                    let overlap = w.overlap(target);
                    covered += overlap;
                    let weight = if self.kind == SeriesKind::Accumulation {
                        // `calculateAccumulationWeights`, `getPercentOverlapped`.
                        overlap as f64 / (w.end - w.start) as f64
                    } else {
                        // `calculateAverageWeights`.
                        overlap as f64 / total as f64
                    };
                    sources.push((i, weight));
                }
                WindowPlan::Weighted {
                    sources,
                    gap_seconds: total - covered,
                }
            }
            SeriesKind::Instantaneous if target.start == target.end => {
                let used = self.query_point(target.start);
                if !self.spans(&used, target) {
                    return WindowPlan::Missing;
                }
                // `calculatePointInstantWeights`.
                let sources = match used[..] {
                    [only] => vec![(only, 1.0)],
                    [floor, ceiling] => {
                        let (a, b) = (self.windows[floor].start, self.windows[ceiling].start);
                        let fraction = (target.start - a) as f64 / (b - a) as f64;
                        vec![(floor, 1.0 - fraction), (ceiling, fraction)]
                    }
                    _ => return WindowPlan::Missing,
                };
                WindowPlan::Weighted {
                    sources,
                    gap_seconds: 0,
                }
            }
            SeriesKind::Instantaneous => {
                let used = self.query_span(target.start, target.end);
                if !self.spans(&used, target) {
                    return WindowPlan::Missing;
                }
                WindowPlan::Trapezoid { sources: used }
            }
        }
    }

    /// `TemporalDataReader.readNearest`: the record whose start is nearest, then the shorter, then the
    /// earlier, among `queryNearest`'s candidates. Records sharing a start depend on catalog order.
    pub fn nearest(&self, t: i64) -> Option<usize> {
        let candidates: Vec<usize> = if self.kind == SeriesKind::Instantaneous {
            let floor = self.instants.iter().rev().find(|e| e.0 <= t).map(|e| e.1);
            let ceiling = self.instants.iter().find(|e| e.0 >= t).map(|e| e.1);
            floor.into_iter().chain(ceiling).collect()
        } else {
            self.nearest_period(t)
        };
        // `Stream.min` keeps the first of equals.
        let mut best: Option<(usize, (i64, i64, i64))> = None;
        for i in candidates {
            let w = self.windows[i];
            let key = ((w.start - t).abs(), w.end - w.start, w.start);
            if best.is_none_or(|(_, k)| key < k) {
                best = Some((i, key));
            }
        }
        best.map(|b| b.0)
    }

    /// `IntervalTree.findNearest`. The gap to the predecessor is measured from its **end**, so it is
    /// negative when the query stands on a start, and that record then wins.
    fn nearest_period(&self, t: i64) -> Vec<usize> {
        let overlaps = self.query_period(Window { start: t, end: t });
        if !overlaps.is_empty() {
            return overlaps;
        }
        let pred = self
            .by_start
            .iter()
            .rev()
            .copied()
            .find(|&i| self.windows[i].start <= t);
        let succ = self
            .by_start
            .iter()
            .copied()
            .find(|&i| self.windows[i].start >= t);
        let pred_gap = pred.map_or(i64::MAX, |i| t - self.windows[i].end);
        let succ_gap = succ.map_or(i64::MAX, |i| self.windows[i].start - t);
        let gap = pred_gap.min(succ_gap);
        let mut out = Vec::with_capacity(2);
        if let Some(i) = pred.filter(|_| pred_gap == gap) {
            out.push(i);
        }
        if let Some(i) = succ.filter(|_| succ_gap == gap) {
            out.push(i);
        }
        out
    }

    /// `getMinMaxForPeriodGrids` and `getMinMaxForInstantaneousGrids`. The latter always drops the last
    /// record `query` found, whether or not it lies past the window, then those starting before the
    /// window. Both as there. `None` means there is no data: Vortex raises on an empty instant list.
    pub fn min_max_sources(&self, target: Window) -> Option<Vec<usize>> {
        if self.kind == SeriesKind::Instantaneous {
            let mut found = if target.start == target.end {
                self.query_point(target.start)
            } else {
                self.query_span(target.start, target.end)
            };
            found.pop()?;
            found.retain(|&i| self.windows[i].start >= target.start);
            Some(found)
        } else {
            Some(self.query_period(target))
        }
    }
}

/// `calculateWeightedData`'s step for one source. `result` starts at zero. A marker in either makes
/// the marker stick; NaN is not the marker and flows through the arithmetic.
pub fn accumulate(result: &mut [f32], cells: &[f32], weight: f64) {
    let marker = f64::from(GRID_NODATA);
    for (r, &cell) in result.iter_mut().zip(cells) {
        if f64::from(*r) == marker || f64::from(cell) == marker {
            *r = GRID_NODATA;
        } else {
            let weighted = f64::from(cell) * weight;
            *r = (f64::from(*r) + weighted) as f32;
        }
    }
}

/// `calculateAverageOfTwoGrids`, added to `result`. `overlap` and `whole` are truncated minutes, as
/// the Java's `(int) toMinutes()` are. `adjust_start` and `adjust_end` are its two flags.
///
/// `strict` leaves a cell with a missing end as the marker for good. Otherwise the marker is carried
/// as a number, as there, and [`finish_trapezoid`] divides it.
#[allow(clippy::too_many_arguments)]
pub fn add_trapezoid(
    result: &mut [f32],
    first: &[f32],
    second: &[f32],
    overlap: i32,
    whole: i32,
    adjust_start: bool,
    adjust_end: bool,
    strict: bool,
) {
    let (overlap, whole) = (f64::from(overlap), f64::from(whole));
    for ((r, &a), &b) in result.iter_mut().zip(first).zip(second) {
        let (mut value1, mut value2) = (f64::from(a), f64::from(b));
        let avg = if value1 == UNDEFINED_DOUBLE || value2 == UNDEFINED_DOUBLE {
            UNDEFINED_DOUBLE
        } else {
            let difference = value2 - value1;
            if adjust_start {
                value1 = value2 - overlap * difference / whole;
            }
            if adjust_end {
                value2 = value1 + overlap * difference / whole;
            }
            0.5 * (value1 + value2) * overlap
        };
        if strict && (*r == GRID_NODATA || avg == UNDEFINED_DOUBLE) {
            *r = GRID_NODATA;
        } else {
            *r = (f64::from(*r) + avg) as f32;
        }
    }
}

/// `calculatePeriodInstant`'s last step: `result[i] /= totalInterval`, in `f32`.
pub fn finish_trapezoid(result: &mut [f32], total_minutes: i32, strict: bool) {
    let total = total_minutes as f32;
    for r in result.iter_mut() {
        if !(strict && *r == GRID_NODATA) {
            *r /= total;
        }
    }
}

/// `getMinMaxForGrids`. Starts at `FLT_MAX` and `-FLT_MAX`; a cell no source holds stays there, so
/// its minimum is `FLT_MAX`, which reads as a value, and its maximum `-FLT_MAX`, which reads as missing.
pub fn min_max_step(min: &mut [f32], max: &mut [f32], cells: &[f32]) {
    let marker = f64::from(GRID_NODATA);
    for ((lo, hi), &cell) in min.iter_mut().zip(max.iter_mut()).zip(cells) {
        if f64::from(cell) == marker {
            continue;
        }
        if cell < *lo {
            *lo = cell;
        }
        if cell > *hi {
            *hi = cell;
        }
    }
}

// Reserve fallibly: an abort cannot be caught.
fn filled(n: usize, value: f32) -> Result<Vec<f32>, VortexError> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).map_err(|_| {
        VortexError::Io(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!("no memory for a grid of {n} cells"),
        ))
    })?;
    out.resize(n, value);
    Ok(out)
}

/// Compute one window, reading each source once, no more than two at a time in memory.
///
/// `read(i)` returns source `i`'s cells, each with `n` cells.
pub fn compute_window(
    series: &TemporalSeries,
    plan: &WindowPlan,
    target: Window,
    n: usize,
    rule: Rule,
    read: &mut dyn FnMut(usize) -> Result<Vec<f32>, VortexError>,
) -> Result<Vec<f32>, VortexError> {
    let check = |cells: Vec<f32>| {
        if cells.len() == n {
            Ok(cells)
        } else {
            Err(refused(
                "series",
                format!("a record holds {} cells, not {n}", cells.len()),
            ))
        }
    };
    match plan {
        // `buildGrid`: an empty result is filled with the base grid's marker.
        WindowPlan::Missing => filled(n, GRID_NODATA),
        WindowPlan::Weighted { sources, .. } => {
            let mut result = filled(n, 0.0)?;
            for &(i, weight) in sources {
                accumulate(&mut result, &check(read(i)?)?, weight);
            }
            Ok(result)
        }
        WindowPlan::Trapezoid { sources } => {
            let strict = rule == Rule::Strict;
            let mut result = filled(n, 0.0)?;
            let mut remaining = target.start;
            let mut first = None;
            for pair in sources.windows(2) {
                let a = match first.take() {
                    Some(cells) => cells,
                    None => check(read(pair[0])?)?,
                };
                let b = check(read(pair[1])?)?;
                let span = Window {
                    start: series.windows[pair[0]].start,
                    end: series.windows[pair[1]].start,
                };
                let query = Window {
                    start: remaining,
                    end: target.end,
                };
                // `(int) Duration.toMinutes()`: truncated.
                let overlap = (span.overlap(query) / 60) as i32;
                let whole = ((span.end - span.start) / 60) as i32;
                add_trapezoid(
                    &mut result,
                    &a,
                    &b,
                    overlap,
                    whole,
                    span.start != remaining,
                    span.end != target.end,
                    strict,
                );
                remaining = span.end;
                first = Some(b);
            }
            let total = ((target.end - target.start) / 60) as i32;
            finish_trapezoid(&mut result, total, strict);
            Ok(result)
        }
    }
}

/// Windows from midnight of the first day (`BatchTimeStepResampler.process`), fixed length, while
/// `!currentEnd.isAfter(endTime)`. Returns the windows and the seconds left after the last.
pub fn resample_windows(
    series: &TemporalSeries,
    step_minutes: i64,
    limit: usize,
) -> Result<(Vec<Window>, i64), VortexError> {
    if !TIME_STEP_MINUTES.contains(&step_minutes) {
        return Err(refused(
            "time step",
            format!("{step_minutes} minutes is not one of Vortex's: {TIME_STEP_MINUTES:?}"),
        ));
    }
    let (Some(first), Some(last)) = (series.start_time(), series.end_time()) else {
        return Ok((Vec::new(), 0));
    };
    let step = step_minutes * 60;
    let mut start = first.div_euclid(DAY) * DAY;
    let mut windows = Vec::new();
    while start + step <= last {
        if windows.len() >= limit {
            return Err(refused(
                "time step",
                format!(
                    "it makes more than {limit} windows. Raise the limit, or use a longer step"
                ),
            ));
        }
        windows.try_reserve(1).map_err(|_| {
            VortexError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "no memory for the windows",
            ))
        })?;
        windows.push(Window {
            start,
            end: start + step,
        });
        start += step;
    }
    Ok((windows, last - start))
}

// ---- DSS files -----------------------------------------------------------------------------

enum Reader {
    V7(Box<Dss7File<File>>),
    V6(Box<Dss6File<FileSource>>),
}

fn decode_err(path: &Path, e: impl fmt::Display) -> VortexError {
    VortexError::Decode {
        format: "DSS",
        message: format!("{}: {e}", path.display()),
    }
}

impl Reader {
    fn open(path: &Path) -> Result<Reader, VortexError> {
        let head = read_head(path)?;
        let sig = FileSignature::from_bytes(&head).map_err(|e| {
            refused(
                format!("input {}", path.display()),
                format!("it is not a DSS file: {e}"),
            )
        })?;
        Ok(match sig.version {
            DssVersion::V7 => Reader::V7(Box::new(
                Dss7File::open(path).map_err(|e| decode_err(path, e))?,
            )),
            DssVersion::V6 => Reader::V6(Box::new(
                Dss6File::open(path).map_err(|e| decode_err(path, e))?,
            )),
        })
    }

    // Grid records only (`DssDataReader.getVariables`), as stored, sorted, once each.
    fn grid_pathnames(&mut self, path: &Path) -> Result<Vec<Vec<u8>>, VortexError> {
        let mut names: Vec<Vec<u8>> = match self {
            Reader::V7(f) => {
                let c = f.catalog().map_err(|e| decode_err(path, e))?;
                c.resolved_live()
                    .into_iter()
                    .map(|i| &c.catalog.entries[i])
                    .filter(|e| GridType::from_record_type(e.record_type).is_ok())
                    .map(|e| e.pathname.clone())
                    .collect()
            }
            Reader::V6(f) => {
                let c = f.catalog().map_err(|e| decode_err(path, e))?;
                c.entries
                    .iter()
                    .filter(|e| e.is_live() && GridType::from_record_type(e.record_type).is_ok())
                    .map(|e| e.pathname.clone())
                    .collect()
            }
        };
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn read(&mut self, path: &Path, pathname: &[u8]) -> Result<Grid, VortexError> {
        match self {
            Reader::V7(f) => f.read_grid(pathname),
            Reader::V6(f) => f.read_grid(pathname),
        }
        .map_err(|e| decode_err(path, format!("{}: {e}", String::from_utf8_lossy(pathname))))
    }
}

fn read_head(path: &Path) -> Result<Vec<u8>, VortexError> {
    use std::io::Read;
    let mut head = vec![0u8; dss_core::header::MIN_HEADER_BYTES];
    let mut file = File::open(path)?;
    let mut n = 0;
    while n < head.len() {
        let k = file.read(&mut head[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    head.truncate(n);
    Ok(head)
}

/// The six parts of a pathname, split on `/`.
fn split_parts(pathname: &[u8]) -> Result<[String; 6], VortexError> {
    let text = String::from_utf8_lossy(pathname);
    let parts: Vec<&str> = text.split('/').collect();
    if parts.len() != 8 || !parts[0].is_empty() || !parts[7].is_empty() {
        return Err(refused(
            format!("pathname {text}"),
            "it does not have six parts",
        ));
    }
    Ok([
        parts[1].trim().to_string(),
        parts[2].trim().to_string(),
        parts[3].trim().to_string(),
        parts[4].trim().to_string(),
        parts[5].trim().to_string(),
        parts[6].trim().to_string(),
    ])
}

/// A D or E part as whole seconds (`DssDataReader.dssToDto`). `:2400` is 0000 of the next day.
fn part_seconds(part: &str) -> Option<i64> {
    let t = grid_write::parse_grid_time(part.as_bytes())?;
    seconds_of(t).ok()
}

fn frame_time_of(pathname: &str, parts: &[String; 6]) -> Result<FrameTime, VortexError> {
    let (d, e) = (parts[3].as_str(), parts[4].as_str());
    if d.is_empty() {
        if e.is_empty() {
            return Ok(FrameTime::Invariant);
        }
        return Err(refused(
            format!("record {pathname}"),
            "it has an E part and no D part",
        ));
    }
    let bad = |which: &str, text: &str| {
        refused(
            format!("record {pathname}"),
            format!("its {which} part {text:?} is not a grid time"),
        )
    };
    let start = part_seconds(d).ok_or_else(|| bad("D", d))?;
    if e.is_empty() {
        return Ok(FrameTime::Instant(pair_of(start)));
    }
    let end = part_seconds(e).ok_or_else(|| bad("E", e))?;
    if end < start {
        return Err(refused(
            format!("record {pathname}"),
            "its E part is before its D part",
        ));
    }
    Ok(if end == start {
        FrameTime::Instant(pair_of(start))
    } else {
        FrameTime::Period {
            start: pair_of(start),
            end: pair_of(end),
        }
    })
}

fn window_of(time: &FrameTime) -> Option<Window> {
    match *time {
        FrameTime::Invariant => None,
        FrameTime::Instant(t) => {
            let s = seconds_of(t).ok()?;
            Some(Window { start: s, end: s })
        }
        FrameTime::Period { start, end } => Some(Window {
            start: seconds_of(start).ok()?,
            end: seconds_of(end).ok()?,
        }),
    }
}

fn data_type_of(code: i32, pathname: &str) -> Result<DataType, VortexError> {
    match code {
        c if c == DataType::PerAver.dss_code() => Ok(DataType::PerAver),
        c if c == DataType::PerCum.dss_code() => Ok(DataType::PerCum),
        c if c == DataType::InstVal.dss_code() => Ok(DataType::InstVal),
        c if c == DataType::InstCum.dss_code() => Ok(DataType::InstCum),
        c => Err(refused(
            format!("record {pathname}"),
            format!("data type code {c} has no rule"),
        )),
    }
}

/// What the output's CRS is: `WktFactory.fromGridInfo`, then `DssUtil.getGridInfo`.
fn sink_crs_of(grid: &Grid, pathname: &str) -> Result<SinkCrs, VortexError> {
    dss_source::cell_zero_is_the_crs_origin(grid, pathname)?;
    SinkCrs::of_frame(&dss_source::crs_of(grid, pathname)?)
}

/// `dssToDto` then `getGridInfo`, whose `float` products decide the cell indices of an unusual cell
/// size: `lowerLeftCellX * cellSize` and `direction * ny * cellSize` are `f32` in the Java.
fn placement_of(grid: &Grid) -> Result<sink_dss::Placement, VortexError> {
    sink_dss::placement(&dss_source::transform_of(grid), grid.ny, Align::Vortex)
}

/// The grid as `to_stored_cells` wants it: rows south to north, so nothing is flipped.
fn stored_parts(
    cells: Vec<f32>,
    grid: &Grid,
    time: FrameTime,
    data_type: DataType,
) -> GridFrameParts {
    let size = f64::from(grid.cell_size);
    GridFrameParts {
        cells,
        nx: grid.nx,
        ny: grid.ny,
        row_order: RowOrder::SouthToNorth,
        transform: GeoTransform::axis_aligned(0.0, size, 0.0, size),
        crs: Crs::Undefined,
        // `DssDataReader`'s `noDataValue` is always `UNDEFINED_FLOAT`.
        nodata: Some(GRID_NODATA),
        time,
        data_type,
        units: grid.data_units.clone(),
        variable: String::new(),
        description: None,
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map_or(String::new(), |n| n.to_string_lossy().into_owned())
}

/// What a run may hold in its plan.
#[derive(Debug, Clone, Copy)]
pub struct TemporalLimits {
    pub max_windows: usize,
    pub max_records: usize,
}

impl Default for TemporalLimits {
    fn default() -> Self {
        TemporalLimits {
            max_windows: 1 << 20,
            max_records: 1 << 20,
        }
    }
}

/// What both tools take.
#[derive(Debug, Clone)]
pub struct TemporalRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Full pathnames. `None` is every grid in the file. The resampler then takes the whole series
    /// of each (`condenseVariables`), so one pathname selects every record sharing its A, B, C and F.
    pub selection: Option<Vec<String>>,
    /// A, B and F of the output. A source's A and B are not kept, as in Vortex.
    /// `F` of `*` keeps the source's F (`DssDataWriter.write`).
    pub parts: Parts,
    pub c_part: Option<String>,
    pub units: Option<String>,
    pub data_type: Option<DataType>,
    pub version: Option<u8>,
    pub sync: bool,
    pub allow_undetectable_readers: bool,
    /// Crash simulation at this record of the run, for tests.
    #[doc(hidden)]
    pub fault: Option<(usize, FaultPoint)>,
    pub limits: TemporalLimits,
}

/// One record written.
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalRecord {
    /// The source record for the shifter, or the series for the resampler.
    pub source: String,
    pub source_type: DataType,
    pub every_cell_missing: bool,
    pub record: WrittenRecord,
}

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TemporalReport {
    pub records: Vec<TemporalRecord>,
    /// Selected series or records written nothing, and why, as Vortex skips them.
    pub skipped: Vec<(String, String)>,
    /// Seconds after the last window, summed over series: time a window would have overrun.
    pub dropped_tail_seconds: i64,
    /// Output records whose every cell is missing because the window begins before the series.
    pub windows_every_cell_missing: usize,
}

/// Why a run stopped, and what it had finished.
#[derive(Debug)]
pub struct TemporalError {
    pub failure: Failure,
    pub phase: &'static str,
    pub source: Option<String>,
    /// Records the writer acknowledged before the run stopped. They remain in the file. This is not
    /// every record the file may now hold: see `in_doubt`.
    pub completed: Vec<TemporalRecord>,
    /// The pathname being written when the writer failed, in the form planned. Its state is unknown:
    /// the writer leaves no torn record, but it may have published this one whole before it failed. An
    /// error is no proof it was not written. Read the file, or run again, which replaces it. `None` when
    /// no write of a record had begun.
    pub in_doubt: Option<String>,
}

impl fmt::Display for TemporalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "time tool stopped while {}", self.phase)?;
        if let Some(s) = &self.source {
            write!(f, " at {s}")?;
        }
        write!(
            f,
            ": {} ({} record(s) already written remain)",
            self.failure,
            self.completed.len()
        )?;
        if let Some(pathname) = &self.in_doubt {
            write!(
                f,
                "; {pathname} was being written, and may or may not be in the file: read the file, \
                 or run again, which replaces it"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for TemporalError {}

impl From<VortexError> for TemporalError {
    fn from(e: VortexError) -> Self {
        planning(Failure::Vortex(e))
    }
}

fn planning(failure: Failure) -> TemporalError {
    TemporalError {
        failure,
        phase: "planning",
        source: None,
        completed: Vec::new(),
        in_doubt: None,
    }
}

// Why a record was not written, and whether it may have been.
struct WriteStop {
    failure: Failure,
    in_doubt: bool,
}

impl From<Failure> for WriteStop {
    fn from(failure: Failure) -> Self {
        WriteStop {
            failure,
            in_doubt: false,
        }
    }
}

impl From<VortexError> for WriteStop {
    fn from(e: VortexError) -> Self {
        Failure::Vortex(e).into()
    }
}

fn check_lock(path: &Path, request: &TemporalRequest) -> Result<(), Failure> {
    let report = lock::lock_status(path).map_err(|e| Failure::Write(WriteError::Lock(e)))?;
    let policy = LockPolicy {
        allow_undetectable_readers: request.allow_undetectable_readers,
        ..LockPolicy::default()
    };
    lock::ensure_unlocked_for_write(&report, &policy)
        .map(|_| ())
        .map_err(|r| Failure::Write(WriteError::Locked(r)))
}

// Refuses before anything is touched. Returns the output's version and whether it exists.
fn plan_output(request: &TemporalRequest) -> Result<(u8, bool), TemporalError> {
    if !matches!(request.version, None | Some(6) | Some(7)) {
        return Err(planning(Failure::BadRequest(format!(
            "version must be 6 or 7, not {:?}",
            request.version
        ))));
    }
    // By the file itself, so another name for the input (a hard link, a symlink) is refused as well.
    // Only an existing output can be one; a check that cannot be made refuses too.
    let same = file_identity::same_file(&request.input, &request.output).map_err(|e| {
        refused(
            format!("output {}", request.output.display()),
            format!(
                "cannot tell whether it is the input {}: {e}",
                request.input.display()
            ),
        )
    })?;
    if same {
        return Err(refused(
            format!("output {}", request.output.display()),
            format!(
                "it is also the input {}. Observed behaviour we did not implement: Vortex writes \
                 into it, so a replaced record can be read again by the same run",
                request.input.display()
            ),
        )
        .into());
    }
    let existing = if request.output.exists() {
        let head = read_head(&request.output)?;
        let sig = FileSignature::from_bytes(&head).map_err(|e| {
            refused(
                format!("output {}", request.output.display()),
                format!("it exists and is not a DSS file: {e}"),
            )
        })?;
        Some(match sig.version {
            DssVersion::V6 => 6,
            DssVersion::V7 => 7,
        })
    } else {
        None
    };
    let version = match (existing, request.version) {
        (Some(have), Some(want)) if have != want => {
            return Err(refused(
                format!("output {}", request.output.display()),
                format!(
                    "it is DSS{have}, and version {want} was asked for. Writers keep a file's \
                     version"
                ),
            )
            .into())
        }
        (Some(have), _) => have,
        (None, Some(want)) => want,
        (None, None) => 7,
    };
    if existing.is_some() {
        check_lock(&request.output, request).map_err(planning)?;
    } else if let Some(parent) = request
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        if !parent.is_dir() {
            return Err(planning(Failure::BadRequest(format!(
                "output folder {} does not exist",
                parent.display()
            ))));
        }
    }
    Ok((version, existing.is_some()))
}

// What is noted of the input before planning. See the module for what this does not see.
#[derive(PartialEq)]
struct InputStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn input_stamp(input: &Path) -> Result<InputStamp, VortexError> {
    let m = std::fs::metadata(input)?;
    Ok(InputStamp {
        len: m.len(),
        modified: m.modified().ok(),
    })
}

fn input_unchanged(input: &Path, stamp: &InputStamp) -> Result<(), VortexError> {
    if input_stamp(input)? == *stamp {
        return Ok(());
    }
    Err(refused(
        format!("input {}", input.display()),
        "it has changed since planning, so what was planned may no longer be what it holds",
    ))
}

fn check_parts(request: &TemporalRequest) -> Result<(), VortexError> {
    for (name, value) in [("A", &request.parts.a), ("B", &request.parts.b)] {
        if value == "*" {
            return Err(refused(
                format!("part {name}"),
                "it cannot be '*'. Observed behaviour we did not implement: Vortex 1.1.0 and \
                 0.13.3 leave the part empty, and do not keep the source's",
            ));
        }
    }
    Ok(())
}

fn output_parts(request: &TemporalRequest, source_f: &str) -> Parts {
    let f = if request.parts.f == "*" {
        source_f.to_string()
    } else {
        request.parts.f.clone()
    };
    Parts {
        a: request.parts.a.clone(),
        b: request.parts.b.clone(),
        f,
    }
}

fn selected(request: &TemporalRequest, names: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, VortexError> {
    let chosen = match &request.selection {
        None => names,
        Some(wanted) => {
            let by_upper: HashMap<String, &Vec<u8>> = names
                .iter()
                .map(|n| (String::from_utf8_lossy(n).to_ascii_uppercase(), n))
                .collect();
            let mut out = Vec::with_capacity(wanted.len());
            for w in wanted {
                let found = by_upper
                    .get(&w.trim().to_ascii_uppercase())
                    .ok_or_else(|| {
                        refused(
                            format!("selection {w}"),
                            "it is not a grid record in the input. Observed behaviour we did not \
                         implement: Vortex 1.1.0 logs it and goes on",
                        )
                    })?;
                out.push((*found).clone());
            }
            out.sort();
            out.dedup();
            out
        }
    };
    if chosen.is_empty() {
        return Err(refused(
            format!("input {}", request.input.display()),
            "the selection holds no grid record, so there is nothing to do",
        ));
    }
    if chosen.len() > request.limits.max_records {
        return Err(refused(
            format!("input {}", request.input.display()),
            format!(
                "{} records selected, over the limit of {}",
                chosen.len(),
                request.limits.max_records
            ),
        ));
    }
    Ok(chosen)
}

// One record to write.
struct Item {
    source: Vec<u8>,
    source_text: String,
    source_type: DataType,
    time: FrameTime,
    planned: PlannedRecord,
    crs: SinkCrs,
    place: sink_dss::Placement,
    window: Option<(usize, Window)>,
}

// Planned pathnames must be new to this run, and pass the writer's own checks.
fn check_planned(items: &[Item], version: u8) -> Result<(), VortexError> {
    let mut seen: HashMap<Vec<u8>, &str> = HashMap::new();
    for item in items {
        let stored = sink_dss::check_record(&item.planned, &item.crs, version).map_err(|e| {
            refused(
                format!("record {}", item.planned.pathname),
                format!("the writer would refuse it: {e}"),
            )
        })?;
        let key = stored.to_ascii_uppercase();
        if let Some(other) = seen.insert(key, &item.source_text) {
            return Err(refused(
                format!("record {}", item.planned.pathname),
                format!(
                    "both {other} and {} would be written there. Observed behaviour we did not \
                     implement: Vortex 1.1.0 keeps whichever it wrote last. State other parts, or \
                     select one",
                    item.source_text
                ),
            ));
        }
    }
    Ok(())
}

fn meaning_of(
    c_part: &str,
    file_name: &str,
    units: &str,
    time: &FrameTime,
    source_type: DataType,
    request: &TemporalRequest,
) -> Result<units::RecordMeaning, VortexError> {
    // A source that says INST-VAL has stated its type, which the instant-precipitation rule asks for.
    let data_type = match (request.data_type, time, source_type) {
        (Some(t), _, _) => Some(t),
        (None, FrameTime::Instant(_), DataType::InstVal) => Some(DataType::InstVal),
        _ => None,
    };
    let overrides = Overrides {
        c_part: request.c_part.as_deref(),
        units: request.units.as_deref(),
        data_type,
    };
    units::record_meaning(c_part, Some(c_part), file_name, units, time, &overrides)
}

fn sink_options(request: &TemporalRequest, version: u8, index: usize) -> SinkOptions {
    SinkOptions {
        version,
        sync: request.sync,
        allow_undetectable_readers: request.allow_undetectable_readers,
        fault: request
            .fault
            .and_then(|(at, point)| (at == index).then_some(point)),
    }
}

/// Called after each record with (done, total). `Break` stops the run; finished records remain.
pub type TemporalProgress<'a> = &'a mut dyn FnMut(usize, usize) -> ControlFlow<()>;

/// The time shifter (`TimeShifter.shift`): every selected record's time moved, its cells unchanged
/// except for what `DssDataWriter` always does (unit conversion and the missing marker).
///
/// The whole run is planned first, reading every selected record once, so a refusal leaves the
/// output untouched and does not create it.
pub fn shift_times(
    request: &TemporalRequest,
    shift_start: i64,
    shift_end: i64,
    progress: TemporalProgress,
) -> Result<TemporalReport, TemporalError> {
    check_parts(request)?;
    let (version, exists) = plan_output(request)?;
    let input = &request.input;
    let stamp = input_stamp(input)?;
    let file_name = file_name_of(input);
    let mut reader = Reader::open(input)?;
    let names = reader.grid_pathnames(input)?;
    let chosen = selected(request, names)?;
    let mut items = Vec::with_capacity(chosen.len());
    for name in &chosen {
        let text = String::from_utf8_lossy(name).into_owned();
        let at = |e: VortexError| TemporalError {
            source: Some(text.clone()),
            ..planning(Failure::Vortex(e))
        };
        let mut grid = reader.read(input, name).map_err(at)?;
        if holds_its_own_null(&grid) {
            return Err(at(own_null_refusal(
                name,
                &grid,
                "the output would hold a number in its place",
            )));
        }
        let parts = split_parts(name).map_err(at)?;
        let source_time = frame_time_of(&text, &parts).map_err(at)?;
        let source_type = data_type_of(grid.data_type, &text).map_err(at)?;
        let time = shift_time(&source_time, shift_start, shift_end).map_err(at)?;
        let meaning = meaning_of(
            &parts[2],
            &file_name,
            &grid.data_units,
            &time,
            source_type,
            request,
        )
        .map_err(at)?;
        let derived = meaning.c_part.clone();
        let planned = sink_dss::plan_record(
            &output_parts(request, &parts[5]),
            meaning,
            &derived,
            Some(&parts[2]),
            &time,
        )
        .map_err(at)?;
        let crs = sink_crs_of(&grid, &text).map_err(at)?;
        let place = placement_of(&grid).map_err(at)?;
        let item = Item {
            source: name.clone(),
            source_text: text.clone(),
            source_type,
            time,
            planned,
            crs,
            place,
            window: None,
        };
        // Every record is checked as it would be stored, so none is written if one cannot be.
        let cells = std::mem::take(&mut grid.data);
        stored_cells(&item, &grid, cells).map_err(at)?;
        items.push(item);
    }
    check_planned(&items, version)?;
    input_unchanged(input, &stamp)?;
    let mut report = TemporalReport::default();
    let mut created = exists;
    let total = items.len();
    for (done, item) in items.iter().enumerate() {
        let written = input_unchanged(input, &stamp)
            .map_err(WriteStop::from)
            .and_then(|()| write_one(request, item, version, done, &mut created, &mut reader));
        finish_one(written, item, false, &mut report)?;
        if progress(done + 1, total).is_break() {
            return Err(stop(item, report));
        }
    }
    Ok(report)
}

fn stop(item: &Item, report: TemporalReport) -> TemporalError {
    TemporalError {
        failure: Failure::Cancelled,
        phase: "writing",
        source: Some(item.source_text.clone()),
        completed: report.records,
        in_doubt: None,
    }
}

fn finish_one(
    written: Result<(WrittenRecord, bool), WriteStop>,
    item: &Item,
    resampled: bool,
    report: &mut TemporalReport,
) -> Result<(), TemporalError> {
    match written {
        Ok((record, all_missing)) => {
            if resampled && all_missing {
                report.windows_every_cell_missing += 1;
            }
            report.records.push(TemporalRecord {
                source: item.source_text.clone(),
                source_type: item.source_type,
                every_cell_missing: all_missing,
                record,
            });
            Ok(())
        }
        Err(stop) => Err(TemporalError {
            failure: stop.failure,
            phase: "writing",
            source: Some(item.source_text.clone()),
            completed: std::mem::take(&mut report.records),
            in_doubt: stop.in_doubt.then(|| item.planned.pathname.clone()),
        }),
    }
}

fn write_one(
    request: &TemporalRequest,
    item: &Item,
    version: u8,
    index: usize,
    created: &mut bool,
    reader: &mut Reader,
) -> Result<(WrittenRecord, bool), WriteStop> {
    let mut grid = reader.read(&request.input, &item.source)?;
    // The plan decided this record's placement. An input that now states another must not reach it.
    let place = placement_of(&grid)?;
    let crs = sink_crs_of(&grid, &item.source_text)?;
    if place != item.place || crs != item.crs {
        return Err(refused(
            format!("record {}", item.source_text),
            "the input's grid has changed since planning",
        )
        .into());
    }
    let cells = std::mem::take(&mut grid.data);
    store(request, item, &grid, cells, version, index, created)
}

/// A record's cells as they would be stored, or a refusal naming the record and its time. This is the
/// check for -901, -902 and an out-of-range conversion (`sink_dss::to_stored_cells`). It runs on every
/// record while planning.
fn stored_cells(item: &Item, grid: &Grid, cells: Vec<f32>) -> Result<GridFrameParts, VortexError> {
    let mut parts = stored_parts(cells, grid, item.time, item.planned.meaning.data_type);
    sink_dss::to_stored_cells(&mut parts, item.planned.meaning.conversion).map_err(|e| {
        let when = window_of(&item.time).map_or("no time".to_string(), window_text);
        refused(
            format!(
                "record {} ({when}, from {})",
                item.planned.pathname, item.source_text
            ),
            e.to_string(),
        )
    })?;
    Ok(parts)
}

#[allow(clippy::too_many_arguments)]
fn store(
    request: &TemporalRequest,
    item: &Item,
    grid: &Grid,
    cells: Vec<f32>,
    version: u8,
    index: usize,
    created: &mut bool,
) -> Result<(WrittenRecord, bool), WriteStop> {
    let parts = stored_cells(item, grid, cells)?;
    let all_missing = parts.cells.iter().all(|&c| c == GRID_NODATA);
    if !*created {
        // No record's write has begun, so none is in doubt.
        import::create_output(&request.output, version).map_err(Failure::Write)?;
        *created = true;
    }
    let written = sink_dss::write_record(
        &request.output,
        &item.planned,
        &item.crs,
        item.place,
        &parts.cells,
        grid.nx,
        grid.ny,
        &sink_options(request, version, index),
    )
    .map_err(|e| WriteStop {
        failure: Failure::Write(e),
        in_doubt: true,
    })?;
    Ok((written, all_missing))
}

/// Whether a specified grid holds its own null value, as this engine's reader takes one
/// (`Grid::is_missing`), and that value is not HEC's marker.
fn holds_its_own_null(grid: &Grid) -> bool {
    grid.grid_type == GridType::Specified
        && grid.null_value != GRID_NODATA
        && grid.data.contains(&grid.null_value)
}

fn own_null_refusal(name: &[u8], grid: &Grid, how: &str) -> VortexError {
    refused(
        format!("record {}", String::from_utf8_lossy(name)),
        format!(
            "a cell holds this specified grid's null value {}, which this library's reader takes as \
             missing, and {how}. Observed behaviour we did not implement: Vortex 1.1.0 and 0.13.3 use \
             it as a number",
            grid.null_value
        ),
    )
}

/// A source's cells as `rule` lets a window use them. For -901 and -902, see the module.
fn usable_source(name: &[u8], grid: Grid, rule: Rule) -> Result<Vec<f32>, VortexError> {
    if rule == Rule::Strict {
        if holds_its_own_null(&grid) {
            return Err(own_null_refusal(
                name,
                &grid,
                "a window cannot use it as a number. Ask for rule=\"vortex\"",
            ));
        }
        if let Some(v) = grid
            .data
            .iter()
            .find(|&&c| c != GRID_NODATA && sink_dss::reader_takes_as_missing(c))
        {
            return Err(refused(
                format!("record {}", String::from_utf8_lossy(name)),
                format!(
                    "a cell holds {v}, which this library's reader takes as missing, so a window \
                     cannot use it as a number or as missing without a guess. Observed behaviour we \
                     did not implement: Vortex 1.1.0 and 0.13.3 use it as a number. Ask for \
                     rule=\"vortex\""
                ),
            ));
        }
    }
    Ok(grid.data)
}

// One window's cells, reading each source through the checks above.
fn window_cells(
    loaded: &DssSeries,
    plan: &WindowPlan,
    target: Window,
    rule: Rule,
    reader: &mut Reader,
    input: &Path,
) -> Result<Vec<f32>, VortexError> {
    let series = series_of(loaded)?;
    let n = grid_cells(&loaded.base)?;
    let cells = compute_window(series, plan, target, n, rule, &mut |i| {
        let name = loaded
            .records
            .get(i)
            .ok_or_else(|| refused(&loaded.key, format!("no record at index {i}")))?;
        let grid = reader.read(input, name)?;
        if shape_of(&grid) != shape_of(&loaded.base) {
            return Err(refused(
                format!("record {}", String::from_utf8_lossy(name)),
                "the input's grid has changed since planning",
            ));
        }
        usable_source(name, grid, rule)
    })?;
    if rule == Rule::Strict && cells.iter().any(|c| c.is_infinite()) {
        return Err(refused(
            format!("window {} of {}", window_text(target), loaded.key),
            "a cell comes to an infinity, and the writer would store that as missing",
        ));
    }
    Ok(cells)
}

// What must agree across a series: everything Vortex takes from its first record.
#[derive(PartialEq)]
struct SeriesShape {
    grid_type: GridType,
    nx: usize,
    ny: usize,
    cell_bits: u32,
    lower_left: (i32, i32),
    cell_zero: (u32, u32),
    null_value_bits: u32,
    srs_definition: String,
    albers: Option<AlbersProjection>,
    units: String,
    data_type: i32,
}

fn shape_of(g: &Grid) -> SeriesShape {
    SeriesShape {
        grid_type: g.grid_type,
        nx: g.nx,
        ny: g.ny,
        cell_bits: g.cell_size.to_bits(),
        lower_left: (g.lower_left_cell_x, g.lower_left_cell_y),
        cell_zero: (
            g.x_coord_of_grid_cell_zero.to_bits(),
            g.y_coord_of_grid_cell_zero.to_bits(),
        ),
        null_value_bits: g.null_value.to_bits(),
        srs_definition: g.srs_definition.clone(),
        albers: g.albers.clone(),
        units: g.data_units.clone(),
        data_type: g.data_type,
    }
}

/// One series read from a file: its index, its records' pathnames, and one grid's metadata.
pub struct DssSeries {
    /// `/A/B/C/*/*/F/`, as `condenseVariables` writes it.
    pub key: String,
    pub series: Option<TemporalSeries>,
    /// Pathnames, in the order of `series.windows()`.
    pub records: Vec<Vec<u8>>,
    pub data_type: DataType,
    pub f_part: String,
    pub c_part: String,
    base: Grid,
}

fn series_key(parts: &[String; 6]) -> String {
    format!("/{}/{}/{}/*/*/{}/", parts[0], parts[1], parts[2], parts[5])
}

// Reads every record of a series once, keeping none of its cells.
fn load_series(
    reader: &mut Reader,
    input: &Path,
    key: &str,
    names: &[Vec<u8>],
) -> Result<DssSeries, VortexError> {
    let mut timed: Vec<(Window, Vec<u8>)> = Vec::with_capacity(names.len());
    let mut untimed = 0usize;
    let mut base: Option<(Grid, SeriesShape)> = None;
    let mut parts_of = None;
    for name in names {
        let text = String::from_utf8_lossy(name).into_owned();
        let parts = split_parts(name)?;
        let time = frame_time_of(&text, &parts)?;
        let mut grid = reader.read(input, name)?;
        let shape = shape_of(&grid);
        match &base {
            None => {
                grid.data = Vec::new();
                base = Some((grid, shape));
            }
            Some((_, first)) if *first != shape => {
                return Err(refused(
                    format!("series {key}"),
                    format!(
                        "record {text} differs from the first in its grid, CRS, units or data \
                         type. Observed behaviour we did not implement: Vortex 1.1.0 and 0.13.3 \
                         take all of these from the catalog's first record"
                    ),
                ))
            }
            Some(_) => {}
        }
        match window_of(&time) {
            Some(w) => timed.push((w, name.clone())),
            None => untimed += 1,
        }
        parts_of = Some(parts);
    }
    let (Some((base, _)), Some(parts)) = (base, parts_of) else {
        return Err(refused(format!("series {key}"), "it holds no record"));
    };
    let key_text = key.to_string();
    let data_type = data_type_of(base.data_type, &key_text)?;
    // `getCatalog(sorted)`: by time. `findMaximum` and ties at one start depend on it.
    timed.sort_by_key(|(w, _)| (w.start, w.end));
    if untimed > 0 && !timed.is_empty() {
        return Err(refused(
            format!("series {key}"),
            format!(
                "{untimed} of its records hold no time and {} do. Observed behaviour we did not \
                 implement: Vortex 1.1.0 and 0.13.3 take the type and grid from the catalog's first \
                 record",
                timed.len()
            ),
        ));
    }
    let series = match timed.first() {
        Some((first, _)) => Some(TemporalSeries::new(
            SeriesKind::of(data_type, &parts[2], *first),
            timed.iter().map(|t| t.0).collect(),
        )?),
        None => None,
    };
    Ok(DssSeries {
        key: key_text,
        series,
        records: timed.into_iter().map(|t| t.1).collect(),
        data_type,
        f_part: parts[5].clone(),
        c_part: parts[2].clone(),
        base,
    })
}

// Groups by A, B, C and F, and refuses two that differ only by case: `getCatalog`'s wildcard would
// then read both as one series.
fn group_series(
    chosen: &[Vec<u8>],
    all: &[Vec<u8>],
) -> Result<BTreeMap<String, Vec<Vec<u8>>>, VortexError> {
    let mut wanted: HashSet<String> = HashSet::new();
    for name in chosen {
        wanted.insert(series_key(&split_parts(name)?));
    }
    let mut groups: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    let mut upper: HashMap<String, String> = HashMap::new();
    for name in all {
        let Ok(parts) = split_parts(name) else {
            continue;
        };
        let key = series_key(&parts);
        if !wanted.contains(&key) {
            continue;
        }
        if let Some(other) = upper.insert(key.to_ascii_uppercase(), key.clone()) {
            if other != key {
                return Err(refused(
                    format!("series {key}"),
                    format!("it differs from {other} only by case, and DSS pathnames do not"),
                ));
            }
        }
        groups.entry(key).or_default().push(name.clone());
    }
    Ok(groups)
}

/// The time-step resampler (`BatchTimeStepResampler.process`), with its overlap rules. See the module
/// for what `rule` changes.
pub fn resample_times(
    request: &TemporalRequest,
    step_minutes: i64,
    rule: Rule,
    progress: TemporalProgress,
) -> Result<TemporalReport, TemporalError> {
    check_parts(request)?;
    let (version, exists) = plan_output(request)?;
    let input = &request.input;
    let stamp = input_stamp(input)?;
    let file_name = file_name_of(input);
    let mut reader = Reader::open(input)?;
    let all = reader.grid_pathnames(input)?;
    let chosen = selected(request, all.clone())?;
    let groups = group_series(&chosen, &all)?;
    let mut report = TemporalReport::default();
    let mut work: Vec<(DssSeries, Vec<WindowPlan>)> = Vec::with_capacity(groups.len());
    let mut items = Vec::new();
    for (key, names) in &groups {
        let at = |e: VortexError| TemporalError {
            source: Some(key.clone()),
            ..planning(Failure::Vortex(e))
        };
        let loaded = load_series(&mut reader, input, key, names).map_err(at)?;
        let Some(series) = loaded.series.as_ref() else {
            // Vortex: `Could not determine time range for variable`, and on to the next.
            report
                .skipped
                .push((key.clone(), "its records hold no time".to_string()));
            continue;
        };
        let (windows, tail) =
            resample_windows(series, step_minutes, request.limits.max_windows).map_err(at)?;
        report.dropped_tail_seconds += tail;
        let crs = sink_crs_of(&loaded.base, key).map_err(at)?;
        let place = placement_of(&loaded.base).map_err(at)?;
        let index = work.len();
        let mut plans = Vec::with_capacity(windows.len());
        for w in windows {
            let plan = series.plan(w);
            if rule == Rule::Strict {
                if let WindowPlan::Weighted { gap_seconds, .. } = plan {
                    if gap_seconds > 0 {
                        return Err(at(refused(
                            format!("window {}", window_text(w)),
                            format!(
                                "{gap_seconds} s of it has no source, so it would hold a partial \
                                 {}. Observed behaviour we did not implement: Vortex 1.1.0 and \
                                 0.13.3 write it. Ask for rule=\"vortex\"",
                                if series.kind() == SeriesKind::Accumulation {
                                    "volume"
                                } else {
                                    "average"
                                }
                            ),
                        )));
                    }
                }
            }
            let time = FrameTime::Period {
                start: pair_of(w.start),
                end: pair_of(w.end),
            };
            let meaning = meaning_of(
                &loaded.c_part,
                &file_name,
                &loaded.base.data_units,
                &time,
                loaded.data_type,
                request,
            )
            .map_err(at)?;
            let derived = meaning.c_part.clone();
            let planned = sink_dss::plan_record(
                &output_parts(request, &loaded.f_part),
                meaning,
                &derived,
                Some(&loaded.c_part),
                &time,
            )
            .map_err(at)?;
            items.push(Item {
                source: Vec::new(),
                source_text: key.clone(),
                source_type: loaded.data_type,
                time,
                planned,
                crs: crs.clone(),
                place,
                window: Some((index, w)),
            });
            plans.push(plan);
        }
        work.push((loaded, plans));
    }
    if items.is_empty() {
        return Err(refused(
            format!("input {}", input.display()),
            format!(
                "no complete window of {step_minutes} minutes, so there is nothing to write ({} \
                 series skipped). Observed behaviour we did not implement: Vortex 1.1.0 and 0.13.3 \
                 finish with no output and no message",
                report.skipped.len()
            ),
        )
        .into());
    }
    check_planned(&items, version)?;
    // Every window is computed before the first write, so no record is written if one cannot be stored.
    let mut per_series = vec![0usize; work.len()];
    for item in &items {
        let Some((index, target)) = item.window else {
            continue;
        };
        let (loaded, plans) = &work[index];
        let plan = &plans[per_series[index]];
        per_series[index] += 1;
        let checked = window_cells(loaded, plan, target, rule, &mut reader, input)
            .and_then(|cells| stored_cells(item, &loaded.base, cells));
        if let Err(e) = checked {
            return Err(TemporalError {
                source: Some(item.source_text.clone()),
                ..planning(Failure::Vortex(e))
            });
        }
    }
    input_unchanged(input, &stamp)?;
    let mut created = exists;
    let total = items.len();
    let mut per_series = vec![0usize; work.len()];
    for (done, item) in items.iter().enumerate() {
        let Some((index, target)) = item.window else {
            continue;
        };
        let (loaded, plans) = &work[index];
        let plan = &plans[per_series[index]];
        per_series[index] += 1;
        let written = input_unchanged(input, &stamp)
            .and_then(|()| window_cells(loaded, plan, target, rule, &mut reader, input))
            .map_err(WriteStop::from)
            .and_then(|cells| {
                store(
                    request,
                    item,
                    &loaded.base,
                    cells,
                    version,
                    done,
                    &mut created,
                )
            });
        finish_one(written, item, true, &mut report)?;
        if progress(done + 1, total).is_break() {
            return Err(stop(item, report));
        }
    }
    Ok(report)
}

fn window_text(w: Window) -> String {
    let text = |t: i64| {
        let (j, s) = pair_of(t);
        let (y, m, d) = dss_core::time::julian_to_ymd(j);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}", s / 3600, s % 3600 / 60)
    };
    format!("{} to {}", text(w.start), text(w.end))
}

/// What [`read_window`], [`read_nearest`] and [`read_min_max`] return. Cells run south to north, as a
/// DSS grid stores them, with stored bits: `-FLT_MAX` marks missing, and no value is mapped.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowRead {
    pub cells: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
    pub start: i64,
    pub end: i64,
    pub data_type: DataType,
    pub units: String,
    pub cell_size: f32,
    pub lower_left: (i32, i32),
}

/// What a series states, before any window is read (`getStartTime`, `getEndTime`, `getDataUnits`,
/// `getGridDefinition`).
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesInfo {
    pub key: String,
    pub records: usize,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub data_type: DataType,
    pub units: String,
    pub nx: usize,
    pub ny: usize,
    pub cell_size: f32,
    pub lower_left: (i32, i32),
    pub srs_definition: String,
}

// The series holding a pathname, or matching `/A/B/C/*/*/F/`.
fn open_series(input: &Path, pathname: &str) -> Result<(Reader, DssSeries), VortexError> {
    let mut reader = Reader::open(input)?;
    let all = reader.grid_pathnames(input)?;
    let wanted = series_key(&split_parts(pathname.trim().as_bytes())?);
    let names: Vec<Vec<u8>> = all
        .into_iter()
        .filter(|n| {
            split_parts(n)
                .map(|p| series_key(&p).eq_ignore_ascii_case(&wanted))
                .unwrap_or(false)
        })
        .collect();
    if names.is_empty() {
        return Err(refused(
            format!("series {wanted}"),
            "no grid record in the input belongs to it",
        ));
    }
    let loaded = load_series(&mut reader, input, &wanted, &names)?;
    Ok((reader, loaded))
}

fn series_of(loaded: &DssSeries) -> Result<&TemporalSeries, VortexError> {
    loaded
        .series
        .as_ref()
        .ok_or_else(|| refused(format!("series {}", loaded.key), "its records hold no time"))
}

fn build_read(loaded: &DssSeries, cells: Vec<f32>, w: Window) -> WindowRead {
    let b = &loaded.base;
    WindowRead {
        cells,
        nx: b.nx,
        ny: b.ny,
        start: w.start,
        end: w.end,
        data_type: loaded.data_type,
        units: b.data_units.clone(),
        cell_size: b.cell_size,
        lower_left: (b.lower_left_cell_x, b.lower_left_cell_y),
    }
}

fn grid_cells(b: &Grid) -> Result<usize, VortexError> {
    b.nx.checked_mul(b.ny)
        .ok_or_else(|| refused("grid", "its size overflows"))
}

/// What a series states.
pub fn series_info(input: &Path, pathname: &str) -> Result<SeriesInfo, VortexError> {
    let (_, loaded) = open_series(input, pathname)?;
    let b = &loaded.base;
    Ok(SeriesInfo {
        key: loaded.key.clone(),
        records: loaded.records.len(),
        start: loaded.series.as_ref().and_then(TemporalSeries::start_time),
        end: loaded.series.as_ref().and_then(TemporalSeries::end_time),
        data_type: loaded.data_type,
        units: b.data_units.clone(),
        nx: b.nx,
        ny: b.ny,
        cell_size: b.cell_size,
        lower_left: (b.lower_left_cell_x, b.lower_left_cell_y),
        srs_definition: b.srs_definition.clone(),
    })
}

/// `TemporalDataReader.read`: one window of a series, unconverted, as there. `start == end` asks for
/// an instant. Units stay as the record states them: `DataConverter` belongs to the writer.
pub fn read_window(
    input: &Path,
    pathname: &str,
    start: i64,
    end: i64,
    rule: Rule,
) -> Result<WindowRead, VortexError> {
    let (mut reader, loaded) = open_series(input, pathname)?;
    let series = series_of(&loaded)?;
    if end < start {
        return Err(refused("window", "it ends before it starts"));
    }
    let target = Window { start, end };
    if rule == Rule::Strict && start == end && series.kind() != SeriesKind::Instantaneous {
        return Err(refused(
            format!("instant {}", window_text(target)),
            "a period series has no value at an instant. Observed behaviour we did not implement: \
             Vortex 1.1.0 and 0.13.3 give zeros for PER-CUM and NaN for PER-AVER. Ask for \
             rule=\"vortex\"",
        ));
    }
    let plan = series.plan(target);
    if rule == Rule::Strict {
        if let WindowPlan::Weighted { gap_seconds, .. } = plan {
            if gap_seconds > 0 {
                return Err(refused(
                    format!("window {}", window_text(target)),
                    format!(
                        "{gap_seconds} s of it has no source. Observed behaviour we did not \
                         implement: Vortex 1.1.0 and 0.13.3 return what the others hold. Ask for \
                         rule=\"vortex\""
                    ),
                ));
            }
        }
    }
    let cells = window_cells(&loaded, &plan, target, rule, &mut reader, input)?;
    Ok(build_read(&loaded, cells, target))
}

/// `TemporalDataReader.readNearest`: one record's cells, with that record's own window.
pub fn read_nearest(input: &Path, pathname: &str, time: i64) -> Result<WindowRead, VortexError> {
    let (mut reader, loaded) = open_series(input, pathname)?;
    let series = series_of(&loaded)?;
    let i = series
        .nearest(time)
        .ok_or_else(|| refused(&loaded.key, "the series holds no timed record"))?;
    let name = loaded
        .records
        .get(i)
        .ok_or_else(|| refused(&loaded.key, format!("no record at index {i}")))?;
    let cells = reader.read(input, name)?.data;
    Ok(build_read(&loaded, cells, series.windows()[i]))
}

/// `TemporalDataReader.getMinMaxGridData`: the (minimum, maximum) grids over a window.
pub fn read_min_max(
    input: &Path,
    pathname: &str,
    start: i64,
    end: i64,
) -> Result<(WindowRead, WindowRead), VortexError> {
    let (mut reader, loaded) = open_series(input, pathname)?;
    let series = series_of(&loaded)?;
    let target = Window { start, end };
    let sources = series.min_max_sources(target).ok_or_else(|| {
        refused(
            format!("window {}", window_text(target)),
            "no instant lies in it. Observed behaviour we did not implement: Vortex 1.1.0 and \
             0.13.3 raise IndexOutOfBoundsException",
        )
    })?;
    let n = grid_cells(&loaded.base)?;
    if sources.is_empty() {
        // `buildMinMaxGrids`, then `buildGrid`'s fill.
        let empty = filled(n, GRID_NODATA)?;
        return Ok((
            build_read(&loaded, empty.clone(), target),
            build_read(&loaded, empty, target),
        ));
    }
    let mut min = filled(n, f32::MAX)?;
    let mut max = filled(n, -f32::MAX)?;
    for i in sources {
        let name = loaded
            .records
            .get(i)
            .ok_or_else(|| refused(&loaded.key, format!("no record at index {i}")))?;
        let cells = reader.read(input, name)?.data;
        if cells.len() != n {
            return Err(refused(&loaded.key, "a record's cell count differs"));
        }
        min_max_step(&mut min, &mut max, &cells);
    }
    Ok((
        build_read(&loaded, min, target),
        build_read(&loaded, max, target),
    ))
}

/// `(julian, seconds)` from a D or E part, or `None`. `01JAN2024:2400` and `02JAN2024:0000` agree.
pub fn parse_stamp(text: &str) -> Option<i64> {
    part_seconds(text.trim())
}

/// A window's D and E parts for a data type, as a record would store them.
pub fn stamps_of(w: Window, data_type: DataType) -> Result<(String, String), VortexError> {
    let time = if w.start == w.end {
        FrameTime::Instant(pair_of(w.start))
    } else {
        FrameTime::Period {
            start: pair_of(w.start),
            end: pair_of(w.end),
        }
    };
    sink_dss::time_parts(&time, data_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;
    const BASE: i64 = 45_000 * DAY;

    fn hour(start: i64, end: i64) -> Window {
        Window {
            start: BASE + start * H,
            end: BASE + end * H,
        }
    }

    fn series(kind: SeriesKind, spans: &[(i64, i64)]) -> TemporalSeries {
        TemporalSeries::new(kind, spans.iter().map(|&(a, b)| hour(a, b)).collect()).unwrap()
    }

    // TemporalDataReaderTest's data.
    fn accumulation() -> (TemporalSeries, Vec<Vec<f32>>) {
        let s = series(
            SeriesKind::Accumulation,
            &[(1, 2), (2, 3), (3, 4), (4, 5), (5, 6)],
        );
        let v = [2.0, 1.5, 0.0, 3.0, 2.5];
        (s, v.iter().map(|&x| vec![x; 9]).collect())
    }

    fn run(s: &TemporalSeries, cells: &[Vec<f32>], w: Window, rule: Rule) -> Vec<f32> {
        let plan = s.plan(w);
        compute_window(s, &plan, w, cells[0].len(), rule, &mut |i| {
            Ok(cells[i].clone())
        })
        .unwrap()
    }

    #[test]
    fn temporal_data_reader_test_accumulation_values() {
        let (s, cells) = accumulation();
        for (w, want) in [
            (hour(2, 3), 1.5f32),
            (hour(1, 6), 9.0),
            (
                Window {
                    start: BASE + 3 * H / 2,
                    end: BASE + 3 * H,
                },
                2.5,
            ),
            (
                Window {
                    start: BASE + 5 * H / 4,
                    end: BASE + 13 * H / 4,
                },
                3.0,
            ),
        ] {
            assert_eq!(run(&s, &cells, w, Rule::Strict), vec![want; 9], "{w:?}");
        }
        for w in [
            Window {
                start: BASE + H / 4,
                end: BASE + 3 * H,
            },
            Window {
                start: BASE + 9 * H / 2,
                end: BASE + 25 * H / 4,
            },
        ] {
            assert_eq!(s.plan(w), WindowPlan::Missing, "{w:?}");
            assert_eq!(run(&s, &cells, w, Rule::Strict), vec![GRID_NODATA; 9]);
        }
    }

    #[test]
    fn accumulation_keeps_volume_over_complete_windows() {
        // Dyadic values and dyadic shares, so `f32` can hold every sum: equality with no tolerance.
        let spans: Vec<(i64, i64)> = (0..24).map(|h| (h, h + 1)).collect();
        let s = series(SeriesKind::Accumulation, &spans);
        let cells: Vec<Vec<f32>> = (0..24)
            .map(|h| vec![h as f32 / 8.0, 2.0 * h as f32 + 0.25, 0.0, 1.0])
            .collect();
        let total: Vec<f32> = (0..4)
            .map(|c| cells.iter().map(|g| g[c]).sum::<f32>())
            .collect();
        for step in [1i64, 2, 3, 4, 6, 8, 12, 24] {
            let mut sum = vec![0.0f32; 4];
            let mut h = 0;
            while h < 24 {
                let out = run(&s, &cells, hour(h, h + step), Rule::Strict);
                for (a, b) in sum.iter_mut().zip(&out) {
                    *a += *b;
                }
                h += step;
            }
            assert_eq!(sum, total, "{step} h");
        }
        // Splitting: 15-minute windows of one hour's depth.
        let quarter = Window {
            start: BASE + 3 * H,
            end: BASE + 3 * H + 900,
        };
        let out = run(&s, &cells, quarter, Rule::Strict);
        assert_eq!(out[1], cells[3][1] / 4.0);
    }

    #[test]
    fn a_window_with_a_gap_is_seen_and_holds_only_what_is_present() {
        let s = series(SeriesKind::Accumulation, &[(0, 1), (2, 3)]);
        let cells = vec![vec![1.0f32], vec![2.0f32]];
        let plan = s.plan(hour(0, 3));
        let WindowPlan::Weighted { gap_seconds, .. } = plan else {
            panic!("{plan:?}");
        };
        assert_eq!(gap_seconds, H);
        // Vortex's number, which holds no depth for the middle hour.
        assert_eq!(run(&s, &cells, hour(0, 3), Rule::Vortex), vec![3.0]);
    }

    #[test]
    fn the_covered_window_can_hide_a_gap() {
        // [5,6) ends first and is taken. [0,7) overlaps it and is dropped, so [0,5) is uncovered, and
        // `first.start > target.start` then gives every cell missing.
        let s = series(SeriesKind::Accumulation, &[(5, 6), (0, 7)]);
        assert_eq!(s.plan(hour(0, 6)), WindowPlan::Missing);
        assert!(matches!(
            s.plan(hour(5, 6)),
            WindowPlan::Weighted { gap_seconds: 0, .. }
        ));
    }

    #[test]
    fn shorter_sources_win_and_later_ends_follow() {
        let s = series(SeriesKind::Accumulation, &[(0, 6), (0, 1), (1, 2), (2, 3)]);
        let sources = s.plan(hour(0, 3)).sources();
        assert_eq!(sources, vec![1, 2, 3], "the hours, not the six-hour record");
    }

    #[test]
    fn missing_is_never_zero_even_for_a_touching_share() {
        let mut r = vec![0.0f32, 0.0, 0.0];
        accumulate(&mut r, &[1.0, GRID_NODATA, f32::NAN], 0.5);
        accumulate(&mut r, &[GRID_NODATA, 2.0, 1.0], 0.0);
        assert_eq!(r[0], GRID_NODATA);
        assert_eq!(r[1], GRID_NODATA, "the marker sticks");
        assert!(r[2].is_nan());
    }

    #[test]
    fn minus_901_is_an_ordinary_value_here() {
        let mut r = vec![0.0f32];
        accumulate(&mut r, &[-901.0], 1.0);
        assert_eq!(r, vec![-901.0]);
    }

    #[test]
    fn find_maximum_is_ported_with_its_shape() {
        let s = series(SeriesKind::Accumulation, &[(0, 24), (1, 2), (2, 3)]);
        assert_eq!(s.end_time(), Some(BASE + 2 * H));
        let s = series(SeriesKind::Accumulation, &[(0, 1), (1, 2), (2, 3)]);
        assert_eq!(s.end_time(), Some(BASE + 3 * H));
        let s = series(SeriesKind::Accumulation, &[(0, 1), (1, 9), (2, 3), (3, 4)]);
        // Sorted: mid 1 is [1,9); right holds 4; its own end is the largest.
        assert_eq!(s.end_time(), Some(BASE + 9 * H));
    }

    #[test]
    fn windows_start_at_midnight_of_the_first_day_and_drop_the_tail() {
        let s = series(SeriesKind::Accumulation, &[(7, 8), (8, 9), (9, 10)]);
        let (w, tail) = resample_windows(&s, 180, 100).unwrap();
        assert_eq!(w.first().unwrap().start, BASE);
        assert_eq!(w.len(), 3);
        assert_eq!(w.last().unwrap().end, BASE + 9 * H);
        assert_eq!(tail, H);
        // The first two begin before the series: missing, not partial.
        assert_eq!(s.plan(w[0]), WindowPlan::Missing);
        assert_eq!(s.plan(w[1]), WindowPlan::Missing);
        assert!(resample_windows(&s, 7, 100).is_err());
        assert!(resample_windows(&s, 1, 2).is_err(), "over the limit");
    }

    #[test]
    fn inst_cum_takes_its_rule_from_its_first_record() {
        let (instant, period) = (hour(1, 1), hour(1, 2));
        let of = |c| SeriesKind::of(DataType::InstCum, c, period);
        assert_eq!(
            SeriesKind::of(DataType::InstCum, "PRECIPITATION", instant),
            SeriesKind::Instantaneous
        );
        assert_eq!(of("PRECIPITATION"), SeriesKind::Accumulation);
        assert_eq!(of("MY-VAR"), SeriesKind::Average);
        // The stated types do not look at the name or the window.
        assert_eq!(
            SeriesKind::of(DataType::PerAver, "PRECIPITATION", instant),
            SeriesKind::Average
        );
    }

    // The grid's CRS comes from `dss_source`, and these are the states no writer here can make.
    fn real_grid() -> Grid {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/public/vortex_p53/qpe.dss");
        let mut f = Dss7File::open(&path).unwrap();
        let c = f.catalog().unwrap();
        let live = c.resolved_live();
        let name = c.catalog.entries[live[0]].pathname.clone();
        f.read_grid(&name).unwrap()
    }

    #[test]
    fn an_undefined_grid_and_a_moved_cell_zero_stay_refused() {
        let mut g = real_grid();
        assert!(sink_crs_of(&g, "x").is_ok(), "the base case must work");
        g.grid_type = GridType::Undefined;
        assert!(sink_crs_of(&g, "x")
            .unwrap_err()
            .to_string()
            .contains("undefined"));
        let mut g = real_grid();
        g.x_coord_of_grid_cell_zero = 2000.0;
        assert!(sink_crs_of(&g, "x")
            .unwrap_err()
            .to_string()
            .contains("cell zero"));
    }

    #[test]
    fn a_window_stated_twice_is_refused() {
        let e = TemporalSeries::new(SeriesKind::Accumulation, vec![hour(0, 1), hour(0, 1)])
            .unwrap_err();
        assert!(e.to_string().contains("2400"), "{e}");
    }

    #[test]
    fn the_2400_convention_reads_as_one_instant() {
        assert_eq!(parse_stamp("01JAN2024:2400"), parse_stamp("02JAN2024:0000"));
        assert_ne!(parse_stamp("01JAN2024:2400"), parse_stamp("01JAN2024:0000"));
        // And crossing the year, in both spellings.
        assert_eq!(parse_stamp("31DEC2023:2400"), parse_stamp("01JAN2024:0000"));
        let start = parse_stamp("01JAN2024:0000").unwrap();
        let end = parse_stamp("02JAN2024:0000").unwrap();
        let (d, e) = stamps_of(Window { start, end }, DataType::PerCum).unwrap();
        assert_eq!(
            (d.as_str(), e.as_str()),
            ("01JAN2024:0000", "01JAN2024:2400")
        );
        let at = Window { start: end, end };
        let (d, e) = stamps_of(at, DataType::InstVal).unwrap();
        assert_eq!((d.as_str(), e.as_str()), ("01JAN2024:2400", ""));
    }

    fn stamped(d: &str, e: &str, shift: i64) -> (String, String) {
        let time = FrameTime::Period {
            start: pair_of(parse_stamp(d).unwrap()),
            end: pair_of(parse_stamp(e).unwrap()),
        };
        let moved = shift_time(&time, shift, shift).unwrap();
        sink_dss::time_parts(&moved, DataType::PerCum).unwrap()
    }

    #[test]
    fn shifts_cross_boundaries_by_plain_arithmetic() {
        // Year, leap February, and a date with a clock change somewhere: exact, no zone.
        for (d, e, shift, want_d, want_e) in [
            (
                "31DEC2023:2300",
                "31DEC2023:2400",
                H,
                "01JAN2024:0000",
                "01JAN2024:0100",
            ),
            (
                "01JAN2024:0000",
                "01JAN2024:0100",
                -H,
                "31DEC2023:2300",
                "31DEC2023:2400",
            ),
            (
                "01MAR2020:0000",
                "01MAR2020:2400",
                -6 * H,
                "29FEB2020:1800",
                "01MAR2020:1800",
            ),
            (
                "28FEB2023:2300",
                "28FEB2023:2400",
                H,
                "01MAR2023:0000",
                "01MAR2023:0100",
            ),
            (
                "10MAR2024:0100",
                "10MAR2024:0200",
                H,
                "10MAR2024:0200",
                "10MAR2024:0300",
            ),
            (
                "03NOV2024:0100",
                "03NOV2024:0200",
                H,
                "03NOV2024:0200",
                "03NOV2024:0300",
            ),
        ] {
            let (a, b) = stamped(d, e, shift);
            assert_eq!(
                (a.as_str(), b.as_str()),
                (want_d, want_e),
                "{d} {e} {shift}"
            );
        }
    }

    #[test]
    fn an_instant_opens_and_a_period_closes() {
        let t = FrameTime::Instant(pair_of(BASE));
        assert_eq!(shift_time(&t, 0, 0).unwrap(), t);
        assert!(matches!(
            shift_time(&t, 0, H).unwrap(),
            FrameTime::Period { .. }
        ));
        assert!(shift_time(&t, H, 0).is_err(), "ends before it starts");
        let p = FrameTime::Period {
            start: pair_of(BASE),
            end: pair_of(BASE + H),
        };
        assert!(matches!(
            shift_time(&p, H, 0).unwrap(),
            FrameTime::Instant(_)
        ));
        assert!(shift_time(&p, i64::MAX, 0).is_err(), "overflow is refused");
        assert_eq!(
            shift_time(&FrameTime::Invariant, H, H).unwrap(),
            FrameTime::Invariant
        );
    }

    #[test]
    fn trapezoid_is_temporal_data_reader_test_value() {
        // `readPeriodInstantPartialMultipleGrids`: 02:15 to 04:50 over 15, 20, 18, 22, 15.
        let s = TemporalSeries::new(
            SeriesKind::Instantaneous,
            (1..=5).map(|h| hour(h, h)).collect(),
        )
        .unwrap();
        let cells: Vec<Vec<f32>> = [15.0f32, 20.0, 18.0, 22.0, 15.0]
            .iter()
            .map(|&v| vec![v; 9])
            .collect();
        let w = Window {
            start: BASE + 2 * H + 900,
            end: BASE + 4 * H + 3000,
        };
        let out = run(&s, &cells, w, Rule::Vortex);
        assert_eq!(out[0], 19.341398f32);
        // A point between: `readPointInstantInBetweenGrids`.
        let at = Window {
            start: BASE + H + 900,
            end: BASE + H + 900,
        };
        assert_eq!(run(&s, &cells, at, Rule::Vortex)[0], 16.25);
    }

    #[test]
    fn a_missing_instant_differs_by_rule_as_stated() {
        let s =
            TemporalSeries::new(SeriesKind::Instantaneous, vec![hour(0, 0), hour(1, 1)]).unwrap();
        let cells = vec![vec![GRID_NODATA, 1.0], vec![2.0, 3.0]];
        let strict = run(&s, &cells, hour(0, 1), Rule::Strict);
        assert_eq!(strict[0], GRID_NODATA);
        assert_eq!(strict[1], 2.0);
        let vortex = run(&s, &cells, hour(0, 1), Rule::Vortex);
        assert_eq!(vortex[0], GRID_NODATA / 60.0, "a value, as Vortex stores");
        assert_eq!(vortex[1], 2.0);
    }

    #[test]
    fn nearest_follows_the_gap_rule() {
        let (s, _) = accumulation();
        let start = |h2: i64| s.windows()[s.nearest(BASE + h2 * H / 2).unwrap()].start;
        assert_eq!(start(6), BASE + 3 * H, "on a start");
        assert_eq!(start(5), BASE + 2 * H, "midpoint, the earlier");
        assert_eq!(start(1), BASE + H, "before the first");
        assert_eq!(start(13), BASE + 5 * H, "after the last");
    }

    #[test]
    fn min_max_drops_the_last_instant_found() {
        let s = TemporalSeries::new(
            SeriesKind::Instantaneous,
            (1..=5).map(|h| hour(h, h)).collect(),
        )
        .unwrap();
        // 03:00 to 05:30: 3, 4 and 5 are found, and 5 is dropped: `getInstantMinMaxGridData`.
        let w = Window {
            start: BASE + 3 * H,
            end: BASE + 5 * H + 1800,
        };
        assert_eq!(s.min_max_sources(w).unwrap(), vec![2, 3]);
        let none = series(SeriesKind::Instantaneous, &[]);
        assert_eq!(none.min_max_sources(hour(0, 1)), None);
    }

    #[test]
    fn min_max_skips_the_marker_and_leaves_its_starts() {
        let mut min = vec![f32::MAX; 2];
        let mut max = vec![-f32::MAX; 2];
        min_max_step(&mut min, &mut max, &[GRID_NODATA, 1.0]);
        min_max_step(&mut min, &mut max, &[GRID_NODATA, f32::NAN]);
        assert_eq!((min[0], max[0]), (f32::MAX, -f32::MAX));
        assert_eq!((min[1], max[1]), (1.0, 1.0));
    }
}

//! NetCDF and HDF5 files through the importer: [`NetcdfFile`] behind
//! [`GridSource`], with the reader's limits and a stop a caller can reach.
//!
//! The source reads what `GridDatasetReader` in Vortex 1.1.0 reads (`source/netcdf.rs`), and the
//! importer's own rules then apply to each step: the refusal of precipitation at an instant, the
//! unit table, the writer's checks. Two rules are this module's own, both stricter than Vortex and
//! both lifted by `window_rule="vortex"`:
//!
//! - **A window a file-name rule moved.** `GridDatasetReader.adjustTimeForSpecialFile` stamps some
//!   files by their name (`SpecialFileType`): `01.nc` gets a day from its first step, `gfs.nc` is
//!   shifted 90 minutes, `hrrr*wrfsfcf*` gets an hour. Where that differs from what the file itself
//!   states, by its time bounds or its times, the step is refused and the refusal names both. One rule
//!   is taken by default, [`AORC_RULE`]: there the name selects the product's convention, and the
//!   file's own times give each hour's end.
//! - **Precipitation that is not a depth.** `VortexVariable.fromName` calls a variable precipitation
//!   by its name alone, so a count, an index or a percentage can be stored as `PRECIPITATION`,
//!   PER-CUM. Here the source's units must be a depth, or a rate Vortex converts to one.

use crate::error::VortexError;
use crate::frame::{DataType, FrameTime, GridFrame};
use crate::import::{GridSource, SourceGridDef, SourceLimits, StepMeta};
use crate::products::window as product_window;
use crate::products::WindowRule;
use crate::sink_dss;
use crate::source::netcdf::{FrameHeader, Limits, NetcdfFile};
use crate::units;
use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The one file-name rule taken by default, `AORC_APCP`.
///
/// **The file's name selects the product's convention, and the file's own times select the end
/// instants.** A name matching `*aorc*apcp*nc4*` says the file is AORC precipitation, whose value at a
/// time is the depth over the hour ending then: NWS documents the product that way (AORC v1.1
/// methods, section 4.1: "one-hour accumulated surface-precipitation (APCP) ending at the 'top' of
/// each hour"), and its time is wgrib2's verification time, the end of the interval. Each record then
/// ends at a time read from the file's `time` variable and starts an hour earlier.
///
/// **No digits in the name are read or compared.** A file renamed to another date or hour, or to a
/// name with no date, keeps the stamps of its own times.
///
/// Three things in the file must hold as well, or the step is refused: the variable is
/// `APCP_surface`; the file states no period of its own, so the rule moves nothing the file said; and
/// its steps, if several, are one hour apart by its own times. **A file of one step has no spacing to
/// check, so that condition passes with nothing tested:** for one step the hour rests on the name and
/// the variable's name. A file of other precipitation renamed to this pattern, with a variable so
/// named, would be stamped as AORC.
///
/// An AORC stamp of no length once gave HEC-HMS no rain.
pub const AORC_RULE: &str = "AORC_APCP";
const AORC_VARIABLE: &str = "APCP_surface";
const HOUR_MS: i64 = 3_600_000;

/// What a caller may set on the NetCDF reader. `None` keeps the reader's default
/// ([`Limits::default`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetcdfOptions {
    /// Seconds one read may take. **Not a deadline:** it is checked before each chunk and each filter
    /// stage, and a deflate or szip stage cannot be interrupted, so a read ends that much late. A
    /// limit of 1 s took 17.6 s (measured) on `double_deflate_8x8.nc4`, whose chunks each
    /// inflate to 256 MiB twice. No limit by default: a read's time is bounded by
    /// [`Self::max_decoded_bytes_per_read`], and one small file can still take about 45 s in one read
    /// (measured).
    pub max_seconds_per_read: Option<u64>,
    /// Bytes one read may decode. The reader's default is 32 GiB; a step over it is a typed refusal
    /// naming this setting.
    pub max_decoded_bytes_per_read: Option<u64>,
    /// Bytes of the chunk cache. A step whose chunks hold several steps is cheap to repeat only when
    /// the cache holds them all.
    pub chunk_cache_bytes: Option<usize>,
    /// Bytes of parsed structures the reader may build in one open or read.
    pub max_reader_bytes: Option<u64>,
}

impl NetcdfOptions {
    /// The reader's limits for one import. A file is read by range, so the importer's input-size
    /// limit, which bounds what is held in memory, does not apply; the reader's own does.
    pub fn limits(&self, source: &SourceLimits) -> Limits {
        let d = Limits::default();
        Limits {
            max_cells: source.max_cells,
            max_seconds_per_read: self.max_seconds_per_read,
            max_decoded_bytes_per_read: self
                .max_decoded_bytes_per_read
                .unwrap_or(d.max_decoded_bytes_per_read),
            chunk_cache_bytes: self.chunk_cache_bytes.unwrap_or(d.chunk_cache_bytes),
            max_reader_bytes: self.max_reader_bytes.unwrap_or(d.max_reader_bytes),
            ..d
        }
    }
}

/// A stop a caller holds while a run it cannot otherwise reach is in progress. Asking ends the run
/// before its next frame, and ends a NetCDF read in progress before its next chunk or filter stage
/// ([`NetcdfFile::request_stop`]). A stage in progress finishes first: 17.1 s from the request to
/// the return was measured on a file whose chunks each inflate to 256 MiB twice. A
/// warp or a write in progress also finishes. Clones share one flag.
#[derive(Debug, Clone, Default)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the run to stop. Safe from any thread.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Withdraw the request, so the handle can be used for another run.
    pub fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// The flag itself, for [`crate::import::ImportRequest::stop`].
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

/// Whether a file's first bytes are NetCDF classic or HDF5, at offset 0. Classic: `CDF` and a
/// version byte of 1, 2 or 5 (netCDF-C `libsrc/v1hpg.c`, `ncx_get_NC`'s magic). HDF5: the 8-byte
/// signature (HDF5 file format specification, superblock).
pub(crate) fn has_magic(head: &[u8]) -> bool {
    const HDF5: &[u8] = b"\x89HDF\r\n\x1a\n";
    head.starts_with(HDF5)
        || (head.len() >= 4 && &head[..3] == b"CDF" && matches!(head[3], 1 | 2 | 5))
}

/// An HDF5 signature after a user block, which the format allows at 512 bytes and each doubling.
/// Looked for only once no other format's bytes are found.
pub(crate) fn has_offset_magic(head: &[u8]) -> bool {
    const HDF5: &[u8] = b"\x89HDF\r\n\x1a\n";
    [512usize, 1024, 2048]
        .iter()
        .any(|at| head.get(*at..).is_some_and(|rest| rest.starts_with(HDF5)))
}

// A step as first read: what is kept of it, and the window the file itself states.
type Read = (Kept, Option<(i64, i64)>);

// One step, kept after the first read of a variable.
#[derive(Clone, Copy)]
struct Kept {
    time: FrameTime,
    data_type: DataType,
}

// A variable's steps, read once. `steps[i]` is `None` where that step is refused; the refusal is
// worded again from the file when asked for.
struct Resolved {
    name: String,
    // From the first step: the grid and names do not change between a variable's steps.
    shape: Option<FrameHeader>,
    steps: Vec<Option<Kept>>,
    hourly: bool,
}

/// One NetCDF or HDF5 file as the importer sees it.
pub struct NetcdfSource {
    file: NetcdfFile,
    window_rule: WindowRule,
    resolved: RefCell<Option<Resolved>>,
}

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

fn frame_time(window: Option<(i64, i64)>) -> Option<FrameTime> {
    let split = crate::source::netcdf::frame_stamp;
    Some(match window {
        None => FrameTime::Invariant,
        Some((a, b)) if a == b => FrameTime::Instant(split(b)?),
        Some((a, b)) => FrameTime::Period {
            start: split(a)?,
            end: split(b)?,
        },
    })
}

// A window in words, for a refusal.
fn words(time: Option<&FrameTime>) -> String {
    let Some(time) = time else {
        return "a time with a fraction of a second".into();
    };
    let kind = match time {
        FrameTime::Period { .. } => DataType::PerCum,
        _ => DataType::InstVal,
    };
    match (time, sink_dss::time_parts(time, kind)) {
        (FrameTime::Invariant, _) => "no time".into(),
        (FrameTime::Instant(_), Ok((d, _))) => format!("the instant {d}"),
        (FrameTime::Period { .. }, Ok((d, e))) => format!("{d} to {e}"),
        // A time off the whole minute has no pathname form: say it in seconds past midnight.
        (FrameTime::Instant(t), Err(_)) => format!("the instant {}", day_and_seconds(*t)),
        (FrameTime::Period { start, end }, Err(_)) => {
            format!("{} to {}", day_and_seconds(*start), day_and_seconds(*end))
        }
    }
}

fn day_and_seconds((julian, seconds): (i64, i64)) -> String {
    let (y, m, d) = dss_core::time::julian_to_ymd(julian);
    format!("{y:04}-{m:02}-{d:02} at {seconds} s past midnight")
}

/// Whether a step's window may be used: Vortex's, where no file-name rule moved it off the file's own
/// statement, or where [`AORC_RULE`] did and the file agrees, or under `window_rule="vortex"`.
fn check_window(header: &FrameHeader, rule: WindowRule, hourly: bool) -> Result<(), VortexError> {
    let Some(name_rule) = header.name_rule else {
        return Ok(());
    };
    let own = frame_time(header.file_window);
    if own == Some(header.time) || rule == WindowRule::Vortex {
        return Ok(());
    }
    let what = format!("time window of {}", header.variable);
    if name_rule == AORC_RULE {
        let one_hour_ending_at_the_files_time = match (header.file_window, own, header.time) {
            (Some((a, b)), Some(FrameTime::Instant(at)), FrameTime::Period { end, .. }) => {
                a == b && end == at && frame_time(Some((b - HOUR_MS, b))) == Some(header.time)
            }
            _ => false,
        };
        let disagreement = if header.short_name != AORC_VARIABLE {
            Some(format!(
                "the variable is {}, not {AORC_VARIABLE}",
                header.short_name
            ))
        } else if !one_hour_ending_at_the_files_time {
            Some(format!("the file itself states {}", words(own.as_ref())))
        } else if !hourly {
            Some("the file's steps are not one hour apart".to_string())
        } else {
            None
        };
        return match disagreement {
            None => Ok(()),
            Some(why) => Err(refused(
                what,
                format!(
                    "the file's name selects Vortex's rule {name_rule}, which stamps this step {}, \
                     the hour ending at the file's own time, and {why}. Pass window_rule vortex for \
                     Vortex's stamps",
                    words(Some(&header.time))
                ),
            )),
        };
    }
    Err(refused(
        what,
        format!(
            "Vortex's file-name rule {name_rule} stamps this step {}, and the file itself states {}. \
             A file's name is a label, not a statement of the product's period. Observed behaviour we \
             did not implement by default: Vortex stamps the record by the name. Pass window_rule \
             vortex for Vortex's stamps",
            words(Some(&header.time)),
            words(own.as_ref())
        ),
    ))
}

/// Why a NetCDF variable that Vortex names precipitation is not stored as one by default, or `None`.
/// `label` is the caller's `units=`, which relabels and does not convert.
pub(crate) fn not_a_depth(
    variable: &str,
    source_units: &str,
    label: Option<&str>,
) -> Option<String> {
    let depth =
        |text: &str| units::parse_unit(Some(text)).is_some_and(product_window::is_depth_unit);
    let converts = matches!(
        units::parse_unit(Some(source_units)),
        Some(units::Unit::MmPerSecond | units::Unit::MmPerHour | units::Unit::MmPerDay)
    );
    // A rate over a period becomes a depth (`DataConverter.convert`).
    if converts || depth(source_units) {
        return None;
    }
    if source_units.trim().is_empty() {
        // The file states no units at all, so only the caller's label can say depth. Units it does
        // state, read here or not, are not a depth, and a label relabels without converting.
        return match label {
            Some(label) if depth(label) => None,
            _ => Some(format!(
                "{variable} is in {source_units:?}, which does not state a depth"
            )),
        };
    }
    Some(format!(
        "{variable} is in {source_units:?}, so it is not a rain depth"
    ))
}

impl NetcdfSource {
    /// Open a file. `stop`, when given, ends its reads as [`NetcdfFile::request_stop`] does.
    pub fn open(
        path: &Path,
        limits: Limits,
        window_rule: WindowRule,
        stop: Option<&Arc<AtomicBool>>,
    ) -> Result<Self, VortexError> {
        let file = NetcdfFile::open_with_stop(path, limits, stop.cloned())?;
        Ok(NetcdfSource {
            file,
            window_rule,
            resolved: RefCell::new(None),
        })
    }

    pub fn file(&self) -> &NetcdfFile {
        &self.file
    }

    // Read a variable's steps once: every header, then the window rule on each.
    fn resolve(&self, name: &str) -> Result<Resolved, VortexError> {
        let variable = self.file.variable(name)?;
        let count = variable.step_count();
        let mut headers: Vec<Option<Read>> = Vec::new();
        let mut steps: Vec<Option<Kept>> = Vec::new();
        let room = |_| {
            refused(
                format!("NetCDF variable {name}"),
                format!("cannot hold its {count} steps"),
            )
        };
        headers.try_reserve_exact(count).map_err(room)?;
        steps.try_reserve_exact(count).map_err(room)?;
        let mut shape: Option<FrameHeader> = None;
        for step in 0..count {
            match variable.frame_header(step) {
                Ok(header) => {
                    headers.push(Some((
                        Kept {
                            time: header.time,
                            data_type: header.data_type,
                        },
                        header.file_window,
                    )));
                    if shape.is_none() {
                        shape = Some(header);
                    }
                }
                Err(_) => headers.push(None),
            }
        }
        // AORC's rule is taken only on steps one hour apart, by the file's own times.
        let own: Vec<Option<i64>> = headers
            .iter()
            .map(|h| match h {
                Some((_, Some((a, b)))) if a == b => Some(*b),
                _ => None,
            })
            .collect();
        let hourly = own.windows(2).all(|pair| match (pair[0], pair[1]) {
            (Some(a), Some(b)) => b - a == HOUR_MS,
            _ => false,
        });
        for (step, kept) in headers.iter().enumerate() {
            let usable = match (kept, &shape) {
                (Some((kept, file_window)), Some(shape)) => {
                    let header = FrameHeader {
                        time: kept.time,
                        data_type: kept.data_type,
                        file_window: *file_window,
                        ..shape.clone()
                    };
                    check_window(&header, self.window_rule, hourly)
                        .ok()
                        .map(|_| *kept)
                }
                _ => None,
            };
            debug_assert!(step == steps.len());
            steps.push(usable);
        }
        Ok(Resolved {
            name: name.to_string(),
            shape,
            steps,
            hourly,
        })
    }

    fn with<T>(
        &self,
        name: &str,
        f: impl FnOnce(&Resolved) -> Result<T, VortexError>,
    ) -> Result<T, VortexError> {
        let mut slot = self
            .resolved
            .try_borrow_mut()
            .map_err(|_| refused("NetCDF source", "it is already in use"))?;
        if slot.as_ref().is_none_or(|r| r.name != name) {
            *slot = Some(self.resolve(name)?);
        }
        match slot.as_ref() {
            Some(resolved) => f(resolved),
            None => Err(refused("NetCDF source", "no variable was read")),
        }
    }

    // The step's refusal, worded from the file. Reached only for a step [`Self::resolve`] kept as
    // refused, so a file that now answers otherwise has changed.
    fn refusal(&self, name: &str, step: usize, hourly: bool) -> VortexError {
        let header = match self.file.variable(name).and_then(|v| v.frame_header(step)) {
            Ok(header) => header,
            Err(e) => return e,
        };
        match check_window(&header, self.window_rule, hourly) {
            Err(e) => e,
            Ok(()) => refused(
                format!("NetCDF variable {name}"),
                format!("step {step} was refused when the file was first read, and is not now: the file has changed"),
            ),
        }
    }

    fn kept(&self, name: &str, step: usize) -> Result<(Kept, FrameHeader), VortexError> {
        let found = self.with(name, |r| {
            let kept = match r.steps.get(step) {
                Some(kept) => *kept,
                None => {
                    return Err(refused(
                        format!("NetCDF variable {name}"),
                        format!("step {step} of {}", r.steps.len()),
                    ))
                }
            };
            Ok((kept, r.shape.clone(), r.hourly))
        })?;
        match found {
            (Some(kept), Some(shape), _) => Ok((kept, shape)),
            (_, _, hourly) => Err(self.refusal(name, step, hourly)),
        }
    }
}

impl GridSource for NetcdfSource {
    fn file_name(&self) -> &str {
        self.file.file_name()
    }

    fn variables(&self) -> Vec<String> {
        self.file.variable_names()
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        self.with(variable, |r| Ok(r.steps.len()))
    }

    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError> {
        let (kept, shape) = self.kept(variable, step)?;
        Ok(StepMeta {
            time: kept.time,
            units: shape.units,
            description: shape.description,
            source_type: kept.data_type,
        })
    }

    fn grid(&self, variable: &str, step: usize) -> Result<SourceGridDef, VortexError> {
        let (_, shape) = self.kept(variable, step)?;
        Ok(SourceGridDef {
            nx: shape.nx,
            ny: shape.ny,
            transform: shape.transform,
            crs: shape.crs,
            nodata: shape.nodata,
            stored_wkt: None,
        })
    }

    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        // A step the plan refused is never decoded under another's name.
        self.kept(variable, step)?;
        self.file.variable(variable)?.frame(step)
    }

    fn short_name(&self, variable: &str) -> Result<String, VortexError> {
        self.with(variable, |r| match &r.shape {
            Some(shape) => Ok(shape.short_name.clone()),
            // No step of it can be read, and the step's own refusal says why.
            None => Ok(variable.to_string()),
        })
    }

    fn precipitation_must_be_a_depth(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Crs, GeoTransform, NamedCrs, RowOrder};

    const T: i64 = 441_763_200_000; // 1984-01-01 00:00

    fn header(
        rule: Option<&'static str>,
        file: Option<(i64, i64)>,
        vortex: Option<(i64, i64)>,
    ) -> FrameHeader {
        let time = frame_time(vortex).unwrap();
        FrameHeader {
            nx: 2,
            ny: 2,
            row_order: RowOrder::NorthToSouth,
            transform: GeoTransform::axis_aligned(0.0, 1.0, 0.0, -1.0),
            crs: Crs::Named(NamedCrs::Shg),
            nodata: None,
            time,
            data_type: match time {
                FrameTime::Period { .. } => DataType::PerCum,
                _ => DataType::InstVal,
            },
            units: "kg/m^2".into(),
            variable: AORC_VARIABLE.into(),
            description: None,
            short_name: AORC_VARIABLE.into(),
            file_window: file,
            name_rule: rule,
        }
    }

    #[test]
    fn a_window_no_rule_moved_passes_and_a_moved_one_is_refused() {
        let own = Some((T, T + HOUR_MS));
        assert!(check_window(&header(None, own, Some((T, T))), WindowRule::Refuse, true).is_ok());
        assert!(check_window(&header(Some("GPM"), own, own), WindowRule::Refuse, true).is_ok());
        let moved = header(
            Some("GFS"),
            own,
            Some((T + 90 * 60_000, T + HOUR_MS + 90 * 60_000)),
        );
        let e = check_window(&moved, WindowRule::Refuse, true)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("GFS") && e.contains("01JAN1984:0130 to 01JAN1984:0230"),
            "{e}"
        );
        assert!(e.contains("01JAN1984:0000 to 01JAN1984:0100"), "{e}");
        assert!(check_window(&moved, WindowRule::Vortex, true).is_ok());
    }

    #[test]
    fn aorc_is_taken_only_when_the_file_agrees() {
        let aorc = |file, vortex| header(Some(AORC_RULE), file, vortex);
        let hour = Some((T - HOUR_MS, T));
        let good = aorc(Some((T, T)), hour);
        assert!(check_window(&good, WindowRule::Refuse, true).is_ok());
        // Steps not an hour apart.
        let e = check_window(&good, WindowRule::Refuse, false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not one hour apart"), "{e}");
        // Another variable.
        let mut other = good.clone();
        other.short_name = "TMP_2maboveground".into();
        let e = check_window(&other, WindowRule::Refuse, true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not APCP_surface"), "{e}");
        // The file states its own period, and it is not the rule's.
        let stated = aorc(Some((T - 3 * HOUR_MS, T)), hour);
        let e = check_window(&stated, WindowRule::Refuse, true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("31DEC1983:2100 to 31DEC1983:2400"), "{e}");
        // A stamp off by one step: the hour starting at the file's time.
        let late = aorc(Some((T, T)), Some((T, T + HOUR_MS)));
        assert!(check_window(&late, WindowRule::Refuse, true).is_err());
        // Each is Vortex's record when asked for.
        for h in [&other, &stated, &late] {
            assert!(check_window(h, WindowRule::Vortex, false).is_ok());
        }
    }

    #[test]
    fn precipitation_must_state_a_depth() {
        assert_eq!(not_a_depth("precipitationCal", "mm", None), None);
        assert_eq!(not_a_depth("APCP_surface", "kg/m^2", None), None);
        assert_eq!(not_a_depth("precipitationCal", "mm/hr", None), None);
        assert!(not_a_depth("precipitationCal_cnt", "count", None).is_some());
        assert!(not_a_depth("probabilityLiquidPrecipitation", "percent", Some("MM")).is_some());
        assert!(not_a_depth("precip", "", None).is_some());
        assert_eq!(not_a_depth("precip", "", Some("IN")), None);
        assert!(not_a_depth("precip", "", Some("PERCENT")).is_some());
        assert!(not_a_depth("precipitation_cnt", "count", Some("MM")).is_some());
    }

    #[test]
    fn magic_is_at_the_first_byte() {
        assert!(has_magic(b"CDF\x01rest"));
        assert!(has_magic(b"CDF\x02"));
        assert!(has_magic(b"CDF\x05"));
        assert!(has_magic(b"\x89HDF\r\n\x1a\nmore"));
        assert!(!has_magic(b"CDF\x03"));
        assert!(!has_magic(b"CDF"));
        assert!(!has_magic(b" CDF\x01"));
        assert!(!has_magic(b"GRIB"));
        let mut block = vec![0u8; 512];
        block.extend_from_slice(b"\x89HDF\r\n\x1a\n");
        assert!(!has_magic(&block) && has_offset_magic(&block));
        assert!(!has_offset_magic(&block[..515]));
    }
}

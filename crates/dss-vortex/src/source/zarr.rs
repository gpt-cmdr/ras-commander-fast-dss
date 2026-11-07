//! A local Zarr v2 directory store: the route for AORC v1.1 (packet P49, decision V8).
//!
//! No network: a commander library fetches the metadata and the chunks it needs into a folder, and
//! this reads the folder. Sources: the Zarr v2 storage specification; `xarray` 2026.9.0 with
//! `zarr-python` 3.4.2 as the reference for what a store's values mean (`oracle/vortex/compare_zarr.py`);
//! NetCDF-Java 5.5.3 for the grid's edges, as the NetCDF source reads an AORC `.nc4`.
//! Vortex 0.13.3 and 1.1.0 read no Zarr, so Vortex is not a reference here.
//!
//! Layers: [`json`] parses metadata, [`codec`] decodes one chunk, and this file holds the store,
//! its axes and time, and the windowed read.
//!
//! **What a cell is.** A stored value is missing if it equals the array's `fill_value`, its
//! `_FillValue`, or a `missing_value`, compared exactly as `xarray` does, so a real cell that
//! happens to hold the fill is missing here as it is there. Any other value is
//! `(f32)(scale_factor * v + add_offset)` computed in `f64`: NetCDF-Java's order, and bit for bit
//! what `xarray` gives when its `float64` result is cast to `float32`.
//!
//! **A chunk with no file** is the array's `fill_value` (the specification), which is in the mask,
//! so it reads as missing. AORC states `fill_value: null`, for which the specification promises no
//! content. Here that also reads as missing, never as a number. Observed behaviour we did not
//! implement: `zarr-python` 3.4.2 reads such a chunk as zeros, so `xarray` reports 0 mm of rain
//! where a partial store has no data.
//!
//! **Time.** A store's time axis states instants. AORC's precipitation is the hour ending at its
//! stamp (NWS OWP, "AORC v1.1 methods", section 4.1: "one-hour accumulated surface-precipitation
//! (APCP) ending at the 'top' of each hour"; Vortex 1.1.0 `GridDatasetReader`, its `AORC_APCP` file
//! rule, `instantTime.minusHours(1)`). So `APCP_surface` is the period (T - 1 h, T], PER-CUM, in a
//! store recognized as AORC v1.1: its `aorc_version` is the text `v1.1`, its times are at the top
//! of the hour, and no `time_label`, `accumulation_interval_hours` or `cell_methods` it states
//! says otherwise. The period is what the product is, not how its times are spaced: a subset with
//! gaps, or one step alone, is still hourly totals. That is a rule about what the store states,
//! not about its file name. A variable that claims to be AORC's and fails the rule has no period
//! of its own, and is refused with what to state; so is any other precipitation, by the
//! importer, unless the caller states an accumulation. Every other variable keeps its instant.
//!
//! **Staying inside the store.** Every path read is below the store's root, and each component
//! on the way is looked at without following it: a junction or symbolic link in any of them is
//! refused, and what remains is resolved and must be inside the root ([`inside`]). Not covered: a
//! link put in place while a read is under way, between that check and the opening of the file.
//!
//! **Memory and time.** One chunk is decoded at a time, to the size its metadata gives it, and
//! only chunks a window touches are opened. Every allocation sized from the store is fallible, and
//! every loop is bounded by a limit in [`Limits`] or by the bytes read.

mod codec;
mod json;

use crate::crs::CrsDef;
use crate::error::VortexError;
use crate::import::{GridSource, OpenOptions, SourceGridDef, StepMeta};
use crate::products::window::Accumulation;
use crate::products::WindowRule;
use crate::{Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder};
use codec::Compressor;
use json::Json;
use std::collections::VecDeque;
use std::ops::Range;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

fn decode_error(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: "Zarr",
        message: message.into(),
    }
}

/// EPSG:4326 as GDAL 3.2 writes it in WKT1, which is what AORC's `crs` attribute names.
pub const EPSG_4326_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST],AUTHORITY[\"EPSG\",\"4326\"]]";

/// Bounds on what a store can make this reader allocate, read and decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most cells in one grid: the rows times the columns of a window.
    pub max_cells: usize,
    /// Most bytes one read may return: its steps times its cells times four.
    pub max_read_bytes: u64,
    /// Largest metadata file. AORC's consolidated metadata is 12 kB.
    pub max_metadata_bytes: u64,
    /// Most arrays in one store, and most directory entries looked at to find them.
    pub max_variables: usize,
    /// Most values in one axis.
    pub max_axis_points: usize,
    /// Largest decoded chunk. AORC's is 9.4 MB.
    pub max_chunk_bytes: usize,
    /// Most chunks one read may touch, counted from the window before any is opened.
    pub max_chunks_per_read: u64,
    /// Most bytes one read may decode, whole chunks, however few cells it returns. A chunk with no
    /// file costs nothing. One AORC hour of the whole grid is 1,089 chunks and 10.3 GB, which is
    /// over this: state a window.
    pub max_decoded_bytes_per_read: u64,
    /// Decoded chunks kept, in bytes, so the next step of a window does not decode them again.
    pub chunk_cache_bytes: usize,
    /// Time one read may take, checked before each chunk. No limit by default: a read's time is
    /// limited by [`Limits::max_decoded_bytes_per_read`].
    pub max_seconds_per_read: Option<u64>,
    /// Most cells one import may read from a store: its steps times the cells of its window,
    /// counted while planning, before any is read. 2^32 is 122 hours of AORC's whole grid, or 26
    /// years of hours on a 150 x 150 window. An import over it is refused with the selection to
    /// add: each frame's own limits would otherwise be met one frame at a time, for a year.
    pub max_cells_per_import: u64,
    /// Refuse a read that touches a chunk with no file, instead of reading it as missing. For a
    /// caller who needs to know a local copy is complete over what it reads.
    pub refuse_absent_chunks: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_cells: 1 << 28,
            max_read_bytes: 4 << 30,
            max_metadata_bytes: 16 << 20,
            max_variables: 1 << 12,
            max_axis_points: 1 << 24,
            max_chunk_bytes: 256 << 20,
            max_chunks_per_read: 1 << 16,
            max_decoded_bytes_per_read: 4 << 30,
            chunk_cache_bytes: 48 << 20,
            max_seconds_per_read: None,
            max_cells_per_import: 1 << 32,
            refuse_absent_chunks: false,
        }
    }
}

/// Most dimensions an array may have. A grid here has two or three.
const MAX_RANK: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    I1,
    U1,
    I2,
    U2,
    I4,
    U4,
    I8,
    U8,
    F4,
    F8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dtype {
    kind: Kind,
    big_endian: bool,
}

impl Dtype {
    /// A NumPy array-protocol type string: byte order, kind, size.
    fn parse(text: &str) -> Option<Dtype> {
        let bytes = text.as_bytes();
        let (order, rest) = bytes.split_first()?;
        let kind = match rest {
            b"i1" => Kind::I1,
            b"u1" => Kind::U1,
            b"i2" => Kind::I2,
            b"u2" => Kind::U2,
            b"i4" => Kind::I4,
            b"u4" => Kind::U4,
            b"i8" => Kind::I8,
            b"u8" => Kind::U8,
            b"f4" => Kind::F4,
            b"f8" => Kind::F8,
            _ => return None,
        };
        let one_byte = matches!(kind, Kind::I1 | Kind::U1);
        let big_endian = match order {
            b'<' => false,
            b'>' => true,
            b'|' if one_byte => false,
            _ => return None,
        };
        Some(Dtype { kind, big_endian })
    }

    fn size(self) -> usize {
        match self.kind {
            Kind::I1 | Kind::U1 => 1,
            Kind::I2 | Kind::U2 => 2,
            Kind::I4 | Kind::U4 | Kind::F4 => 4,
            Kind::I8 | Kind::U8 | Kind::F8 => 8,
        }
    }

    /// One stored item as a double. Exact, except a 64-bit integer over 2^53, which is `None`.
    fn value(self, item: &[u8]) -> Option<f64> {
        let mut le = [0u8; 8];
        let n = self.size();
        le[..n].copy_from_slice(&item[..n]);
        if self.big_endian {
            le[..n].reverse();
        }
        let exact = |v: i128| (v.unsigned_abs() <= 1 << 53).then_some(v as f64);
        match self.kind {
            Kind::I1 => Some(f64::from(le[0] as i8)),
            Kind::U1 => Some(f64::from(le[0])),
            Kind::I2 => Some(f64::from(i16::from_le_bytes([le[0], le[1]]))),
            Kind::U2 => Some(f64::from(u16::from_le_bytes([le[0], le[1]]))),
            Kind::I4 => Some(f64::from(i32::from_le_bytes([le[0], le[1], le[2], le[3]]))),
            Kind::U4 => Some(f64::from(u32::from_le_bytes([le[0], le[1], le[2], le[3]]))),
            Kind::I8 => exact(i128::from(i64::from_le_bytes(le))),
            Kind::U8 => exact(i128::from(u64::from_le_bytes(le))),
            Kind::F4 => Some(f64::from(f32::from_le_bytes([le[0], le[1], le[2], le[3]]))),
            Kind::F8 => Some(f64::from_le_bytes(le)),
        }
    }

    fn is_float(self) -> bool {
        matches!(self.kind, Kind::F4 | Kind::F8)
    }
}

/// One array's metadata: `.zarray` and `.zattrs`.
#[derive(Debug, Clone)]
struct Array {
    name: String,
    shape: Vec<u64>,
    chunks: Vec<u64>,
    dtype: Dtype,
    compressor: Compressor,
    /// `fill_value`, as a stored value. `None` for `null`.
    fill: Option<f64>,
    separator: char,
    attrs: Json,
    /// `_ARRAY_DIMENSIONS`, the `xarray` convention.
    dims: Option<Vec<String>>,
}

impl Array {
    fn what(&self) -> String {
        format!("Zarr array {}", self.name)
    }

    fn attr(&self, name: &str) -> Option<&Json> {
        self.attrs.get(name)
    }

    fn text_attr(&self, name: &str) -> Option<&str> {
        self.attr(name).and_then(Json::as_text)
    }

    /// Bytes of one decoded chunk, held to [`Limits::max_chunk_bytes`].
    fn chunk_bytes(&self, limits: &Limits) -> Result<usize, VortexError> {
        let mut bytes = self.dtype.size() as u64;
        for c in &self.chunks {
            bytes = bytes.saturating_mul(*c);
        }
        usize::try_from(bytes)
            .ok()
            .filter(|b| *b <= limits.max_chunk_bytes)
            .ok_or_else(|| {
                refused(
                    self.what(),
                    format!(
                        "a chunk of {:?} is over the limit of {} bytes (max_chunk_bytes)",
                        self.chunks, limits.max_chunk_bytes
                    ),
                )
            })
    }
}

/// A name that is one path component and nothing else. An array's name becomes a folder under the
/// store, so one that could leave it, or that Windows reads as a device, is refused.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.ends_with(['.', ' '])
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        && !name.chars().any(|c| c.is_control())
        && !is_device_name(name)
}

/// Names Windows opens as a device whatever folder they are in, with or without an extension:
/// `CON`, `PRN`, `AUX`, `NUL`, the console's `CONIN$` and `CONOUT$`, and `COM` or `LPT` with one
/// digit, which may be a superscript one, two or three (Microsoft, "Naming Files, Paths, and
/// Namespaces"). Refused on every platform, so a store reads the same everywhere.
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&upper.as_str()) {
        return true;
    }
    let mut rest = upper.chars();
    let port: String = rest.by_ref().take(3).collect();
    let digit = rest.next();
    (port == "COM" || port == "LPT")
        && rest.next().is_none()
        && digit.is_some_and(|d| d.is_ascii_digit() || ['\u{b9}', '\u{b2}', '\u{b3}'].contains(&d))
}

/// A symbolic link, a junction, or any other reparse point: a name that stands for another place.
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT. A junction is one, and is not always a symbolic link.
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// What is at `path`, which must be below `root`, or `None` if nothing is. Every component from
/// the root down is looked at without following it, and a link in any of them is refused: an
/// array's folder, a chunk's parent, the file itself. What remains is then resolved and must still
/// be inside the root. The root itself is the caller's path and may be reached however they like.
///
/// Not covered: a link put in place between this check and the read that follows it.
fn inside(root: &Path, path: &Path, what: &str) -> Result<Option<std::fs::Metadata>, VortexError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| refused(what, format!("{} is not in the store", path.display())))?;
    let mut at = root.to_path_buf();
    let mut found: Option<std::fs::Metadata> = None;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(refused(
                what,
                format!("{} is not a path inside the store", path.display()),
            ));
        };
        // Something that is not a folder has nothing inside it.
        if found.as_ref().is_some_and(|m| !m.is_dir()) {
            return Ok(None);
        }
        at.push(name);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if is_link(&meta) {
            return Err(refused(
                what,
                format!(
                    "{} is a link, and a store is read only from inside its own folder",
                    at.display()
                ),
            ));
        }
        found = Some(meta);
    }
    let (Ok(real_root), Ok(real)) = (std::fs::canonicalize(root), std::fs::canonicalize(path))
    else {
        return Ok(None);
    };
    if !real.starts_with(&real_root) {
        return Err(refused(
            what,
            format!("{} resolves to a place outside the store", path.display()),
        ));
    }
    Ok(found)
}

/// Whether a plain file of this name is in the store's root. A link is not one.
fn has_file(root: &Path, name: &str) -> bool {
    matches!(inside(root, &root.join(name), "Zarr store"), Ok(Some(meta)) if meta.is_file())
}

/// Read a whole file of at most `most` bytes from below the store's root. `None` if it does not
/// exist. A link anywhere on the way is refused ([`inside`]), and so is anything but a regular file.
fn read_bounded(
    root: &Path,
    path: &Path,
    most: u64,
    what: &str,
) -> Result<Option<Vec<u8>>, VortexError> {
    use std::io::Read;
    let Some(meta) = inside(root, path, what)? else {
        return Ok(None);
    };
    if !meta.file_type().is_file() {
        return Err(refused(
            what,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.len() > most {
        return Err(refused(
            what,
            format!(
                "{} holds {} bytes, over the {most} it may",
                path.display(),
                meta.len()
            ),
        ));
    }
    let mut bytes: Vec<u8> = Vec::new();
    bytes
        .try_reserve_exact(meta.len() as usize + 1)
        .map_err(|_| refused(what, format!("could not allocate {} bytes", meta.len())))?;
    // The size was read before the file: a file that grew is cut off at the limit, not followed.
    std::fs::File::open(path)?
        .take(most + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > most {
        return Err(refused(
            what,
            format!("{} grew past {most} bytes", path.display()),
        ));
    }
    Ok(Some(bytes))
}

fn parse_json(bytes: &[u8], what: &str) -> Result<Json, VortexError> {
    json::parse(bytes).map_err(|why| decode_error(format!("{what}: its JSON is malformed: {why}")))
}

fn parse_array(name: &str, zarray: &Json, zattrs: &Json) -> Result<Array, VortexError> {
    let what = format!("Zarr array {name}");
    let bad = |why: &str| decode_error(format!("{what}: {why}"));
    if zarray.get("zarr_format") != Some(&Json::Int(2)) {
        return Err(refused(
            what,
            format!(
                "zarr_format is {:?}, and only version 2 is read",
                zarray.get("zarr_format")
            ),
        ));
    }
    let numbers = |key: &str| -> Result<Vec<u64>, VortexError> {
        let list = zarray
            .get(key)
            .and_then(Json::as_list)
            .ok_or_else(|| bad(&format!("it has no {key} list")))?;
        if list.len() > MAX_RANK {
            return Err(refused(
                what.clone(),
                format!("{} dimensions is over the limit of {MAX_RANK}", list.len()),
            ));
        }
        list.iter()
            .map(|v| v.as_u64())
            .collect::<Option<Vec<u64>>>()
            .ok_or_else(|| bad(&format!("its {key} are not whole numbers")))
    };
    let shape = numbers("shape")?;
    let chunks = numbers("chunks")?;
    if chunks.len() != shape.len() {
        return Err(bad("its chunks and shape differ in length"));
    }
    if chunks.contains(&0) {
        return Err(bad("a chunk has no extent"));
    }
    let dtype = match zarray.get("dtype") {
        Some(Json::Text(text)) => Dtype::parse(text).ok_or_else(|| {
            refused(
                what.clone(),
                format!("data type {text:?} is not a number this reader takes"),
            )
        })?,
        Some(Json::List(_)) => {
            return Err(refused(what, "a structured data type is not a grid"));
        }
        _ => return Err(bad("it has no dtype")),
    };
    let compressor = codec::compressor_of(&what, zarray.get("compressor").unwrap_or(&Json::Null))?;
    match zarray.get("filters") {
        None | Some(Json::Null) => {}
        Some(Json::List(filters)) if filters.is_empty() => {}
        Some(Json::List(filters)) => {
            let id = filters[0].get("id").and_then(Json::as_text).unwrap_or("?");
            return Err(refused(
                what,
                format!("filter {id:?} has no reader here; no filter does"),
            ));
        }
        Some(_) => return Err(bad("its filters are not a list")),
    }
    match zarray.get("order").and_then(Json::as_text) {
        Some("C") => {}
        Some("F") => {
            return Err(refused(
                what,
                "its chunks are in Fortran order, which has no sample here",
            ))
        }
        _ => return Err(bad("its order is not C or F")),
    }
    let fill = match zarray.get("fill_value") {
        None | Some(Json::Null) => None,
        Some(Json::Text(text)) if dtype.is_float() => Some(match text.as_str() {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => return Err(bad("its fill_value is text that is not NaN or an infinity")),
        }),
        Some(value @ (Json::Int(_) | Json::Float(_))) => Some(
            value
                .as_f64()
                .ok_or_else(|| bad("its fill_value has no exact double"))?,
        ),
        Some(_) => return Err(bad("its fill_value is not a number")),
    };
    let separator = match zarray.get("dimension_separator") {
        None => '.',
        Some(Json::Text(t)) if t == "." => '.',
        Some(Json::Text(t)) if t == "/" => '/',
        Some(_) => return Err(bad("its dimension_separator is not . or /")),
    };
    if !matches!(zattrs, Json::Map(_)) {
        return Err(bad("its attributes are not an object"));
    }
    let dims = match zattrs.get("_ARRAY_DIMENSIONS") {
        None => None,
        Some(list) => {
            let names: Option<Vec<String>> = list
                .as_list()
                .and_then(|l| l.iter().map(|n| n.as_text().map(str::to_string)).collect());
            let names = names.ok_or_else(|| bad("its _ARRAY_DIMENSIONS are not names"))?;
            if names.len() != shape.len() {
                return Err(bad("its _ARRAY_DIMENSIONS and shape differ in length"));
            }
            Some(names)
        }
    };
    Ok(Array {
        name: name.to_string(),
        shape,
        chunks,
        dtype,
        compressor,
        fill,
        separator,
        attrs: zattrs.clone(),
        dims,
    })
}

struct Cache {
    /// Oldest first: `(array, chunk index, decoded bytes)`.
    entries: VecDeque<(usize, Vec<u64>, Arc<Vec<u8>>)>,
    bytes: usize,
}

/// What one read has used.
struct Meter {
    started: Instant,
    decoded: u64,
}

/// A Zarr v2 group on disk.
pub struct ZarrStore {
    root: PathBuf,
    file_name: String,
    arrays: Vec<Array>,
    /// Every array's attributes by name, including arrays whose chunks are not read. A grid
    /// mapping is only its attributes, and `rioxarray` writes it with zarr-python's default blosc.
    attributes: Vec<(String, Json)>,
    left_out: Vec<String>,
    consolidated: bool,
    limits: Limits,
    stop: Arc<AtomicBool>,
    cache: Mutex<Cache>,
    resolved: Mutex<Vec<Option<Arc<Resolved>>>>,
}

impl std::fmt::Debug for ZarrStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZarrStore")
            .field("file_name", &self.file_name)
            .field("arrays", &self.arrays.len())
            .finish()
    }
}

/// Whether a directory looks like a Zarr store of any version. For `import::open_source`.
pub fn is_store(path: &Path) -> bool {
    [".zmetadata", ".zgroup", ".zarray", "zarr.json"]
        .iter()
        .any(|name| has_file(path, name))
}

impl ZarrStore {
    pub fn open(path: &Path) -> Result<Self, VortexError> {
        Self::open_with_limits(path, Limits::default())
    }

    pub fn open_with_limits(path: &Path, limits: Limits) -> Result<Self, VortexError> {
        Self::open_with(path, limits, None)
    }

    /// Open with a stop flag the caller holds: setting it stops a read before its next chunk.
    pub fn open_with(
        path: &Path,
        limits: Limits,
        stop: Option<Arc<AtomicBool>>,
    ) -> Result<Self, VortexError> {
        let opened = catch_unwind(AssertUnwindSafe(|| Self::load(path, limits, stop)));
        opened.unwrap_or_else(|_| Err(decode_error("this reader panicked on this store")))
    }

    fn load(
        path: &Path,
        limits: Limits,
        stop: Option<Arc<AtomicBool>>,
    ) -> Result<Self, VortexError> {
        let what = format!("Zarr store {}", path.display());
        if !path.is_dir() {
            return Err(refused(what, "it is not a folder"));
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let read =
            |name: &str| read_bounded(path, &path.join(name), limits.max_metadata_bytes, &what);
        if has_file(path, "zarr.json") {
            return Err(VortexError::Unsupported {
                what: format!("{what}: Zarr version 3; version 2 is read"),
            });
        }
        let mut left_out = Vec::new();
        // (name, .zarray, .zattrs), each parsed.
        let mut found: Vec<(String, Json, Json)> = Vec::new();
        let empty = || Json::Map(Vec::new());
        let consolidated = read(".zmetadata")?;
        let group = match &consolidated {
            Some(bytes) => {
                let all = parse_json(bytes, &what)?;
                if all.get("zarr_consolidated_format") != Some(&Json::Int(1)) {
                    return Err(refused(
                        what,
                        format!(
                            "zarr_consolidated_format is {:?}, and only 1 is read",
                            all.get("zarr_consolidated_format")
                        ),
                    ));
                }
                let entries = all.get("metadata").and_then(Json::as_map).ok_or_else(|| {
                    decode_error(format!("{what}: .zmetadata has no metadata object"))
                })?;
                if entries.len() > limits.max_variables.saturating_mul(2).saturating_add(2) {
                    return Err(refused(
                        what,
                        format!(
                            "{} metadata entries is over the limit of {} arrays",
                            entries.len(),
                            limits.max_variables
                        ),
                    ));
                }
                for (key, value) in entries {
                    let Some(name) = key.strip_suffix("/.zarray") else {
                        continue;
                    };
                    if !safe_name(name) {
                        // A nested group's array, or a key that is not a name at all.
                        left_out.push(format!("{name:?}: not an array in the root group"));
                        continue;
                    }
                    let attrs = all
                        .get("metadata")
                        .and_then(|m| m.get(&format!("{name}/.zattrs")))
                        .cloned()
                        .unwrap_or_else(empty);
                    found.push((name.to_string(), value.clone(), attrs));
                }
                all.get("metadata").and_then(|m| m.get(".zgroup")).cloned()
            }
            None => {
                let group = read(".zgroup")?;
                if group.is_none() {
                    return Err(if has_file(path, ".zarray") {
                        refused(what, "it is one array, not a group of variables and axes")
                    } else {
                        VortexError::Unsupported {
                            what: format!("{what}: no .zmetadata or .zgroup; not a Zarr v2 store"),
                        }
                    });
                }
                let mut looked = 0usize;
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    looked += 1;
                    if looked > limits.max_variables {
                        return Err(refused(
                            what,
                            format!(
                                "over {} entries in the folder (max_variables)",
                                limits.max_variables
                            ),
                        ));
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let meta = entry.metadata()?;
                    if is_link(&meta) {
                        left_out.push(format!(
                            "{name:?}: it is a link, and a store is read only from inside its own \
                             folder"
                        ));
                        continue;
                    }
                    if !meta.is_dir() {
                        continue;
                    }
                    if !safe_name(&name) {
                        left_out.push(format!("{name:?}: not a name this reader takes"));
                        continue;
                    }
                    let dir = path.join(&name);
                    let array_what = format!("Zarr array {name}");
                    let read_in = |file: &str| {
                        read_bounded(
                            path,
                            &dir.join(file),
                            limits.max_metadata_bytes,
                            &array_what,
                        )
                    };
                    let Some(zarray) = read_in(".zarray")? else {
                        left_out.push(format!("{name:?}: a group, or a folder with no .zarray"));
                        continue;
                    };
                    let zattrs = match read_in(".zattrs")? {
                        Some(bytes) => parse_json(&bytes, &array_what)?,
                        None => empty(),
                    };
                    found.push((name, parse_json(&zarray, &array_what)?, zattrs));
                }
                match group {
                    Some(bytes) => Some(parse_json(&bytes, &what)?),
                    None => None,
                }
            }
        };
        match group.as_ref().and_then(|g| g.get("zarr_format")) {
            Some(Json::Int(2)) => {}
            other => {
                return Err(refused(
                    what,
                    format!("its group states zarr_format {other:?}, and only 2 is read"),
                ))
            }
        }
        if found.len() > limits.max_variables {
            return Err(refused(
                what,
                format!(
                    "{} arrays is over the limit of {}",
                    found.len(),
                    limits.max_variables
                ),
            ));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        let mut arrays: Vec<Array> = Vec::new();
        let mut attributes: Vec<(String, Json)> = Vec::new();
        arrays
            .try_reserve_exact(found.len())
            .map_err(|_| refused(what.clone(), "could not allocate its arrays"))?;
        attributes
            .try_reserve_exact(found.len())
            .map_err(|_| refused(what.clone(), "could not allocate its arrays"))?;
        for (name, zarray, zattrs) in &found {
            if consolidated.is_some() {
                // A store may also hold each array's own files. Where it does, they must say what
                // the consolidated copy says: readers differ on which they open.
                let dir = path.join(name);
                let array_what = format!("Zarr array {name}");
                for (file, known) in [(".zarray", zarray), (".zattrs", zattrs)] {
                    let own = read_bounded(
                        path,
                        &dir.join(file),
                        limits.max_metadata_bytes,
                        &array_what,
                    )?;
                    if let Some(bytes) = own {
                        if parse_json(&bytes, &array_what)? != *known {
                            return Err(refused(
                                array_what,
                                format!("its {file} and the store's .zmetadata disagree"),
                            ));
                        }
                    }
                }
            }
            // An array this reader does not take is left out, and named when it is asked for.
            attributes.push((name.clone(), zattrs.clone()));
            match parse_array(name, zarray, zattrs) {
                Ok(array) => arrays.push(array),
                Err(error) => left_out.push(format!("{name:?}: {error}")),
            }
        }
        let resolved = vec![None; arrays.len()];
        Ok(ZarrStore {
            root: path.to_path_buf(),
            file_name,
            arrays,
            attributes,
            left_out,
            consolidated: consolidated.is_some(),
            limits,
            stop: stop.unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            cache: Mutex::new(Cache {
                entries: VecDeque::new(),
                bytes: 0,
            }),
            resolved: Mutex::new(resolved),
        })
    }

    /// The store folder's name.
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Whether the metadata came from `.zmetadata`.
    pub fn is_consolidated(&self) -> bool {
        self.consolidated
    }

    /// Arrays and folders not offered, each with why.
    pub fn left_out(&self) -> &[String] {
        &self.left_out
    }

    /// Stop a read in progress from another thread. It ends with a typed error before its next
    /// chunk, and so do later reads until [`ZarrStore::clear_stop`].
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn clear_stop(&self) {
        self.stop.store(false, Ordering::SeqCst);
    }

    /// The flag [`ZarrStore::request_stop`] sets, for a caller that holds the store elsewhere.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// Arrays with named dimensions that are not their own axis, sorted: what `xarray` calls data
    /// variables. Whether one is a grid is found when it is asked for.
    pub fn variable_names(&self) -> Vec<String> {
        self.arrays
            .iter()
            .filter(|a| match &a.dims {
                Some(dims) => !(dims.len() == 1 && dims[0] == a.name),
                None => false,
            })
            .map(|a| a.name.clone())
            .collect()
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.arrays.iter().position(|a| a.name == name)
    }

    /// One variable, with its axes, time and CRS read.
    pub fn variable(&self, name: &str) -> Result<Variable<'_>, VortexError> {
        let what = format!("Zarr variable {name}");
        let index = self
            .find(name)
            .filter(|i| self.arrays[*i].dims.is_some())
            .ok_or_else(|| {
                let left = self
                    .left_out
                    .iter()
                    .find(|l| l.starts_with(&format!("{name:?}:")));
                refused(
                    what.clone(),
                    match left {
                        Some(why) => format!("left out: {why}"),
                        None => "not in this store, or it names no dimensions".to_string(),
                    },
                )
            })?;
        let known = self.resolved.lock().ok().and_then(|r| r[index].clone());
        let resolved = match known {
            Some(r) => r,
            None => {
                let made = catch_unwind(AssertUnwindSafe(|| self.resolve(index)))
                    .unwrap_or_else(|_| Err(decode_error("this reader panicked on this store")))
                    .map_err(|error| match error {
                        VortexError::Refused { what: w, reason } if !w.contains(name) => {
                            VortexError::Refused {
                                what: format!("{what}: {w}"),
                                reason,
                            }
                        }
                        other => other,
                    })?;
                let made = Arc::new(made);
                if let Ok(mut slots) = self.resolved.lock() {
                    slots[index] = Some(made.clone());
                }
                made
            }
        };
        Ok(Variable {
            store: self,
            r: resolved,
        })
    }

    /// The path of one chunk. Its parts are numbers, so it cannot leave the array's folder.
    fn chunk_path(&self, array: &Array, index: &[u64]) -> PathBuf {
        let mut path = self.root.join(&array.name);
        if array.separator == '/' {
            for i in index {
                path.push(i.to_string());
            }
        } else {
            let key: Vec<String> = index.iter().map(u64::to_string).collect();
            path.push(key.join("."));
        }
        path
    }

    /// One decoded chunk, or `None` if it has no file.
    fn chunk(
        &self,
        array_index: usize,
        index: &[u64],
        meter: &mut Meter,
    ) -> Result<Option<Arc<Vec<u8>>>, VortexError> {
        let array = &self.arrays[array_index];
        if self.stop.load(Ordering::SeqCst) {
            return Err(refused(array.what(), "the read was stopped"));
        }
        if let Some(most) = self.limits.max_seconds_per_read {
            if meter.started.elapsed().as_secs() >= most {
                return Err(refused(
                    array.what(),
                    format!("the read passed its limit of {most} s (max_seconds_per_read)"),
                ));
            }
        }
        if let Ok(cache) = self.cache.lock() {
            let hit = cache
                .entries
                .iter()
                .find(|(a, i, _)| *a == array_index && i == index);
            if let Some((_, _, bytes)) = hit {
                return Ok(Some(bytes.clone()));
            }
        }
        let expected = array.chunk_bytes(&self.limits)?;
        let path = self.chunk_path(array, index);
        let what = format!("Zarr chunk {}", path.display());
        let Some(stored) =
            read_bounded(&self.root, &path, codec::max_stored_bytes(expected), &what)?
        else {
            if self.limits.refuse_absent_chunks {
                return Err(refused(
                    what,
                    "it has no file, and a complete store was asked for (refuse_absent_chunks)",
                ));
            }
            return Ok(None);
        };
        meter.decoded = meter.decoded.saturating_add(expected as u64);
        if meter.decoded > self.limits.max_decoded_bytes_per_read {
            return Err(refused(
                array.what(),
                format!(
                    "the read would decode over {} bytes of chunks (max_decoded_bytes_per_read). \
                     Read a smaller window",
                    self.limits.max_decoded_bytes_per_read
                ),
            ));
        }
        let decoded = Arc::new(codec::decode(array.compressor, &stored, expected, &what)?);
        drop(stored);
        if expected <= self.limits.chunk_cache_bytes {
            if let Ok(mut cache) = self.cache.lock() {
                while cache.bytes + expected > self.limits.chunk_cache_bytes {
                    match cache.entries.pop_front() {
                        Some((_, _, old)) => cache.bytes -= old.len(),
                        None => break,
                    }
                }
                // A cache that cannot grow is not an error: the chunk is decoded again next time.
                if cache.entries.try_reserve(1).is_ok() {
                    let mut key: Vec<u64> = Vec::new();
                    if key.try_reserve_exact(index.len()).is_ok() {
                        key.extend_from_slice(index);
                        cache.bytes += expected;
                        cache.entries.push_back((array_index, key, decoded.clone()));
                    }
                }
            }
        }
        Ok(Some(decoded))
    }

    /// Visit a region's cells in C order, one run along the last dimension at a time:
    /// `(index of the run's first cell in the region, cells, their stored bytes)`. The bytes are
    /// `None` where the chunk has no file.
    fn visit(
        &self,
        array_index: usize,
        ranges: &[Range<u64>],
        mut run: impl FnMut(usize, usize, Option<&[u8]>) -> Result<(), VortexError>,
    ) -> Result<(), VortexError> {
        let array = &self.arrays[array_index];
        let rank = array.shape.len();
        if ranges.len() != rank || rank == 0 {
            return Err(refused(array.what(), "a region of another rank"));
        }
        for (d, r) in ranges.iter().enumerate() {
            if r.start >= r.end || r.end > array.shape[d] {
                return Err(refused(
                    array.what(),
                    format!(
                        "region {}..{} is empty or outside dimension {d} of {}",
                        r.start, r.end, array.shape[d]
                    ),
                ));
            }
        }
        let item = array.dtype.size();
        let first: Vec<u64> = (0..rank)
            .map(|d| ranges[d].start / array.chunks[d])
            .collect();
        let last: Vec<u64> = (0..rank)
            .map(|d| (ranges[d].end - 1) / array.chunks[d])
            .collect();
        let mut count: u64 = 1;
        for d in 0..rank {
            count = count.saturating_mul(last[d] - first[d] + 1);
        }
        if count > self.limits.max_chunks_per_read {
            return Err(refused(
                array.what(),
                format!(
                    "the read touches {count} chunks, over the limit of {} (max_chunks_per_read). \
                     Read a smaller window",
                    self.limits.max_chunks_per_read
                ),
            ));
        }
        // Cells before a position in the region, and in a chunk, per step of each dimension.
        let mut region_stride = vec![1usize; rank];
        let mut chunk_stride = vec![1usize; rank];
        for d in (0..rank - 1).rev() {
            let extent = (ranges[d + 1].end - ranges[d + 1].start) as usize;
            region_stride[d] = region_stride[d + 1].saturating_mul(extent);
            chunk_stride[d] = chunk_stride[d + 1].saturating_mul(array.chunks[d + 1] as usize);
        }
        let mut meter = Meter {
            started: Instant::now(),
            decoded: 0,
        };
        let mut index = first.clone();
        loop {
            let data = self.chunk(array_index, &index, &mut meter)?;
            // This chunk's part of the region, per dimension, in array coordinates.
            let part: Vec<Range<u64>> = (0..rank)
                .map(|d| {
                    let from = index[d] * array.chunks[d];
                    ranges[d].start.max(from)..ranges[d].end.min(from + array.chunks[d])
                })
                .collect();
            let cells = (part[rank - 1].end - part[rank - 1].start) as usize;
            let mut at: Vec<u64> = part.iter().map(|p| p.start).collect();
            loop {
                let mut in_region = 0usize;
                let mut in_chunk = 0usize;
                for d in 0..rank {
                    in_region += (at[d] - ranges[d].start) as usize * region_stride[d];
                    in_chunk += (at[d] - index[d] * array.chunks[d]) as usize * chunk_stride[d];
                }
                let bytes = match &data {
                    Some(bytes) => Some(
                        bytes
                            .get(in_chunk * item..(in_chunk + cells) * item)
                            .ok_or_else(|| decode_error("a chunk is shorter than its shape"))?,
                    ),
                    None => None,
                };
                run(in_region, cells, bytes)?;
                if !advance(&mut at, &part) {
                    break;
                }
            }
            // Next chunk.
            let mut d = rank;
            let mut done = true;
            while d > 0 {
                d -= 1;
                if index[d] < last[d] {
                    index[d] += 1;
                    done = false;
                    break;
                }
                index[d] = first[d];
            }
            if done {
                return Ok(());
            }
        }
    }

    /// A whole one-dimensional array, as doubles. A value that is missing, or a chunk with no file,
    /// is an error: an axis has no missing coordinates.
    fn axis(&self, array_index: usize) -> Result<Vec<f64>, VortexError> {
        let array = &self.arrays[array_index];
        let what = format!("Zarr axis {}", array.name);
        let n = match array.shape.as_slice() {
            [n] => usize::try_from(*n)
                .ok()
                .filter(|n| *n <= self.limits.max_axis_points && *n > 0),
            _ => return Err(refused(what, "it is not one-dimensional")),
        }
        .ok_or_else(|| {
            refused(
                what.clone(),
                format!(
                    "{:?} values is none, or over the limit of {}",
                    array.shape, self.limits.max_axis_points
                ),
            )
        })?;
        let mask = Mask::of(array)?;
        let mut out: Vec<f64> = Vec::new();
        out.try_reserve_exact(n)
            .map_err(|_| refused(what.clone(), format!("could not allocate {n} values")))?;
        out.resize(n, f64::NAN);
        let dtype = array.dtype;
        let item = dtype.size();
        let whole = 0..n as u64;
        self.visit(
            array_index,
            std::slice::from_ref(&whole),
            |at, cells, bytes| {
                let bytes = bytes.ok_or_else(|| {
                    refused(what.clone(), "a chunk of it has no file in the store")
                })?;
                for (i, stored) in bytes.chunks_exact(item).take(cells).enumerate() {
                    let value = dtype
                        .value(stored)
                        .filter(|v| v.is_finite() && !mask.is_missing(*v))
                        .ok_or_else(|| {
                            refused(what.clone(), "it has a missing or inexact coordinate")
                        })?;
                    out[at + i] = value;
                }
                Ok(())
            },
        )?;
        Ok(out)
    }
}

/// Step to the next row of a region: every dimension but the last, the last of those fastest.
/// `false` when every row has been visited.
fn advance(at: &mut [u64], part: &[Range<u64>]) -> bool {
    let mut d = at.len() - 1;
    while d > 0 {
        d -= 1;
        at[d] += 1;
        if at[d] < part[d].end {
            return true;
        }
        at[d] = part[d].start;
    }
    false
}

/// Which stored values mean missing, as `xarray` 2026.9.0 decides it for a Zarr v2 array.
#[derive(Debug, Clone)]
struct Mask {
    /// Stored values. The first is the one the frame's marker is made from.
    fills: Vec<f64>,
}

impl Mask {
    fn of(array: &Array) -> Result<Mask, VortexError> {
        let what = array.what();
        let number = |name: &str, value: &Json| -> Result<f64, VortexError> {
            match value {
                Json::Int(_) | Json::Float(_) => value.as_f64(),
                _ => None,
            }
            .ok_or_else(|| {
                refused(
                    what.clone(),
                    format!("its {name} is {value:?}, not a number with an exact double"),
                )
            })
        };
        let mut fills: Vec<f64> = Vec::new();
        let add = |fills: &mut Vec<f64>, v: f64| {
            if !fills.iter().any(|f| same_stored(*f, v)) {
                fills.push(v);
            }
        };
        let attribute = match array.attr("_FillValue") {
            None | Some(Json::Null) => None,
            Some(value) => Some(number("_FillValue", value)?),
        };
        match (array.fill, attribute) {
            // xarray takes the array's fill_value and drops the attribute. Which the writer meant
            // is not known, so neither is taken.
            (Some(fill), Some(attribute)) if !same_stored(fill, attribute) => {
                return Err(refused(
                    what,
                    format!(
                        "its fill_value {fill} and its _FillValue attribute {attribute} differ"
                    ),
                ))
            }
            (Some(fill), _) => add(&mut fills, fill),
            (None, Some(attribute)) => add(&mut fills, attribute),
            (None, None) => {}
        }
        match array.attr("missing_value") {
            None | Some(Json::Null) => {}
            Some(Json::List(values)) => {
                for value in values.iter().take(64) {
                    add(&mut fills, number("missing_value", value)?);
                }
                if values.len() > 64 {
                    return Err(refused(what, "over 64 missing values"));
                }
            }
            Some(value) => add(&mut fills, number("missing_value", value)?),
        }
        Ok(Mask { fills })
    }

    fn is_missing(&self, stored: f64) -> bool {
        self.fills.iter().any(|f| same_stored(*f, stored))
    }
}

/// Exact equality, with NaN equal to NaN: `xarray`'s `isnull` when the fill is NaN.
fn same_stored(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// Stored values to cells.
#[derive(Debug, Clone)]
struct Enhance {
    mask: Mask,
    /// `(scale_factor, add_offset)`, when either is stated.
    scale: Option<(f64, f64)>,
}

impl Enhance {
    fn of(array: &Array) -> Result<Enhance, VortexError> {
        let what = array.what();
        if matches!(array.dtype.kind, Kind::I8 | Kind::U8) {
            return Err(refused(what, "64-bit integer cells have no sample here"));
        }
        for name in ["valid_min", "valid_max", "valid_range", "_Unsigned"] {
            if array.attr(name).is_some() {
                // xarray ignores the valid_* attributes and NetCDF-Java masks by them.
                return Err(refused(
                    what,
                    format!("its {name} attribute has no reference result here"),
                ));
            }
        }
        let factor = |name: &str| -> Result<Option<f64>, VortexError> {
            match array.attr(name) {
                None => Ok(None),
                Some(value @ (Json::Int(_) | Json::Float(_))) => value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .map(Some)
                    .ok_or_else(|| refused(what.clone(), format!("its {name} is not finite"))),
                Some(other) => Err(refused(
                    what.clone(),
                    format!("its {name} is {other:?}, not a number"),
                )),
            }
        };
        let (scale, offset) = (factor("scale_factor")?, factor("add_offset")?);
        let scale = match (scale, offset) {
            (None, None) => None,
            (s, o) => Some((s.unwrap_or(1.0), o.unwrap_or(0.0))),
        };
        Ok(Enhance {
            mask: Mask::of(array)?,
            scale,
        })
    }

    /// `(float)(scale * v + offset)`, in double, with no fused step.
    fn value(&self, stored: f64) -> f32 {
        match self.scale {
            Some((scale, offset)) => (scale * stored + offset) as f32,
            None => stored as f32,
        }
    }

    /// The frame's marker: the first fill, as a cell. Missing cells are NaN, as the NetCDF source
    /// gives them, and the marker is what the warp fills with.
    fn nodata(&self) -> Option<f32> {
        self.mask.fills.first().map(|f| self.value(*f))
    }
}

/// What [`ZarrStore::variable`] works out once for a variable.
#[derive(Debug)]
struct Resolved {
    array: usize,
    /// Seconds since 1970 of each step. Empty for a variable with no time dimension.
    instants: Vec<i64>,
    has_time: bool,
    lat: Vec<f64>,
    lon: Vec<f64>,
    crs: Crs,
    enhance: Enhance,
    units: String,
    description: Option<String>,
    /// Seconds each step's value was accumulated over, ending at its instant, when the store's
    /// own statements give it.
    accumulation: Option<i64>,
    /// `accumulation_interval_hours`, in seconds, where the variable states one.
    stated_interval: Option<i64>,
    /// Why a period ending at each time contradicts the variable's `time_label`.
    label_not_end: Option<String>,
    /// Why a variable that claims to be AORC's precipitation is not read as it.
    unrecognized: Option<String>,
}

/// The `aorc_version` texts whose `APCP_surface` is known to be the hour ending at its time: NWS
/// OWP, "AORC v1.1 methods", section 4.1. Another version is not assumed to keep the convention.
const AORC_VERSIONS: [&str; 1] = ["v1.1"];

/// Rows and columns of a variable's grid, in the store's index order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub rows: Range<usize>,
    pub cols: Range<usize>,
}

/// West, south, east and north, in the store's CRS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

/// One selectable variable.
#[derive(Debug)]
pub struct Variable<'a> {
    store: &'a ZarrStore,
    r: Arc<Resolved>,
}

/// `CoordinateAxis1D.makeEdges` (NetCDF-Java 5.5.3), as the NetCDF source applies it.
fn make_edges(coords: &[f64]) -> Vec<f64> {
    let size = coords.len();
    let mut edge = vec![0.0; size + 1];
    if size < 2 {
        if size == 1 {
            edge[0] = coords[0];
            edge[1] = coords[0];
        }
        return edge;
    }
    for i in 1..size {
        edge[i] = (coords[i - 1] + coords[i]) / 2.0;
    }
    edge[0] = coords[0] - (edge[1] - coords[0]);
    edge[size] = coords[size - 1] + (coords[size - 1] - edge[size - 1]);
    edge
}

/// `CoordinateAxis1D.calcIsRegular`: every step within 0.5% of the mean step.
fn is_regular(coords: &[f64]) -> bool {
    let n = coords.len();
    if n < 2 {
        return true;
    }
    let increment = (coords[n - 1] - coords[0]) / (n - 1) as f64;
    increment != 0.0
        && (1..n).all(|i| {
            crate::source::grib::nearly_equals_f64(coords[i] - coords[i - 1], increment, 5.0e-3)
        })
}

/// A CF time unit: `(seconds per unit, seconds since 1970 of its base)`. CF conventions 1.11,
/// section 4.4. Only what both calendars agree on is read: a base in 1583 or later, at UTC.
fn parse_time_units(text: &str) -> Result<(i64, i64), String> {
    let lower = text.trim().to_ascii_lowercase();
    let (unit, base) = lower
        .split_once(" since ")
        .ok_or("it is not `<unit> since <date>`")?;
    let seconds = match unit.trim() {
        "seconds" | "second" | "secs" | "sec" | "s" => 1,
        "minutes" | "minute" | "mins" | "min" => 60,
        "hours" | "hour" | "hrs" | "hr" | "h" => 3600,
        "days" | "day" | "d" => 86_400,
        other => return Err(format!("unit {other:?} has no rule here")),
    };
    let base = base.trim();
    let (date, rest) = match base.find(['t', ' ']) {
        Some(at) => (&base[..at], base[at + 1..].trim()),
        None => (base, ""),
    };
    let mut ymd = date.splitn(3, '-');
    let mut field = |digits: usize| -> Option<i64> {
        let text = ymd.next()?;
        (text.len() <= digits && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())?
    };
    let (Some(year), Some(month), Some(day)) = (field(4), field(2), field(2)) else {
        return Err(format!("date {date:?} is not year-month-day"));
    };
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    };
    if year < 1583 || day < 1 || day > month_days {
        return Err(format!(
            "date {date:?} is not a date in 1583 or later, where the calendars agree"
        ));
    }
    // The zone: none, `z`, `utc`, or a zero offset.
    let mut clock = rest;
    for zone in ["utc", "z", "+00:00", "+0000", "+00", "-00:00"] {
        if let Some(before) = clock.strip_suffix(zone) {
            clock = before.trim();
            break;
        }
    }
    let mut of_day = 0i64;
    if !clock.is_empty() {
        let mut parts = clock.splitn(3, ':');
        let mut next = |most: i64, fraction: bool| -> Option<i64> {
            let Some(text) = parts.next() else {
                return Some(0);
            };
            let whole = match text.split_once('.') {
                Some((whole, zeros)) if fraction && zeros.bytes().all(|b| b == b'0') => whole,
                Some(_) => return None,
                None => text,
            };
            (whole.len() <= 2 && !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()))
                .then(|| whole.parse::<i64>().ok())?
                .filter(|v| *v <= most)
        };
        let (Some(h), Some(m), Some(s)) = (next(23, false), next(59, false), next(59, true)) else {
            return Err(format!("time {clock:?} is not a UTC time of day"));
        };
        of_day = h * 3600 + m * 60 + s;
    }
    let days = dss_core::time::ymd_to_julian(year, month as u32, day as u32)
        - dss_core::time::ymd_to_julian(1970, 1, 1);
    Ok((seconds, days * 86_400 + of_day))
}

/// Seconds either side of 1970 a time may be: the years 1583 to 9999.
const INSTANT_BOUND: f64 = 2.6e11;

const LATITUDE_UNITS: [&str; 6] = [
    "degrees_north",
    "degree_north",
    "degree_n",
    "degrees_n",
    "degreen",
    "degreesn",
];
const LONGITUDE_UNITS: [&str; 6] = [
    "degrees_east",
    "degree_east",
    "degree_e",
    "degrees_e",
    "degreee",
    "degreese",
];

impl ZarrStore {
    fn resolve(&self, index: usize) -> Result<Resolved, VortexError> {
        let array = &self.arrays[index];
        let what = format!("Zarr variable {}", array.name);
        let dims = array
            .dims
            .as_ref()
            .ok_or_else(|| refused(what.clone(), "it names no dimensions"))?;
        let (time_dim, y_dim, x_dim) = match dims.len() {
            2 => (None, 0, 1),
            3 => (Some(0), 1, 2),
            n => {
                return Err(refused(
                    what,
                    format!("not a grid: {n} dimensions, and a grid has (time,) y and x"),
                ))
            }
        };
        // A dimension's axis is the one-dimensional array of its name, on that dimension.
        let axis_of = |d: usize| -> Result<usize, VortexError> {
            let name = &dims[d];
            self.find(name)
                .filter(|i| {
                    let axis = &self.arrays[*i];
                    axis.shape.len() == 1
                        && axis.shape[0] == array.shape[d]
                        && axis.dims.as_ref().is_none_or(|ad| ad[0] == *name)
                })
                .ok_or_else(|| {
                    refused(
                        what.clone(),
                        format!(
                            "its dimension {name} has no axis: no one-dimensional array of that \
                             name and length {}",
                            array.shape[d]
                        ),
                    )
                })
        };
        let (y_axis, x_axis) = (axis_of(y_dim)?, axis_of(x_dim)?);
        let units_of = |i: usize| {
            self.arrays[i]
                .text_attr("units")
                .map(|u| u.trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        let (y_units, x_units) = (units_of(y_axis), units_of(x_axis));
        if LONGITUDE_UNITS.contains(&y_units.as_str()) && LATITUDE_UNITS.contains(&x_units.as_str())
        {
            return Err(refused(
                what,
                "longitude comes before latitude in its dimensions, which has no sample here",
            ));
        }
        if !LATITUDE_UNITS.contains(&y_units.as_str())
            || !LONGITUDE_UNITS.contains(&x_units.as_str())
        {
            return Err(refused(
                what,
                format!(
                    "not a latitude and longitude grid: its axes {} and {} have units {y_units:?} \
                     and {x_units:?}. Projected axes have no sample here",
                    dims[y_dim], dims[x_dim]
                ),
            ));
        }
        let (nx, ny) = (array.shape[x_dim], array.shape[y_dim]);
        let lat = self.axis(y_axis)?;
        let lon = self.axis(x_axis)?;
        if lat.iter().any(|v| !(-90.0..=90.0).contains(v)) {
            return Err(refused(what, "a latitude is outside -90 to 90"));
        }
        if lon.iter().any(|v| !(-180.0..=180.0).contains(v)) {
            return Err(refused(
                what,
                "a longitude is outside -180 to 180. Longitudes from 0 to 360 have no sample here",
            ));
        }
        if nx < 2 || ny < 2 || !is_regular(&lat) || !is_regular(&lon) {
            return Err(refused(
                what,
                "its latitudes or longitudes are not at least two, evenly spaced",
            ));
        }

        let enhance = Enhance::of(array)?;
        let crs = self.crs_of(array)?;

        let mut instants: Vec<i64> = Vec::new();
        if let Some(t) = time_dim {
            let axis_index = axis_of(t)?;
            let axis = &self.arrays[axis_index];
            let axis_what = format!("Zarr time axis {}", axis.name);
            let units = axis
                .text_attr("units")
                .ok_or_else(|| refused(axis_what.clone(), "it has no units"))?;
            match axis
                .text_attr("calendar")
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                None | Some("standard" | "gregorian" | "proleptic_gregorian") => {}
                Some(other) => {
                    return Err(refused(
                        axis_what,
                        format!("calendar {other:?} has no rule here"),
                    ))
                }
            }
            let (unit, base) = parse_time_units(units)
                .map_err(|why| refused(axis_what.clone(), format!("units {units:?}: {why}")))?;
            let values = self.axis(axis_index)?;
            instants
                .try_reserve_exact(values.len())
                .map_err(|_| refused(axis_what.clone(), "could not allocate its times"))?;
            for value in values {
                let seconds = value * unit as f64 + base as f64;
                if seconds.fract() != 0.0 || seconds.abs() > INSTANT_BOUND {
                    return Err(refused(
                        axis_what,
                        format!("time {value} is not a whole second in the years 1583 to 9999"),
                    ));
                }
                instants.push(seconds as i64);
            }
        }

        // What the store states about a period, whatever the product.
        let stated_interval = match array.attr("accumulation_interval_hours") {
            None | Some(Json::Null) => None,
            Some(value) => {
                let seconds = value.as_f64().map(|hours| hours * 3600.0);
                match seconds {
                    Some(s) if s.fract() == 0.0 && (1.0..=3.2e9).contains(&s) => Some(s as i64),
                    _ => {
                        return Err(refused(
                            what,
                            format!("its accumulation_interval_hours is {value:?}, not a period"),
                        ))
                    }
                }
            }
        };
        let label_not_end = match array.attr("time_label") {
            None | Some(Json::Null) => None,
            Some(Json::Text(text))
                if ["end", "ending", "right"]
                    .contains(&text.trim().to_ascii_lowercase().as_str()) =>
            {
                None
            }
            Some(other) => Some(format!(
                "its time_label is {other:?}, and a period here ends at each time"
            )),
        };

        // The AORC rule: see the module's documentation. A reason here means the variable claims
        // to be AORC's and is not read as it.
        let mut accumulation = None;
        let mut unrecognized = None;
        if array.name == "APCP_surface" && array.attr("aorc_version").is_some() {
            let version = array.attr("aorc_version");
            let recognized = version
                .and_then(Json::as_text)
                .is_some_and(|v| AORC_VERSIONS.contains(&v.trim()));
            let method = array.text_attr("cell_methods").map(|m| {
                crate::source::netcdf::parse_time_cell_method(m, time_dim.map(|t| dims[t].as_str()))
            });
            let reason = if !recognized {
                Some(format!(
                    "its aorc_version is {:?}, and {AORC_VERSIONS:?} is what is recognized",
                    version.unwrap_or(&Json::Null)
                ))
            } else if time_dim.is_none() {
                Some("it has no time axis".to_string())
            } else if instants.iter().any(|t| t.rem_euclid(3600) != 0) {
                Some("its times are not at the top of the hour".to_string())
            } else if let Some(why) = &label_not_end {
                Some(why.clone())
            } else if stated_interval.is_some_and(|s| s != 3600) {
                Some(format!(
                    "its accumulation_interval_hours states {} s, not one hour",
                    stated_interval.unwrap_or_default()
                ))
            } else if method
                .as_deref()
                .is_some_and(|m| !m.is_empty() && m != "sum")
            {
                Some(format!(
                    "its cell_methods state {:?} over time, not a sum",
                    method.unwrap_or_default()
                ))
            } else {
                None
            };
            match reason {
                None => accumulation = Some(3600),
                Some(why) => unrecognized = Some(why),
            }
        }

        Ok(Resolved {
            array: index,
            instants,
            has_time: time_dim.is_some(),
            lat,
            lon,
            crs,
            enhance,
            units: array.text_attr("units").unwrap_or_default().to_string(),
            description: array.text_attr("long_name").map(str::to_string),
            accumulation,
            stated_interval,
            label_not_end,
            unrecognized,
        })
    }

    /// The CRS a variable states. Never assumed: with none stated, the frame's CRS is undefined,
    /// and the importer refuses to warp it or to name it in a record.
    fn crs_of(&self, array: &Array) -> Result<Crs, VortexError> {
        let what = format!("Zarr variable {}", array.name);
        let geographic = |crs: Crs| -> Result<Crs, VortexError> {
            if CrsDef::from_frame(&crs)?.is_geographic() {
                Ok(crs)
            } else {
                Err(refused(
                    what.clone(),
                    "its axes are latitude and longitude, and its CRS is projected",
                ))
            }
        };
        let from_text = |text: &str| -> Result<Crs, VortexError> {
            let text = text.trim();
            if text.eq_ignore_ascii_case("EPSG:4326") {
                return geographic(Crs::Wkt(EPSG_4326_WKT.to_string()));
            }
            let head = text.get(..6).unwrap_or("").to_ascii_uppercase();
            if head == "GEOGCS" || head == "PROJCS" {
                return geographic(Crs::Wkt(text.to_string()));
            }
            Err(refused(
                what.clone(),
                format!("its CRS {text:?} has no rule here; EPSG:4326 and WKT1 do"),
            ))
        };
        if let Some(value) = array.attr("crs") {
            return match value {
                Json::Text(text) => from_text(text),
                other => Err(refused(what, format!("its crs attribute is {other:?}"))),
            };
        }
        if let Some(name) = array.text_attr("grid_mapping") {
            let mapping = self
                .attributes
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, attrs)| attrs)
                .ok_or_else(|| {
                    refused(what.clone(), format!("its grid_mapping {name} is absent"))
                })?;
            let text_of = |key: &str| mapping.get(key).and_then(Json::as_text);
            return match text_of("crs_wkt").or_else(|| text_of("spatial_ref")) {
                Some(text) => from_text(text),
                None => Err(refused(
                    what,
                    format!("its grid_mapping {name} has no crs_wkt or spatial_ref"),
                )),
            };
        }
        Ok(Crs::Undefined)
    }
}

impl Variable<'_> {
    fn array(&self) -> &Array {
        &self.store.arrays[self.r.array]
    }

    pub fn name(&self) -> &str {
        &self.array().name
    }

    /// Steps: the time axis's length, or one for a variable with no time.
    pub fn step_count(&self) -> usize {
        if self.r.has_time {
            self.r.instants.len()
        } else {
            1
        }
    }

    /// The whole grid.
    pub fn full_window(&self) -> Window {
        Window {
            rows: 0..self.r.lat.len(),
            cols: 0..self.r.lon.len(),
        }
    }

    pub fn units(&self) -> &str {
        &self.r.units
    }

    pub fn description(&self) -> Option<&str> {
        self.r.description.as_deref()
    }

    pub fn crs(&self) -> &Crs {
        &self.r.crs
    }

    /// The marker missing cells are warped with, or `None` when the store states no fill.
    pub fn nodata(&self) -> Option<f32> {
        self.r.enhance.nodata()
    }

    /// Seconds each step is accumulated over, ending at its time, when the store states it.
    pub fn stated_accumulation(&self) -> Option<i64> {
        self.r.accumulation
    }

    /// Why a variable that claims to be AORC's precipitation has no period of its own, if it
    /// claims so and has none.
    pub fn period_note(&self) -> Option<&str> {
        self.r.unrecognized.as_deref()
    }

    /// Seconds since 1970 of a step's time.
    pub fn instant(&self, step: usize) -> Result<Option<i64>, VortexError> {
        if !self.r.has_time && step == 0 {
            return Ok(None);
        }
        self.r.instants.get(step).copied().map(Some).ok_or_else(|| {
            refused(
                format!("Zarr variable {}", self.name()),
                format!("step {step} of {}", self.step_count()),
            )
        })
    }

    /// A step's time: the store's own period where it states one, else its instant.
    pub fn time(&self, step: usize) -> Result<FrameTime, VortexError> {
        self.time_over(step, self.r.accumulation)
    }

    /// A step's time as a period of `seconds` ending at its instant, or the instant for `None`.
    pub fn time_over(&self, step: usize, seconds: Option<i64>) -> Result<FrameTime, VortexError> {
        let Some(end) = self.instant(step)? else {
            return Ok(FrameTime::Invariant);
        };
        // HEC julian days: `dss_core::time::ymd_to_julian(1970, 1, 1)` is 25568.
        let split = |t: i64| (t.div_euclid(86_400) + 25_568, t.rem_euclid(86_400));
        Ok(match seconds {
            Some(seconds) => FrameTime::Period {
                start: split(end - seconds),
                end: split(end),
            },
            None => FrameTime::Instant(split(end)),
        })
    }

    /// The cells whose extents a box touches: rows and columns whose edges overlap it. Edges are
    /// the whole axis's ([`make_edges`]). A box that misses the grid is refused.
    pub fn window_for_bbox(&self, bbox: &Bbox) -> Result<Window, VortexError> {
        let what = || format!("Zarr variable {}", self.name());
        let all = [bbox.west, bbox.south, bbox.east, bbox.north];
        if !all.iter().all(|v| v.is_finite()) || bbox.west >= bbox.east || bbox.south >= bbox.north
        {
            return Err(refused(what(), format!("{bbox:?} is not a box")));
        }
        let span = |coords: &[f64], low: f64, high: f64| -> Option<Range<usize>> {
            let edges = make_edges(coords);
            let touched = |i: usize| {
                let (a, b) = (edges[i].min(edges[i + 1]), edges[i].max(edges[i + 1]));
                a < high && b > low
            };
            let first = (0..coords.len()).find(|i| touched(*i))?;
            let last = (0..coords.len()).rev().find(|i| touched(*i))?;
            Some(first..last + 1)
        };
        let rows = span(&self.r.lat, bbox.south, bbox.north);
        let cols = span(&self.r.lon, bbox.west, bbox.east);
        match (rows, cols) {
            (Some(rows), Some(cols)) => Ok(Window { rows, cols }),
            _ => Err(refused(what(), format!("{bbox:?} does not touch its grid"))),
        }
    }

    fn check_window(&self, window: &Window) -> Result<(usize, usize), VortexError> {
        let what = || format!("Zarr variable {}", self.name());
        let (rows, cols) = (&window.rows, &window.cols);
        if rows.start >= rows.end
            || cols.start >= cols.end
            || rows.end > self.r.lat.len()
            || cols.end > self.r.lon.len()
        {
            return Err(refused(
                what(),
                format!(
                    "window rows {rows:?}, columns {cols:?} is empty or outside its {} x {} grid",
                    self.r.lon.len(),
                    self.r.lat.len()
                ),
            ));
        }
        let (ny, nx) = (rows.end - rows.start, cols.end - cols.start);
        match nx.checked_mul(ny) {
            Some(cells) if cells <= self.store.limits.max_cells => Ok((nx, ny)),
            _ => Err(refused(
                what(),
                format!(
                    "{nx} x {ny} is over the limit of {} cells (max_cells). Read a smaller window",
                    self.store.limits.max_cells
                ),
            )),
        }
    }

    /// A window's grid: `(nx, ny, transform)`, by `GridDatasetReader.getGridDefinition`'s rule on
    /// the window's own coordinates, as if they were the whole file's.
    pub fn grid(&self, window: &Window) -> Result<(usize, usize, GeoTransform), VortexError> {
        let (nx, ny) = self.check_window(window)?;
        if nx < 2 || ny < 2 {
            return Err(refused(
                format!("Zarr variable {}", self.name()),
                "a window one cell wide or high has no cell size",
            ));
        }
        let x = make_edges(&self.r.lon[window.cols.clone()]);
        let y = make_edges(&self.r.lat[window.rows.clone()]);
        let dx = (x[nx] - x[0]) / nx as f64;
        let dy = (y[ny] - y[0]) / ny as f64;
        Ok((nx, ny, GeoTransform::axis_aligned(x[0], dx, y[0], dy)))
    }

    fn ranges(
        &self,
        steps: &Range<usize>,
        window: &Window,
    ) -> Result<Vec<Range<u64>>, VortexError> {
        if steps.start >= steps.end || steps.end > self.step_count() {
            return Err(refused(
                format!("Zarr variable {}", self.name()),
                format!(
                    "steps {steps:?} is empty or outside its {}",
                    self.step_count()
                ),
            ));
        }
        let mut ranges = Vec::with_capacity(3);
        if self.r.has_time {
            ranges.push(steps.start as u64..steps.end as u64);
        }
        ranges.push(window.rows.start as u64..window.rows.end as u64);
        ranges.push(window.cols.start as u64..window.cols.end as u64);
        Ok(ranges)
    }

    fn read_size(
        &self,
        steps: &Range<usize>,
        window: &Window,
        item: usize,
    ) -> Result<usize, VortexError> {
        let (nx, ny) = self.check_window(window)?;
        let count = nx
            .checked_mul(ny)
            .and_then(|c| c.checked_mul(steps.end.saturating_sub(steps.start)));
        match count {
            Some(c)
                if (c as u64).saturating_mul(item as u64) <= self.store.limits.max_read_bytes =>
            {
                Ok(c)
            }
            _ => Err(refused(
                format!("Zarr variable {}", self.name()),
                format!(
                    "the read is over the limit of {} bytes (max_read_bytes). Read fewer steps or \
                     a smaller window",
                    self.store.limits.max_read_bytes
                ),
            )),
        }
    }

    /// Cells of a window over a run of steps: step after step, each row after row in the store's
    /// order. Missing cells are NaN.
    pub fn read(&self, steps: Range<usize>, window: &Window) -> Result<Vec<f32>, VortexError> {
        catch_unwind(AssertUnwindSafe(|| self.read_cells(&steps, window)))
            .unwrap_or_else(|_| Err(decode_error("this reader panicked on this store")))
    }

    fn read_cells(&self, steps: &Range<usize>, window: &Window) -> Result<Vec<f32>, VortexError> {
        let what = format!("Zarr variable {}", self.name());
        let count = self.read_size(steps, window, 4)?;
        let ranges = self.ranges(steps, window)?;
        let mut cells: Vec<f32> = Vec::new();
        cells
            .try_reserve_exact(count)
            .map_err(|_| refused(what.clone(), format!("could not allocate {count} cells")))?;
        cells.resize(count, f32::NAN);
        let dtype = self.array().dtype;
        let item = dtype.size();
        let enhance = &self.r.enhance;
        let marker = enhance.nodata().filter(|m| !m.is_nan()).map(f32::to_bits);
        self.store.visit(self.r.array, &ranges, |at, n, bytes| {
            // No file: missing, whatever fill the array states. See the module's documentation.
            let Some(bytes) = bytes else {
                return Ok(());
            };
            for (cell, stored) in cells[at..at + n].iter_mut().zip(bytes.chunks_exact(item)) {
                let Some(stored) = dtype.value(stored) else {
                    return Err(decode_error("a cell with no exact double"));
                };
                if enhance.mask.is_missing(stored) || stored.is_nan() {
                    continue;
                }
                let value = enhance.value(stored);
                if Some(value.to_bits()) == marker {
                    return Err(refused(
                        what.clone(),
                        format!(
                            "the valid stored value {stored} becomes {value}, the marker its \
                             missing cells have, and would read as missing"
                        ),
                    ));
                }
                *cell = value;
            }
            Ok(())
        })?;
        Ok(cells)
    }

    /// The stored items of a window, little-endian, in the same order as [`Self::read`], for
    /// comparison with another reader. No mask, scale or offset. A chunk with no file is the
    /// array's `fill_value`, and an error when that is `null`.
    pub fn read_stored(
        &self,
        steps: Range<usize>,
        window: &Window,
    ) -> Result<Vec<u8>, VortexError> {
        catch_unwind(AssertUnwindSafe(|| self.stored(&steps, window)))
            .unwrap_or_else(|_| Err(decode_error("this reader panicked on this store")))
    }

    fn stored(&self, steps: &Range<usize>, window: &Window) -> Result<Vec<u8>, VortexError> {
        let what = format!("Zarr variable {}", self.name());
        let array = self.array();
        let dtype = array.dtype;
        let item = dtype.size();
        let count = self.read_size(steps, window, item)?;
        let ranges = self.ranges(steps, window)?;
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(count * item)
            .map_err(|_| refused(what.clone(), format!("could not allocate {count} items")))?;
        out.resize(count * item, 0);
        self.store.visit(self.r.array, &ranges, |at, n, bytes| {
            let bytes = bytes.ok_or_else(|| {
                refused(
                    what.clone(),
                    "a chunk in the window has no file, so it has no stored bytes",
                )
            })?;
            let target = &mut out[at * item..(at + n) * item];
            target.copy_from_slice(&bytes[..n * item]);
            if dtype.big_endian {
                for one in target.chunks_exact_mut(item) {
                    one.reverse();
                }
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// One step of a window as a frame, with the time [`Self::time`] gives.
    pub fn frame(&self, step: usize, window: &Window) -> Result<GridFrame, VortexError> {
        self.frame_over(step, window, self.r.accumulation)
    }

    /// One step of a window as a frame, over `seconds` ending at its time.
    pub fn frame_over(
        &self,
        step: usize,
        window: &Window,
        seconds: Option<i64>,
    ) -> Result<GridFrame, VortexError> {
        let (nx, ny, transform) = self.grid(window)?;
        let time = self.time_over(step, seconds)?;
        let cells = self.read(step..step + 1, window)?;
        Ok(GridFrame::new(GridFrameParts {
            cells,
            nx,
            ny,
            row_order: if transform.dy < 0.0 {
                RowOrder::NorthToSouth
            } else {
                RowOrder::SouthToNorth
            },
            transform,
            crs: self.r.crs.clone(),
            nodata: self.nodata(),
            time,
            data_type: type_for(&time),
            units: self.r.units.clone(),
            variable: self.name().to_string(),
            description: self.r.description.clone(),
        })?)
    }
}

/// A period here is always an accumulation ending at its stamp; anything else is an instant.
fn type_for(time: &FrameTime) -> DataType {
    match time {
        FrameTime::Period { .. } => DataType::PerCum,
        _ => DataType::InstVal,
    }
}

/// A store as the importer sees it: every variable on one window.
pub struct ZarrSource {
    store: ZarrStore,
    chosen: Mutex<StoreSelection>,
    rule: WindowRule,
    /// Per variable, the store's steps inside `times`.
    selected: Mutex<Vec<(String, Arc<Vec<usize>>)>>,
}

/// What of a store an import takes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StoreSelection {
    /// Only the cells this box touches, in the store's CRS. `None` is the whole grid.
    pub bbox: Option<Bbox>,
    /// Only steps whose time T has `first <= T <= last`, in seconds since 1970 UTC, as
    /// `xarray`'s `sel(time=slice(first, last))` selects. For an accumulation, T is its end.
    pub times: Option<(i64, i64)>,
}

impl ZarrSource {
    pub fn new(store: ZarrStore, rule: WindowRule) -> Self {
        ZarrSource {
            store,
            chosen: Mutex::new(StoreSelection::default()),
            rule,
            selected: Mutex::new(Vec::new()),
        }
    }

    fn chosen(&self) -> StoreSelection {
        self.chosen.lock().map(|c| *c).unwrap_or_default()
    }

    fn choose(&self, change: impl FnOnce(&mut StoreSelection)) {
        if let Ok(mut chosen) = self.chosen.lock() {
            let before = *chosen;
            change(&mut chosen);
            if chosen.times != before.times {
                if let Ok(mut cache) = self.selected.lock() {
                    cache.clear();
                }
            }
        }
    }

    /// Offer only the steps whose time is in `first..=last`, seconds since 1970. The importer
    /// then numbers steps within that selection.
    pub fn with_times(self, times: Option<(i64, i64)>) -> Self {
        self.choose(|c| c.times = times);
        self
    }

    /// The store's steps offered for a variable, or `None` for all of them.
    fn steps_of(&self, variable: &Variable<'_>) -> Result<Option<Arc<Vec<usize>>>, VortexError> {
        let Some((first, last)) = self.chosen().times else {
            return Ok(None);
        };
        let known = self.selected.lock().ok().and_then(|cache| {
            cache
                .iter()
                .find(|(name, _)| name == variable.name())
                .map(|(_, steps)| steps.clone())
        });
        if let Some(steps) = known {
            return Ok(Some(steps));
        }
        let what = || format!("Zarr variable {}", variable.name());
        let mut steps: Vec<usize> = Vec::new();
        for step in 0..variable.step_count() {
            let instant = variable.instant(step)?.ok_or_else(|| {
                refused(what(), "it has no time axis, and a time range was stated")
            })?;
            if (first..=last).contains(&instant) {
                steps
                    .try_reserve(1)
                    .map_err(|_| refused(what(), "could not allocate its steps"))?;
                steps.push(step);
            }
        }
        let steps = Arc::new(steps);
        if let Ok(mut cache) = self.selected.lock() {
            cache.push((variable.name().to_string(), steps.clone()));
        }
        Ok(Some(steps))
    }

    /// The store's step for one of the importer's.
    fn step(&self, variable: &Variable<'_>, step: usize) -> Result<usize, VortexError> {
        match self.steps_of(variable)? {
            None => Ok(step),
            Some(steps) => steps.get(step).copied().ok_or_else(|| {
                refused(
                    format!("Zarr variable {}", variable.name()),
                    format!("step {step} of the {} in the time range", steps.len()),
                )
            }),
        }
    }

    /// Read only the cells a box touches, in the store's CRS, instead of the whole grid.
    pub fn with_bbox(self, bbox: Option<Bbox>) -> Self {
        self.choose(|c| c.bbox = bbox);
        self
    }

    pub fn store(&self) -> &ZarrStore {
        &self.store
    }

    fn window(&self, variable: &Variable<'_>) -> Result<Window, VortexError> {
        match &self.chosen().bbox {
            Some(bbox) => variable.window_for_bbox(bbox),
            None => Ok(variable.full_window()),
        }
    }

    /// The box of the store a destination envelope needs: the envelope in the store's CRS, and
    /// around it four cells of the coarser of the two grids plus two of the store's, which is
    /// more than any kernel here reaches (bilinear one source cell, average half a destination
    /// cell). `None` when it cannot be worked out, and then the whole grid is read.
    fn box_for(
        variable: &Variable<'_>,
        envelope: &crate::warp::WarpEnvelope,
        cell_size: Option<f64>,
    ) -> Option<Bbox> {
        let source = CrsDef::from_frame(variable.crs()).ok()?;
        let e = envelope.envelope;
        let (mut west, mut east, mut south, mut north) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        match &envelope.crs {
            None => (west, east, south, north) = (e.min_x, e.max_x, e.min_y, e.max_y),
            Some(crs) => {
                let to_source =
                    crate::crs::Transformer::new(crs, &source, crate::crs::DatumPolicy::default())
                        .ok()?;
                // A lattice, not only the corners: an edge bows in another projection.
                const STEPS: usize = 32;
                for i in 0..=STEPS {
                    for j in 0..=STEPS {
                        let x = e.min_x + (e.max_x - e.min_x) * i as f64 / STEPS as f64;
                        let y = e.min_y + (e.max_y - e.min_y) * j as f64 / STEPS as f64;
                        let (lon, lat) = to_source.transform(x, y).ok()?;
                        if !lon.is_finite() || !lat.is_finite() {
                            return None;
                        }
                        west = west.min(lon);
                        east = east.max(lon);
                        south = south.min(lat);
                        north = north.max(lat);
                    }
                }
            }
        }
        let (_, _, grid) = variable.grid(&variable.full_window()).ok()?;
        let (sx, sy) = (grid.dx.abs(), grid.dy.abs());
        // A destination cell, in the store's units.
        let (dx, dy) = match cell_size.filter(|c| c.is_finite() && *c > 0.0) {
            Some(cell) => (
                (east - west) / ((e.max_x - e.min_x) / cell).max(1.0),
                (north - south) / ((e.max_y - e.min_y) / cell).max(1.0),
            ),
            None => (sx, sy),
        };
        let (pad_x, pad_y) = (4.0 * sx.max(dx) + 2.0 * sx, 4.0 * sy.max(dy) + 2.0 * sy);
        let bbox = Bbox {
            west: west - pad_x,
            south: south - pad_y,
            east: east + pad_x,
            north: north + pad_y,
        };
        [bbox.west, bbox.south, bbox.east, bbox.north]
            .iter()
            .all(|v| v.is_finite())
            .then_some(bbox)
    }

    /// The seconds a step is accumulated over: the store's own, or the caller's where the store
    /// states none. A caller's that contradicts what the store states is refused, and so is a
    /// variable that claims to be AORC's and is not recognized, with what to state instead.
    fn accumulation(&self, variable: &Variable<'_>) -> Result<Option<i64>, VortexError> {
        period_for(self.rule, variable)
    }
}

/// See [`ZarrSource::accumulation`].
fn period_for(rule: WindowRule, variable: &Variable<'_>) -> Result<Option<i64>, VortexError> {
    {
        let stated = variable.stated_accumulation();
        match rule {
            WindowRule::Accumulation(wanted) => {
                let what = || format!("accumulation {} of {}", wanted.name(), variable.name());
                if let Some(why) = &variable.r.label_not_end {
                    return Err(refused(what(), why.clone()));
                }
                match stated.or(variable.r.stated_interval) {
                    Some(seconds) if seconds != wanted.seconds() => Err(refused(
                        what(),
                        format!("the store states {seconds} s for it"),
                    )),
                    _ => Ok(Some(wanted.seconds())),
                }
            }
            _ => match (stated, variable.period_note()) {
                (None, Some(why)) => Err(refused(
                    format!("period of {}", variable.name()),
                    format!(
                        "{why}, so it is not read as AORC v1.1's hour ending at each time. If its \
                         values are depths over a period ending at each time, state window_rule \
                         accumulation:<duration>"
                    ),
                )),
                _ => Ok(stated),
            },
        }
    }
}

impl GridSource for ZarrSource {
    fn file_name(&self) -> &str {
        self.store.file_name()
    }

    fn variables(&self) -> Vec<String> {
        self.store.variable_names()
    }

    fn left_out(&self) -> Vec<String> {
        self.store.left_out().to_vec()
    }

    fn step_count(&self, variable: &str) -> Result<usize, VortexError> {
        let v = self.store.variable(variable)?;
        let steps = match self.steps_of(&v)? {
            Some(steps) => steps.len(),
            None => v.step_count(),
        };
        let window = self.window(&v)?;
        let (ny, nx) = (
            window.rows.end - window.rows.start,
            window.cols.end - window.cols.start,
        );
        let cells = (steps as u64)
            .saturating_mul(nx as u64)
            .saturating_mul(ny as u64);
        let most = self.store.limits.max_cells_per_import;
        if cells > most {
            return Err(refused(
                format!("import of {variable}"),
                format!(
                    "it would read {steps} steps of {nx} x {ny} cells, {cells} cells, over the \
                     limit of {most} (max_cells_per_import). State times, the first and last to \
                     read; and a bbox, or an envelope, which reads only the cells it needs"
                ),
            ));
        }
        Ok(steps)
    }

    fn select(
        &self,
        variable: &str,
        selection: &crate::import::SourceSelection,
    ) -> Result<(), VortexError> {
        let v = self.store.variable(variable)?;
        let stated = selection.bbox.map(|[west, south, east, north]| Bbox {
            west,
            south,
            east,
            north,
        });
        // The caller's box stands. With none, the envelope's own, padded for the warp.
        let from_envelope = || {
            selection
                .envelope
                .as_ref()
                .and_then(|e| Self::box_for(&v, e, selection.cell_size))
        };
        self.choose(|c| {
            if let Some(times) = selection.times {
                c.times = Some(times);
            }
            if stated.is_some() {
                c.bbox = stated;
            } else if c.bbox.is_none() {
                c.bbox = from_envelope();
            }
        });
        Ok(())
    }

    fn step_meta(&self, variable: &str, step: usize) -> Result<StepMeta, VortexError> {
        let v = self.store.variable(variable)?;
        let time = v.time_over(self.step(&v, step)?, self.accumulation(&v)?)?;
        Ok(StepMeta {
            time,
            units: v.units().to_string(),
            description: v.description().map(str::to_string),
            source_type: type_for(&time),
        })
    }

    fn grid(&self, variable: &str, _step: usize) -> Result<SourceGridDef, VortexError> {
        let v = self.store.variable(variable)?;
        let (nx, ny, transform) = v.grid(&self.window(&v)?)?;
        Ok(SourceGridDef {
            nx,
            ny,
            transform,
            crs: v.crs().clone(),
            nodata: v.nodata(),
            stored_wkt: None,
        })
    }

    fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        let v = self.store.variable(variable)?;
        v.frame_over(
            self.step(&v, step)?,
            &self.window(&v)?,
            self.accumulation(&v)?,
        )
    }

    fn accumulation_stated_by(
        &self,
        variable: &str,
        wanted: Accumulation,
    ) -> Option<Result<&'static str, VortexError>> {
        let result = self.store.variable(variable).and_then(|v| {
            period_for(WindowRule::Accumulation(wanted), &v)?;
            Ok(if v.stated_accumulation() == Some(wanted.seconds()) {
                "store"
            } else {
                "caller"
            })
        });
        Some(result)
    }
}

/// [`Limits`] from the importer's.
pub fn limits_from(options: &OpenOptions) -> Limits {
    Limits {
        max_cells: options.limits.max_cells,
        ..Limits::default()
    }
}

/// Open a store for `import::open_source`: every variable, on its whole grid until the importer
/// selects from it ([`GridSource::select`]).
pub fn open_source(path: &Path, options: &OpenOptions) -> Result<Box<dyn GridSource>, VortexError> {
    open_source_with(path, options, None, None)
}

/// [`open_source`] with a request's own limits and stop flag.
pub fn open_source_with(
    path: &Path,
    options: &OpenOptions,
    limits: Option<Limits>,
    stop: Option<Arc<AtomicBool>>,
) -> Result<Box<dyn GridSource>, VortexError> {
    let limits = limits.unwrap_or_else(|| limits_from(options));
    let store = ZarrStore::open_with(path, limits, stop)?;
    Ok(Box::new(ZarrSource::new(store, options.window_rule)))
}

/// Import stores, taking only the box and the times `selection` states: `import::import_gridded`
/// with `source_bbox` and `source_times` set, which is the one route.
// `ImportError` carries the writer's lock report and every finished record, as in `import`.
#[allow(clippy::result_large_err)]
pub fn import_stores(
    request: &crate::import::ImportRequest,
    selection: &StoreSelection,
    progress: &mut dyn FnMut(&crate::import::Progress) -> std::ops::ControlFlow<()>,
) -> Result<Vec<crate::import::ImportedRecord>, crate::import::ImportError> {
    let mut request = request.clone();
    if let Some(b) = selection.bbox {
        request.source_bbox = Some([b.west, b.south, b.east, b.north]);
    }
    if selection.times.is_some() {
        request.source_times = selection.times;
    }
    crate::import::import_gridded(&request, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_could_leave_the_store_are_not_names() {
        for good in ["APCP_surface", "time", "a.b", "TMP 2m", "x-1"] {
            assert!(safe_name(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "C:x",
            ".zarray",
            "NUL",
            "nul",
            "con.txt",
            "COM1",
            "x.",
            "x ",
            "a\0b",
            "a\nb",
            "a*b",
            "LPT9.zarr",
        ] {
            assert!(!safe_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn data_types_read_both_byte_orders() {
        let le = Dtype::parse("<i2").unwrap();
        let be = Dtype::parse(">i2").unwrap();
        assert_eq!(le.value(&[0x01, 0x80]), Some(-32767.0));
        assert_eq!(be.value(&[0x80, 0x01]), Some(-32767.0));
        assert_eq!(Dtype::parse("|u1").unwrap().value(&[0xFF]), Some(255.0));
        assert_eq!(Dtype::parse("|i1").unwrap().value(&[0xFF]), Some(-1.0));
        assert_eq!(
            Dtype::parse(">f4").unwrap().value(&1.5f32.to_be_bytes()),
            Some(1.5)
        );
        assert_eq!(
            Dtype::parse("<f8").unwrap().value(&(-0.1f64).to_le_bytes()),
            Some(-0.1)
        );
        assert_eq!(
            Dtype::parse("<u4").unwrap().value(&u32::MAX.to_le_bytes()),
            Some(4294967295.0)
        );
        assert_eq!(
            Dtype::parse(">i8").unwrap().value(&(-5i64).to_be_bytes()),
            Some(-5.0)
        );
        // No exact double.
        assert_eq!(
            Dtype::parse("<u8").unwrap().value(&u64::MAX.to_le_bytes()),
            None
        );
        for bad in [
            "i2", "|i2", "=i2", "<i3", "<c8", "<U4", "|b1", "<M8[ns]", "",
        ] {
            assert!(Dtype::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn time_units_read_as_cf_states_them() {
        assert_eq!(parse_time_units("seconds since 1970-01-01"), Ok((1, 0)));
        assert_eq!(
            parse_time_units("hours since 1970-01-02 00:00:00"),
            Ok((3600, 86_400))
        );
        assert_eq!(
            parse_time_units("days since 1970-01-01T06:30:00Z"),
            Ok((86_400, 23_400))
        );
        assert_eq!(
            parse_time_units("minutes since 1969-12-31 23:00:00.0 UTC"),
            Ok((60, -3600))
        );
        assert_eq!(
            parse_time_units("Hours since 2000-03-01 00:00:00 +00:00"),
            Ok((3600, 951_868_800))
        );
        for bad in [
            "seconds",
            "fortnights since 1970-01-01",
            "months since 1970-01-01",
            "seconds since 1970-13-01",
            "seconds since 1970-02-30",
            "seconds since 1582-10-15",
            "seconds since 1970-01-01 00:00:00.5",
            "seconds since 1970-01-01 00:00:00 -05:00",
            "seconds since 1970-01-01 25:00",
            "seconds since yesterday",
        ] {
            assert!(parse_time_units(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn epsg_4326_is_a_geographic_crs_the_crs_module_reads() {
        let def = CrsDef::from_frame(&Crs::Wkt(EPSG_4326_WKT.to_string())).unwrap();
        assert!(def.is_geographic());
    }

    #[test]
    fn edges_and_regularity_follow_netcdf_java() {
        assert_eq!(make_edges(&[0.0, 1.0, 2.0]), vec![-0.5, 0.5, 1.5, 2.5]);
        assert_eq!(make_edges(&[2.0, 1.0]), vec![2.5, 1.5, 0.5]);
        assert!(is_regular(&[0.0, 1.0, 2.0, 3.0]));
        assert!(!is_regular(&[0.0, 1.0, 2.0, 4.0]));
        assert!(!is_regular(&[1.0, 1.0, 1.0]));
    }
}

//! Frames to GeoTIFF and ASCII grid files.
//!
//! **Atomic.** Each file is written to a temporary name beside the target, flushed, read back and
//! compared with the frame, synced, then renamed over the target. A refused or failed write leaves
//! an existing target byte-identical, and leaves no other file behind. A crash can leave
//! `.<name>.<pid>.<n>.tmp` or `.old`, which do not pass as a raster: Vortex's reader and this
//! crate's look at the target's name. An ASCII grid is two files; see [`write_asc`].
//!
//! **Read back.** What this crate reads from the temporary file must equal the frame in cell bits,
//! mask, all six transform coefficients, nodata and CRS, or the write is refused.
//!
//! Vortex writes with `gdal.Translate` (`io/TiffDataWriter`, `io/AscDataWriter`).
//! [`TiffOptions::vortex`] and [`AscOptions::vortex`] apply its rules. **Observed behaviour we did
//! not implement:** with `FORCE_CELLSIZE=TRUE`, Vortex 1.1.0's ASCII grid of a grid with unequal
//! `dx` and `dy` states `dx` as the cell size for both, by `AscDataWriter` and GDAL's `CreateCopy`
//! as read; not run. The default writes `dx` and `dy`, and [`AscOptions::vortex`] refuses.
//! Vortex's GeoTIFF is tiled; this crate's encoder writes strips.
//!
//! **Allocation.** A failed allocation sized by the grid is a typed error, including in the encoder,
//! which is this crate's own; see [`encoder`]. Small, bounded requests remain before the first byte:
//! the CRS text (at most 64 KiB, refused above that) and `flate2`'s fixed-size compressor state.

mod encoder;

use crate::crs::CrsDef;
use crate::error::VortexError;
use crate::frame::{Crs, GeoTransform, GridFrame, RowOrder};
use crate::source::raster::geokeys;
use crate::source::raster::{alloc_zeroed, applied_sidecar, CrsStatus, RasterFile};
use crate::source::raster::{format_g20, write_g20, NumberText};
use encoder::Value;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// What to do with the frame's missing marker.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum NodataPolicy {
    /// Keep the frame's cells and marker.
    #[default]
    Keep,
    /// `RasterUtils.resetNoDataValue`: cells equal to the frame's marker, by `Double.compare`,
    /// become this value, which is then the marker. With no marker in the frame, a cell already
    /// holding this value then reads as missing.
    Reset(f32),
}

/// GeoTIFF options. The default keeps the frame's marker in a deflated BigTIFF.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TiffOptions {
    pub nodata: NodataPolicy,
    pub bigtiff: bool,
    /// Deflate, at level 1 as Vortex's `ZLEVEL=1`. Without it, uncompressed.
    pub deflate: bool,
}

impl Default for TiffOptions {
    fn default() -> Self {
        TiffOptions {
            nodata: NodataPolicy::Keep,
            bigtiff: true,
            deflate: true,
        }
    }
}

impl TiffOptions {
    /// `TiffDataWriter`: float32, deflate at level 1, BigTIFF, nodata reset to -9999.
    pub fn vortex() -> Self {
        TiffOptions {
            nodata: NodataPolicy::Reset(-9999.0),
            ..TiffOptions::default()
        }
    }
}

/// How an ASCII grid states its corner and cell size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AscGeoref {
    /// `%.12f` when it reads back exactly, and otherwise enough digits that it does. A corner that
    /// no text can bring back to the frame's origin is refused.
    #[default]
    RoundTrip,
    /// GDAL's `%.12f` always, so the text matches Vortex's byte for byte. It can change the
    /// transform below its twelfth decimal.
    Vortex,
}

/// ASCII grid options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AscOptions {
    pub nodata: AscNodata,
    pub georef: AscGeoref,
}

/// [`NodataPolicy`] for an ASCII grid, whose options stay `Eq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AscNodata {
    #[default]
    Keep,
    /// As [`NodataPolicy::Reset`], with -9999.
    ResetToVortex,
}

impl AscOptions {
    /// `AscDataWriter`: nodata reset to -9999, GDAL's text, and no unequal cell sizes.
    pub fn vortex() -> Self {
        AscOptions {
            nodata: AscNodata::ResetToVortex,
            georef: AscGeoref::Vortex,
        }
    }
}

fn refuse(what: &str, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.to_string(),
        reason: reason.into(),
    }
}

/// `Double.compare(data[i], noDataValueIn) == 0`, on the widened values.
fn same_as_java(cell: f32, marker: f32) -> bool {
    let (a, b) = (f64::from(cell), f64::from(marker));
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

/// The cells to write and their marker. Copied fallibly: a grid can be near the reader's limit.
fn apply_nodata(
    cells: &[f32],
    nodata: Option<f32>,
    policy: NodataPolicy,
) -> Result<(Vec<f32>, Option<f32>), VortexError> {
    let mut out: Vec<f32> = alloc_zeroed(cells.len(), "raster write")?;
    out.copy_from_slice(cells);
    match policy {
        NodataPolicy::Keep => Ok((out, nodata)),
        NodataPolicy::Reset(marker) => {
            if let Some(old) = nodata {
                for cell in &mut out {
                    if same_as_java(*cell, old) {
                        *cell = marker;
                    }
                }
            }
            Ok((out, Some(marker)))
        }
    }
}

/// `RasterUtils.flipVertically`: rows in the other order, bits unchanged, allocated fallibly.
fn flipped(cells: &[f32], nx: usize) -> Result<Vec<f32>, VortexError> {
    let mut out: Vec<f32> = alloc_zeroed(cells.len(), "raster write")?;
    for (to, from) in out.chunks_exact_mut(nx).zip(cells.chunks_exact(nx).rev()) {
        to.copy_from_slice(from);
    }
    Ok(out)
}

/// The text of a nodata value, which GDAL and this crate read as a double and narrow. The widened
/// float32 prints exactly enough digits to come back to the same bits.
fn nodata_text(value: f32) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    format!("{}", f64::from(value))
}

/// One step of a write that can fail on its own, for tests that make each one fail in turn.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    CreateTemp,
    WriteData,
    Flush,
    ReadBack,
    SyncTemp,
    CreatePrjTemp,
    WritePrjTemp,
    /// Move the existing grid aside.
    MoveOldGrid,
    /// Move the existing `.prj` aside.
    MoveOldPrj,
    RenamePrj,
    RenameGrid,
    /// Undoing a failed step: remove the new `.prj`.
    RemoveNewPrj,
    /// Undoing a failed step: put the old `.prj` back.
    RestorePrj,
    /// Undoing a failed step: put the old grid back.
    RestoreGrid,
    /// The new files are in place; remove what they replaced.
    RemoveOld,
}

/// Called before each step; an error stands in for that step's own failure.
type Faults<'a> = &'a mut dyn FnMut(WriteStep) -> std::io::Result<()>;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name beside `target` that is not a raster's: `.<name>.<pid>.<n>.<suffix>`.
fn sibling(target: &Path, suffix: &str) -> Result<PathBuf, VortexError> {
    let name = target
        .file_name()
        .ok_or_else(|| refuse("raster write", "the target has no file name"))?
        .to_string_lossy()
        .into_owned();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(target.with_file_name(format!(".{name}.{}.{n}.{suffix}", std::process::id())))
}

/// A temporary file beside a target, removed on drop unless it was renamed.
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    fn beside(target: &Path) -> Result<(Self, File), VortexError> {
        let path = sibling(target, "tmp")?;
        let file = File::options()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)?;
        Ok((TempFile { path, armed: true }, file))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Make a rename durable where a directory can be synced. Windows cannot open one this way, and
/// `MoveFileEx` there is not made durable by this crate.
fn sync_parent(target: &Path) {
    #[cfg(unix)]
    if let Some(parent) = target.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = target;
}

/// What the read-back must find, compared bit for bit.
#[allow(clippy::too_many_arguments)]
fn check_read_back(
    read: &RasterFile,
    (nx, ny): (usize, usize),
    transform: &GeoTransform,
    cells: &[f32],
    nodata: Option<f32>,
    expected_crs: &Crs,
    transform_is_exact: bool,
    canonical_nan: bool,
) -> Result<(), VortexError> {
    let got = read.frame().parts();
    let fail = |what: &str| {
        Err(refuse(
            "raster write",
            format!("reading the file back gave a different {what}"),
        ))
    };
    if got.nx != nx || got.ny != ny {
        return fail("size");
    }
    if got.cells.len() != cells.len() {
        return fail("cell count");
    }
    let differs = got.cells.iter().zip(cells).any(|(a, b)| {
        if canonical_nan && a.is_nan() && b.is_nan() {
            return false;
        }
        a.to_bits() != b.to_bits()
    });
    if differs {
        return fail("cell");
    }
    let same_nodata = match (got.nodata, nodata) {
        (None, None) => true,
        (Some(a), Some(b)) => a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()),
        _ => false,
    };
    if !same_nodata {
        return fail("nodata value");
    }
    if transform_is_exact && !same_transform(&got.transform, transform) {
        return fail("transform");
    }
    if &got.crs != expected_crs {
        return fail("CRS");
    }
    Ok(())
}

fn same_transform(a: &GeoTransform, b: &GeoTransform) -> bool {
    a.origin_x.to_bits() == b.origin_x.to_bits()
        && a.dx.to_bits() == b.dx.to_bits()
        && a.row_rotation.to_bits() == b.row_rotation.to_bits()
        && a.origin_y.to_bits() == b.origin_y.to_bits()
        && a.column_rotation.to_bits() == b.column_rotation.to_bits()
        && a.dy.to_bits() == b.dy.to_bits()
}

/// What GDAL would apply to the new file from beside it, which this write cannot know or remove.
/// A read-back of the temporary file cannot see it: only the target's name has these sidecars.
fn refuse_stale_sidecar(path: &Path) -> Result<(), VortexError> {
    if path.file_name().is_none() {
        return Err(refuse("raster write", "the target has no file name"));
    }
    if let Some((found, why)) = applied_sidecar(path, true, false)? {
        return Err(refuse(
            "raster write",
            format!("a {found} beside the target: {why}"),
        ));
    }
    Ok(())
}

/// A folder where the file would go would fail at the rename, after everything else.
fn refuse_directory(path: &Path) -> Result<(), VortexError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            format!("{} is a folder", path.display()),
        )
        .into()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The six numbers a transform tag holds, and which tag, in a plain or a matrix form.
fn transform_tags(frame: &GridFrame) -> ([f64; 3], [f64; 6], [f64; 16], bool) {
    let parts = frame.parts();
    let t = parts.transform;
    let plain_north_up = parts.row_order == RowOrder::NorthToSouth
        && t.row_rotation == 0.0
        && t.column_rotation == 0.0
        && t.dx > 0.0
        && t.dy < 0.0;
    // What GDAL writes for what a pixel scale cannot state.
    let matrix = [
        t.dx,
        t.row_rotation,
        0.0,
        t.origin_x,
        t.column_rotation,
        t.dy,
        0.0,
        t.origin_y,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
    ];
    (
        [t.dx, -t.dy, 0.0],
        [0.0, 0.0, 0.0, t.origin_x, t.origin_y, 0.0],
        matrix,
        plain_north_up,
    )
}

/// The file, from cells that have had the nodata policy applied.
fn encode_cells<W: Write + Seek>(
    writer: W,
    frame: &GridFrame,
    cells: &[f32],
    nodata: Option<f32>,
    keys: Option<&geokeys::EncodedKeys>,
    options: &TiffOptions,
) -> Result<(), VortexError> {
    let parts = frame.parts();
    let (scale, tie, matrix, plain) = transform_tags(frame);
    let nodata_text = nodata.map(nodata_text);
    let mut extra: [(u16, Value<'_>); 7] = [(0, Value::Short(0)); 7];
    let mut used = 0;
    let mut add = |tag, value| {
        extra[used] = (tag, value);
        used += 1;
    };
    if plain {
        add(33550, Value::Doubles(&scale));
        add(33922, Value::Doubles(&tie));
    } else {
        add(34264, Value::Doubles(&matrix));
    }
    if let Some(keys) = keys {
        add(34735, Value::Shorts(&keys.directory));
        if !keys.doubles.is_empty() {
            add(34736, Value::Doubles(&keys.doubles));
        }
        if !keys.ascii.is_empty() {
            add(34737, Value::Ascii(&keys.ascii));
        }
    }
    if let Some(text) = &nodata_text {
        add(42113, Value::Ascii(text));
    }
    encoder::encode(
        writer,
        (parts.nx, parts.ny),
        cells,
        &extra[..used],
        options.bigtiff,
        options.deflate,
    )
}

/// The text of an ASCII grid of a north-up frame, into any writer. [`write_asc`] is the route that
/// keeps a target whole.
#[doc(hidden)]
pub fn encode_asc_to<W: Write>(
    mut writer: W,
    frame: &GridFrame,
    georef: AscGeoref,
) -> Result<(), VortexError> {
    let parts = frame.parts();
    write_asc_text(
        &mut writer,
        (parts.nx, parts.ny),
        &parts.transform,
        &parts.cells,
        parts.nodata,
        georef,
    )
}

/// The encoded file, into any writer. [`write_geotiff`] is the route that keeps a target whole.
#[doc(hidden)]
pub fn encode_geotiff_to<W: Write + Seek>(
    writer: W,
    frame: &GridFrame,
    options: &TiffOptions,
) -> Result<(), VortexError> {
    let parts = frame.parts();
    let (cells, nodata) = apply_nodata(&parts.cells, parts.nodata, options.nodata)?;
    let keys = geokeys::encode(&parts.crs)?;
    encode_cells(writer, frame, &cells, nodata, keys.as_ref(), options)
}

/// A GeoTIFF of one frame: float32, with its rows in the frame's order. See the module's notes.
pub fn write_geotiff(
    frame: &GridFrame,
    path: &Path,
    options: &TiffOptions,
) -> Result<(), VortexError> {
    write_geotiff_with_faults(frame, path, options, &mut |_| Ok(()))
}

/// [`write_geotiff`], with a failure injected at a chosen [`WriteStep`]. For tests.
#[doc(hidden)]
pub fn write_geotiff_with_faults(
    frame: &GridFrame,
    path: &Path,
    options: &TiffOptions,
    fault: Faults<'_>,
) -> Result<(), VortexError> {
    // Refusals come before anything is created.
    refuse_stale_sidecar(path)?;
    refuse_directory(path)?;
    let parts = frame.parts();
    let (cells, nodata) = apply_nodata(&parts.cells, parts.nodata, options.nodata)?;
    let keys = geokeys::encode(&parts.crs)?;
    fault(WriteStep::CreateTemp)?;
    let (mut temp, file) = TempFile::beside(path)?;
    {
        let mut out = BufWriter::new(&file);
        fault(WriteStep::WriteData)?;
        encode_cells(&mut out, frame, &cells, nodata, keys.as_ref(), options)?;
        fault(WriteStep::Flush)?;
        out.flush()?;
    }
    fault(WriteStep::ReadBack)?;
    // A neutral name: the target's own only sets metadata from Vortex's file-name rules.
    let bytes = std::fs::read(&temp.path)?;
    let read = RasterFile::from_bytes("read_back.tif", &bytes, None, Default::default())?;
    drop(bytes);
    let expected = geokeys::canonical(parts.crs.clone());
    check_read_back(
        &read,
        (parts.nx, parts.ny),
        &parts.transform,
        &cells,
        nodata,
        &expected,
        true,
        false,
    )?;
    fault(WriteStep::SyncTemp)?;
    file.sync_all()?;
    drop(file);
    fault(WriteStep::RenameGrid)?;
    std::fs::rename(&temp.path, path)?;
    temp.armed = false;
    sync_parent(path);
    Ok(())
}

/// The text of one corner or cell size that parses to `value`.
fn exact_text(value: f64) -> String {
    let fixed = format!("{value:.12}");
    if fixed
        .parse::<f64>()
        .is_ok_and(|v| v.to_bits() == value.to_bits())
    {
        fixed
    } else {
        format!("{value:?}")
    }
}

/// `yllcorner` text from which `yll + ny * cell` is exactly the origin, which a nearby double may
/// have to supply: the origin is not always the sum of a representable corner and a whole height.
fn corner_text(origin_y: f64, ny: usize, cell: f64) -> Option<String> {
    let height = ny as f64 * cell;
    let start = origin_y - height;
    if start + height == origin_y {
        return Some(exact_text(start));
    }
    let (mut below, mut above) = (start, start);
    for _ in 0..16 {
        below = below.next_down();
        above = above.next_up();
        for candidate in [below, above] {
            if candidate + height == origin_y {
                return Some(exact_text(candidate));
            }
        }
    }
    None
}

/// The grid's text, as `AAIGDataset::CreateCopy` writes it, including its first-value `.0`. It goes
/// straight to the writer, as a grid near the reader's limit could be gigabytes of it.
fn write_asc_text<W: Write>(
    out: &mut W,
    (nx, ny): (usize, usize),
    t: &GeoTransform,
    cells: &[f32],
    nodata: Option<f32>,
    georef: AscGeoref,
) -> Result<(), VortexError> {
    // Text that needs the allocator comes before the first byte, and each cell uses the stack.
    let cell = -t.dy;
    enum Georef {
        Vortex,
        Text {
            y: String,
            x: String,
            cell: Option<String>,
            dx: String,
            dy: String,
        },
    }
    let georef = match georef {
        AscGeoref::Vortex => {
            if t.dx != cell {
                return Err(refuse(
                    "ASCII grid write",
                    "unequal dx and dy under Vortex's rules; observed behaviour we did not \
                     implement: FORCE_CELLSIZE states dx for both",
                ));
            }
            Georef::Vortex
        }
        AscGeoref::RoundTrip => Georef::Text {
            y: corner_text(t.origin_y, ny, cell).ok_or_else(|| {
                refuse(
                    "ASCII grid write",
                    "no corner brings the frame's origin back exactly",
                )
            })?,
            x: exact_text(t.origin_x),
            cell: (t.dx == cell).then(|| exact_text(t.dx)),
            dx: exact_text(t.dx),
            dy: exact_text(cell),
        },
    };
    let marker = nodata.map(|m| format_g20(f64::from(m)));
    writeln!(out, "ncols        {nx}\nnrows        {ny}")?;
    match &georef {
        Georef::Vortex => write!(
            out,
            "xllcorner    {:.12}\nyllcorner    {:.12}\ncellsize     {:.12}\n",
            t.origin_x,
            t.origin_y - ny as f64 * t.dx,
            t.dx
        )?,
        Georef::Text { y, x, cell, dx, dy } => {
            write!(out, "xllcorner    {x}\nyllcorner    {y}\n")?;
            match cell {
                Some(cell) => writeln!(out, "cellsize     {cell}")?,
                None => write!(out, "dx           {dx}\ndy           {dy}\n")?,
            }
        }
    }
    if let Some(marker) = &marker {
        writeln!(out, "NODATA_value  {marker}")?;
    }
    let mut has_dot = false;
    for row in cells.chunks_exact(nx) {
        for &value in row {
            let value = f64::from(value);
            let mut token = NumberText::new();
            write_g20(value, &mut token)
                .map_err(|_| refuse("ASCII grid write", "a value's text is too long"))?;
            let text = token.as_str();
            write!(out, " {text}")?;
            if !has_dot {
                if text.contains(['.', 'e', 'E']) {
                    has_dot = true;
                } else if value.is_finite() {
                    out.write_all(b".0")?;
                    has_dot = true;
                }
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

/// A frame's `.prj` text, or `None` for an undefined CRS.
fn prj_text(crs: &Crs) -> Result<Option<String>, VortexError> {
    match crs {
        Crs::Undefined => Ok(None),
        Crs::Proj(_) => Err(refuse(
            "ASCII grid write",
            "a PROJ string has no .prj form here",
        )),
        Crs::Named(_) => Ok(Some(CrsDef::from_frame(crs)?.to_wkt())),
        Crs::Wkt(text) => {
            if !crate::source::raster::is_wkt1_crs(text) {
                return Err(refuse(
                    "ASCII grid write",
                    "the frame's WKT is not a WKT1 PROJCS or GEOGCS",
                ));
            }
            Ok(Some(text.trim().to_string()))
        }
    }
}

/// An ASCII grid and its `.prj`. Rows run north to south, so a south-up frame is flipped; a rotated
/// frame or one with `dx` not positive is refused, as the format cannot state either.
///
/// The two files cannot be replaced together. Both new files are written and read back first, then
/// the old grid and `.prj` are moved aside, the new `.prj` and grid renamed in, and the old ones
/// removed. **Any failure on the way puts the old files back,** byte-identical, so a failed write
/// changes nothing. If putting them back itself fails, the error names where they are, and they are
/// not removed. A crash in between leaves no grid at that path, never a mismatched pair, with the
/// old grid kept as `.<name>.<pid>.<n>.old`. Should removing the old files fail once the new ones
/// are in place, the write has still succeeded and the `.old` file stays.
///
/// Non-finite values do not survive: GDAL's reader clamps infinity, so an infinite cell is refused.
/// NaN reads back as NaN without its payload.
pub fn write_asc(frame: &GridFrame, path: &Path, options: &AscOptions) -> Result<(), VortexError> {
    write_asc_with_faults(frame, path, options, &mut |_| Ok(()))
}

/// Whether a `.prj` in another case would be what GDAL reads (`AAIGDataset::CommonOpen` tries `prj`
/// and then `PRJ`). On a folder that ignores case the one entry is the target itself.
fn refuse_prj_in_another_case(prj_path: &Path) -> Result<(), VortexError> {
    let Some(name) = prj_path.file_name() else {
        return Ok(());
    };
    let name = name.to_string_lossy();
    let parent = match prj_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut same_but_case = 0;
    for entry in std::fs::read_dir(parent)? {
        if entry?
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(&name)
        {
            same_but_case += 1;
        }
    }
    if same_but_case > 1 || (same_but_case == 1 && !prj_path.exists()) {
        return Err(refuse(
            "raster write",
            "a .prj in another case beside the target would be read for this grid",
        ));
    }
    Ok(())
}

/// Undo the moves made so far, last first. The first failure ends it, and says where things are.
fn restore_pair(
    target: &Path,
    prj_path: &Path,
    old_grid: Option<&Path>,
    old_prj: Option<&Path>,
    new_prj_in_place: bool,
    fault: Faults<'_>,
    cause: VortexError,
) -> VortexError {
    let kept = |what: &str, path: Option<&Path>, e: &dyn std::fmt::Display| {
        VortexError::Io(std::io::Error::other(format!(
            "the write failed ({cause}) and {what} could not be undone ({e}). {}",
            match path {
                Some(path) => format!("The previous file is kept at {}", path.display()),
                None => "Nothing was lost".to_string(),
            }
        )))
    };
    // Each step is what it says; a test can make any one fail. The message names every old file
    // not back in place.
    if new_prj_in_place && old_prj.is_none() {
        let done = fault(WriteStep::RemoveNewPrj).and_then(|()| std::fs::remove_file(prj_path));
        if let Err(e) = done {
            return kept("removing the new .prj", old_grid, &e);
        }
    }
    if let Some(old) = old_prj {
        let done = fault(WriteStep::RestorePrj).and_then(|()| std::fs::rename(old, prj_path));
        if let Err(e) = done {
            // The grid stays aside: beside the new `.prj` it would be a mismatched pair.
            let kept_prj = kept("restoring the .prj", Some(old), &e);
            return match old_grid {
                Some(grid) => VortexError::Io(std::io::Error::other(format!(
                    "{kept_prj}. The previous grid is kept at {}",
                    grid.display()
                ))),
                None => kept_prj,
            };
        }
    }
    if let Some(old) = old_grid {
        let done = fault(WriteStep::RestoreGrid).and_then(|()| std::fs::rename(old, target));
        if let Err(e) = done {
            return kept("restoring the grid", Some(old), &e);
        }
    }
    cause
}

/// Move `path` aside if it exists, returning where it went.
fn move_aside(path: &Path) -> Result<Option<PathBuf>, VortexError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let to = sibling(path, "old")?;
            std::fs::rename(path, &to)?;
            Ok(Some(to))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// [`write_asc`], with a failure injected at a chosen [`WriteStep`]. For tests.
#[doc(hidden)]
pub fn write_asc_with_faults(
    frame: &GridFrame,
    path: &Path,
    options: &AscOptions,
    fault: Faults<'_>,
) -> Result<(), VortexError> {
    refuse_stale_sidecar(path)?;
    let parts = frame.parts();
    let t = parts.transform;
    if t.row_rotation != 0.0 || t.column_rotation != 0.0 {
        return Err(refuse("ASCII grid write", "the frame is rotated"));
    }
    if t.dx.is_nan() || t.dx <= 0.0 {
        return Err(refuse("ASCII grid write", "dx is not positive"));
    }
    let prj = prj_text(&parts.crs)?;
    let (nx, ny) = (parts.nx, parts.ny);
    // As AscDataWriter: origin plus dy * ny, rows reversed.
    let south_up = parts.row_order == RowOrder::SouthToNorth;
    let north_up;
    let (source, transform) = if south_up {
        north_up = flipped(&parts.cells, nx)?;
        let mut t = t;
        t.origin_y += ny as f64 * t.dy;
        t.dy = -t.dy;
        (north_up.as_slice(), t)
    } else {
        (parts.cells.as_slice(), t)
    };
    let policy = match options.nodata {
        AscNodata::Keep => NodataPolicy::Keep,
        AscNodata::ResetToVortex => NodataPolicy::Reset(-9999.0),
    };
    let (cells, nodata) = apply_nodata(source, parts.nodata, policy)?;
    if cells.iter().any(|c| c.is_infinite()) {
        return Err(refuse(
            "ASCII grid write",
            "an infinite cell does not survive GDAL's reader",
        ));
    }

    let prj_path = path.with_extension("prj");
    if prj_path == path {
        return Err(refuse(
            "ASCII grid write",
            "a grid cannot be named as its own .prj",
        ));
    }
    refuse_directory(path)?;
    refuse_directory(&prj_path)?;
    refuse_prj_in_another_case(&prj_path)?;

    fault(WriteStep::CreateTemp)?;
    let (mut temp, file) = TempFile::beside(path)?;
    {
        let mut out = BufWriter::new(&file);
        fault(WriteStep::WriteData)?;
        write_asc_text(
            &mut out,
            (nx, ny),
            &transform,
            &cells,
            nodata,
            options.georef,
        )?;
        fault(WriteStep::Flush)?;
        out.flush()?;
    }
    fault(WriteStep::ReadBack)?;
    let expected = geokeys::canonical(match &prj {
        Some(text) => Crs::Wkt(text.clone()),
        None => Crs::Undefined,
    });
    let bytes = std::fs::read(&temp.path)?;
    let read = RasterFile::from_bytes(
        "read_back.asc",
        &bytes,
        prj.as_deref().map(str::as_bytes),
        Default::default(),
    )?;
    drop(bytes);
    if prj.is_none() && read.crs_status() != &CrsStatus::NotInFile {
        return Err(refuse("ASCII grid write", "a CRS appeared on read-back"));
    }
    check_read_back(
        &read,
        (nx, ny),
        &transform,
        &cells,
        nodata,
        &expected,
        options.georef == AscGeoref::RoundTrip,
        true,
    )?;

    fault(WriteStep::SyncTemp)?;
    file.sync_all()?;
    drop(file);
    let prj_temp = match &prj {
        Some(text) => {
            fault(WriteStep::CreatePrjTemp)?;
            let (temp, mut file) = TempFile::beside(&prj_path)?;
            fault(WriteStep::WritePrjTemp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            Some(temp)
        }
        None => None,
    };

    // Nothing above changed the target. From here every failure puts things back.
    let old_grid = fault(WriteStep::MoveOldGrid)
        .map_err(VortexError::from)
        .and_then(|()| move_aside(path))?;
    let old_prj = match fault(WriteStep::MoveOldPrj)
        .map_err(VortexError::from)
        .and_then(|()| move_aside(&prj_path))
    {
        Ok(old) => old,
        Err(e) => {
            return Err(restore_pair(
                path,
                &prj_path,
                old_grid.as_deref(),
                None,
                false,
                fault,
                e,
            ))
        }
    };
    let mut new_prj_in_place = false;
    if let Some(mut temp) = prj_temp {
        let renamed = fault(WriteStep::RenamePrj)
            .map_err(VortexError::from)
            .and_then(|()| Ok(std::fs::rename(&temp.path, &prj_path)?));
        if let Err(e) = renamed {
            return Err(restore_pair(
                path,
                &prj_path,
                old_grid.as_deref(),
                old_prj.as_deref(),
                false,
                fault,
                e,
            ));
        }
        temp.armed = false;
        new_prj_in_place = true;
    }
    let renamed = fault(WriteStep::RenameGrid)
        .map_err(VortexError::from)
        .and_then(|()| Ok(std::fs::rename(&temp.path, path)?));
    if let Err(e) = renamed {
        return Err(restore_pair(
            path,
            &prj_path,
            old_grid.as_deref(),
            old_prj.as_deref(),
            new_prj_in_place,
            fault,
            e,
        ));
    }
    temp.armed = false;
    sync_parent(path);
    // The write has succeeded. A file that cannot be removed now stays under its `.old` name.
    if fault(WriteStep::RemoveOld).is_ok() {
        for old in [&old_grid, &old_prj].into_iter().flatten() {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(())
}

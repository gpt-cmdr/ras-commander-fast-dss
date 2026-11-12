//! NetCDF and HDF5 source: `netcdf-reader` for the bytes, Vortex 1.1.0's own decoded array as the
//! reference for what they mean.
//!
//! Layers, each with its source named in its file:
//! - [`budget`]: what a file may make the reader do, decided before it is entered.
//! - [`model`]: the variables with their group paths.
//! - [`axes`]: which variables are axes, and each data variable's coordinate system.
//! - [`values`]: stored values to `f32`, in `EnhanceScaleMissingUnsignedImpl`'s arithmetic.
//! - [`time`]: CF time units, `cell_methods` and Vortex's file-name rules.
//! - [`wkt`]: CF grid mappings to the WKT `WktFactory` writes.
//!
//! This reproduces `GridDatasetReader`. A dataset NetCDF-Java does not read as a grid goes to
//! `VariableDsReader`, which no sample reaches, so here it is refused.

mod axes;
mod budget;
mod model;
mod time;
mod values;
mod wkt;

use crate::error::VortexError;
use crate::source::grib::nearly_equals_f64;
use crate::units::km_to_m_or_refuse;
use crate::{Crs, DataType, FrameTime, GeoTransform, GridFrame, GridFrameParts, RowOrder};
use axes::CoordSystems;
use budget::Metered;
use hdf5_reader::storage::{BytesStorage, DynStorage, FileStorage, Storage};
use model::{attr_string, find_attr_ignore_case, string_attr, units, Model, Var};
use netcdf_reader::{
    NcFile, NcMetadataMode, NcOpenOptions, NcReadable, NcSliceInfo, NcSliceInfoElem, NcType,
};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
pub use time::parse_time_cell_method;
use time::{Calendar, DateUnit, DeclaredType, Special};
use values::{Enhance, Kind};

fn refused(what: impl Into<String>, reason: impl Into<String>) -> VortexError {
    VortexError::Refused {
        what: what.into(),
        reason: reason.into(),
    }
}

fn decode_error(message: impl Into<String>) -> VortexError {
    VortexError::Decode {
        format: "NetCDF",
        message: message.into(),
    }
}

/// Stack for decoder calls, reserved and committed as used. Every recursion in the decoder has a
/// depth limit, or one set by [`budget`] before it is entered. `tests/nc_hostile.rs` runs the ones it can build
/// at those depths on a stack an eighth of this size, so what stack the caller has does not matter.
const DECODER_STACK_BYTES: usize = 16 << 20;

/// Run a call on the decoder's stack, with a panic turned into a typed error.
fn on_decoder_stack<T: Send>(
    call: impl FnOnce() -> Result<T, VortexError> + Send,
) -> Result<T, VortexError> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("netcdf-decoder".to_string())
            .stack_size(DECODER_STACK_BYTES)
            .spawn_scoped(scope, call)
            .map_err(|e| {
                refused(
                    "NetCDF file",
                    format!("could not start a thread for its decoder: {e}"),
                )
            })?
            .join()
            .unwrap_or_else(|_| Err(decode_error("the decoder panicked on this input")))
    })
}

/// `netcdf-reader` calls, with an error turned into a typed one. A panic in the decoder is too.
fn guarded<T>(call: impl FnOnce() -> netcdf_reader::Result<T>) -> Result<T, VortexError> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(decode_error(error.to_string())),
        Err(_) => Err(decode_error("the decoder panicked on this input")),
    }
}

/// [`guarded`] for one data read, with the storage's allowance reset for it. One call reads each
/// chunk and index node once, and a file cannot make the reader read much past its own size.
fn guarded_read<T: Send>(
    source: &NetcdfFile,
    call: impl FnOnce() -> netcdf_reader::Result<T> + Send,
) -> Result<T, VortexError> {
    source
        .storage
        .allow(source.read_allowance(), source.limits.max_seconds_per_read);
    on_decoder_stack(|| guarded(call)).map_err(|error| {
        if source.storage.tripped() {
            refused("NetCDF file", format!("a read was refused: {error}"))
        } else {
            error
        }
    })
}

/// Bounds on what a file can make this reader allocate. The cell and input limits match the
/// GRIB2 reader's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most cells in one grid.
    pub max_cells: usize,
    /// Largest input. A file opened by path is read by range, each read taking only the metadata and
    /// chunks it touches, so its size costs no memory. [`NetcdfFile::from_bytes`] already holds its
    /// bytes.
    pub max_input_bytes: u64,
    /// Most variables in one file.
    pub max_variables: usize,
    /// Most values in one axis or bounds variable.
    pub max_axis_points: usize,
    /// Largest decoded chunk, passed to `netcdf-reader`.
    pub max_chunk_bytes: usize,
    /// Fewest bytes in the chunk cache. A step whose chunks hold several steps is cheap only when the
    /// cache holds all of them: otherwise a sequential read gets no hits and decodes them all again.
    /// A file needing more than this, up to [`Limits::max_sized_chunk_cache_bytes`], gets what its
    /// largest such variable needs. To cache a larger step, set this to its size.
    pub chunk_cache_bytes: usize,
    /// Most bytes the cache is sized to by itself. Not a limit on [`Limits::chunk_cache_bytes`].
    pub max_sized_chunk_cache_bytes: usize,
    /// Chunks in the cache. 0 derives it from the cache's bytes and the file's smallest chunk.
    pub chunk_cache_slots: usize,
    /// Most bytes of metadata the reader may keep: headers, attributes, groups and the model built
    /// from them. Estimated before the reader is entered, and also the most metadata it may read to
    /// find out.
    pub max_metadata_bytes: u64,
    /// Most chunks one read may take, counted from that read's selection. The reader lists each
    /// before reading any, from the dataset's shape alone, and charges that list. A real time axis in
    /// one-value chunks takes one chunk per step, and netCDF-C before 4.3 wrote them.
    pub max_chunks_per_read: u64,
    /// Most bytes one read may decode: each filter's output for each chunk in its selection, whole,
    /// however few cells it returns. This is the limit on a read's time. A read over it is a typed
    /// refusal that names this setting. The largest real read measured is 0.279 GiB. The default
    /// also admits a NetCDF-4 file in AORC's Zarr chunk shape, 144 x 128 x 256 of `i2` under shuffle
    /// and deflate, whose step takes 1,089 chunks and 20.6 GB. A time-series rechunking at about 100 GB
    /// a step, which Vortex reads, needs this raised.
    pub max_decoded_bytes_per_read: u64,
    /// Most bytes of parsed structures the reader may build in one open or read, counting what it
    /// also releases. Below this, a process short of memory gets a typed refusal from the same check.
    pub max_reader_bytes: u64,
    /// Time one read may take, checked before each chunk is decoded. No limit by default: the time
    /// a read takes is limited by [`Limits::max_decoded_bytes_per_read`].
    pub max_seconds_per_read: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_cells: 1 << 28,
            max_input_bytes: 1 << 46,
            max_variables: 1 << 16,
            max_axis_points: 1 << 24,
            // netcdf-reader's own default.
            max_chunk_bytes: 256 * 1024 * 1024,
            chunk_cache_bytes: 64 * 1024 * 1024,
            max_sized_chunk_cache_bytes: 1 << 30,
            chunk_cache_slots: 0,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_chunks_per_read: 1 << 22,
            max_decoded_bytes_per_read: 32 << 30,
            max_reader_bytes: 8 << 30,
            max_seconds_per_read: None,
        }
    }
}

/// A NetCDF or HDF5 file in memory.
pub struct NetcdfFile {
    file_name: String,
    file: NcFile,
    storage: Arc<Metered>,
    metadata_estimate: u64,
    chunked: HashMap<u64, budget::Chunked>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    model: Model,
    systems: CoordSystems,
    limits: Limits,
}

impl std::fmt::Debug for NetcdfFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetcdfFile")
            .field("file_name", &self.file_name)
            .field("variables", &self.model.vars.len())
            .finish()
    }
}

/// One record as `DataReader.getDto` describes it. Unlike a [`GridFrame`], this keeps what Vortex
/// reports even where a frame cannot hold it, such as a period type on an instant.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub cells: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
    pub origin_x: f64,
    pub origin_y: f64,
    pub dx: f64,
    pub dy: f64,
    pub wkt: String,
    pub nodata: f64,
    pub units: Option<String>,
    pub short_name: String,
    pub full_name: String,
    pub description: Option<String>,
    /// The `standard_name` and `cell_methods` attributes, so a variable's identity can be checked.
    pub standard_name: Option<String>,
    pub cell_methods: Option<String>,
    /// Milliseconds since 1970, or `None` for `VortexDataInterval.UNDEFINED`.
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub data_type: DataType,
}

#[derive(Debug, Clone, Copy)]
enum Layout {
    /// Rows are y and come first in the file.
    YFirst,
    /// x comes first in the file, as IMERG stores it. `readDataSlice` reorders to y, x.
    XFirst,
}

/// A step as [`Variable::frame`] would state it, before its cells are read.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameHeader {
    pub nx: usize,
    pub ny: usize,
    pub row_order: RowOrder,
    pub transform: GeoTransform,
    pub crs: Crs,
    pub nodata: Option<f32>,
    pub time: FrameTime,
    pub data_type: DataType,
    pub units: String,
    pub variable: String,
    pub description: Option<String>,
    /// The variable's own name, without its group path.
    pub short_name: String,
    /// The window the file itself states for this step, in milliseconds since 1970: its time bounds,
    /// or its time twice for an instant; `None` with no time. [`Self::time`] is what Vortex reports,
    /// which one of its file-name rules may have moved off this.
    pub file_window: Option<(i64, i64)>,
    /// The file-name rule Vortex applied to this file and variable, by its name in
    /// `GridDatasetReader.SpecialFileType`, or `None`.
    pub name_rule: Option<&'static str>,
}

/// One selectable variable, with its grid and time axis read.
#[derive(Debug)]
pub struct Variable<'a> {
    source: &'a NetcdfFile,
    var: &'a Var,
    enhance: Enhance,
    nx: usize,
    ny: usize,
    layout: Layout,
    time_dim: Option<usize>,
    steps: usize,
    windows: Vec<Option<(i64, i64)>>,
    /// The windows with no file-name rule applied, where one was.
    own_windows: Option<Vec<Option<(i64, i64)>>>,
    special: Special,
    origin_x: f64,
    origin_y: f64,
    dx: f64,
    dy: f64,
    wkt: String,
    declared: Option<DeclaredType>,
    nodata: f64,
}

impl NetcdfFile {
    /// Read a file. The file's name matters: Vortex sets some time windows from it.
    pub fn open(path: &Path) -> Result<Self, VortexError> {
        Self::open_with_limits(path, Limits::default())
    }

    pub fn open_with_limits(path: &Path, limits: Limits) -> Result<Self, VortexError> {
        Self::open_with_stop(path, limits, None)
    }

    /// [`Self::open_with_limits`], with the caller's own stop flag in place of the file's. Setting it
    /// is [`Self::request_stop`] on every file opened with it, so a caller that never sees the file,
    /// such as one behind `import::import_gridded`, can still end a read.
    pub fn open_with_stop(
        path: &Path,
        limits: Limits,
        stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self, VortexError> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(path)?.len();
        if size > limits.max_input_bytes {
            return Err(too_large(size, &limits));
        }
        let storage: DynStorage = Arc::new(
            FileStorage::open(path)
                .map_err(|e| decode_error(format!("could not open the file: {e}")))?,
        );
        on_decoder_stack(move || Self::parse(file_name, storage, limits, stop))
    }

    /// Parse bytes.
    pub fn from_bytes(
        file_name: impl Into<String>,
        data: Vec<u8>,
        limits: Limits,
    ) -> Result<Self, VortexError> {
        if data.len() as u64 > limits.max_input_bytes {
            return Err(too_large(data.len() as u64, &limits));
        }
        let file_name = file_name.into();
        let bytes: DynStorage = Arc::new(BytesStorage::from_owner(data));
        on_decoder_stack(move || Self::parse(file_name, bytes, limits, None))
    }

    /// What the reader's cache holds and has done.
    pub fn chunk_cache_stats(&self) -> Option<netcdf_reader::ChunkCacheStats> {
        self.file.chunk_cache_stats()
    }

    fn parse(
        file_name: String,
        bytes: DynStorage,
        limits: Limits,
        stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self, VortexError> {
        let found = budget::preflight(&bytes, &limits)?;
        let mut limits = limits;
        let (cache_bytes, cache_slots) = budget::size_chunk_cache(&found, &limits);
        limits.chunk_cache_bytes = cache_bytes;
        limits.chunk_cache_slots = cache_slots;
        // The reader's allocations cannot fail softly, so find out now whether they will fit.
        let room = found.estimate.saturating_add(budget::TRANSIENT_BYTES);
        let mut probe: Vec<u8> = Vec::new();
        probe
            .try_reserve_exact(usize::try_from(room).unwrap_or(usize::MAX))
            .map_err(|_| {
                refused(
                    "NetCDF file",
                    format!("could not allocate {room} bytes for its metadata"),
                )
            })?;
        drop(probe);
        let options = NcOpenOptions {
            chunk_cache_bytes: limits.chunk_cache_bytes,
            chunk_cache_slots: limits.chunk_cache_slots,
            // Strict: the lossy mode guesses a dataset's dimensions from their sizes.
            metadata_mode: NcMetadataMode::Strict,
            max_chunk_bytes: limits.max_chunk_bytes,
            max_read_bytes: limits.max_cells.saturating_mul(8),
            max_object_header_bytes: usize::try_from(limits.max_metadata_bytes)
                .unwrap_or(usize::MAX),
            external_file_resolver: None,
            external_link_resolver: None,
            filter_registry: None,
        };
        let stop = stop.unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let storage = Arc::new(
            Metered::new(bytes, 0, found.open_reads).with_limits(&limits, Some(stop.clone())),
        );
        let opened = guarded(|| NcFile::from_storage_with_options(storage.clone(), options))
            .and_then(|file| {
                let root = guarded(|| file.root_group())?;
                let model = Model::build(root, limits.max_variables, storage.as_ref())?;
                Ok((file, model))
            });
        // Whatever the reader did with a failed read, its picture of the file may be partial.
        if storage.tripped() {
            return Err(refused(
                "NetCDF file",
                "opening it needed more reading than its metadata accounts for",
            ));
        }
        let (file, model) = opened?;
        let systems = axes::build(&model)?;
        Ok(NetcdfFile {
            file_name,
            file,
            storage,
            metadata_estimate: found.estimate,
            chunked: found.chunked,
            stop,
            model,
            systems,
            limits,
        })
    }

    /// Stop a read in progress from another thread. It ends with a typed error before its next
    /// chunk, and so do later reads until [`NetcdfFile::clear_stop`].
    pub fn request_stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn clear_stop(&self) {
        self.stop.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Bytes of metadata [`budget`] estimated the reader would keep for this file.
    pub fn metadata_estimate(&self) -> u64 {
        self.metadata_estimate
    }

    /// `NetcdfDataReader.getVariables`: full names of variables with a coordinate system, sorted.
    pub fn variable_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .model
            .vars
            .iter()
            .zip(&self.systems.system)
            .filter(|(_, system)| !system.is_empty())
            .map(|(v, _)| v.full.clone())
            .collect();
        names.sort();
        names
    }

    fn find_listed(&self, name: &str) -> Result<usize, VortexError> {
        self.model
            .vars
            .iter()
            .position(|v| v.full == name)
            .filter(|i| !self.systems.system[*i].is_empty())
            .ok_or_else(|| {
                refused(
                    format!("NetCDF variable {name}"),
                    "not in this file, or it has no coordinate system",
                )
            })
    }

    /// `DataReader.builder().variable(name).build()`.
    pub fn variable(&self, name: &str) -> Result<Variable<'_>, VortexError> {
        let index = self.find_listed(name)?;
        let resolved = catch_unwind(AssertUnwindSafe(|| Variable::resolve(self, index)))
            .unwrap_or_else(|_| Err(decode_error("this reader panicked on this input")));
        resolved.map_err(|error| match error {
            // Every refusal names the variable it is about.
            VortexError::Refused { what, reason } if !what.contains(name) => VortexError::Refused {
                what: format!("NetCDF variable {name}: {what}"),
                reason,
            },
            other => other,
        })
    }

    /// One time step of one variable. See [`Variable::frame`].
    pub fn frame(&self, variable: &str, step: usize) -> Result<GridFrame, VortexError> {
        self.variable(variable)?.frame(step)
    }

    fn read_allowance(&self) -> u64 {
        self.storage.len().saturating_mul(4).saturating_add(1 << 22)
    }

    /// Decide one read before the reader is entered. [`budget::plan_read`] holds its chunks and the
    /// bytes they decode to the limits, and then a reservation of `bytes` and what the chunks need is made and released at once. A later read's
    /// own allocation might not be fallible, and a failed infallible allocation aborts the process;
    /// this gives the typed error first when the process has a memory limit.
    fn check_read(
        &self,
        var: &Var,
        selection: &NcSliceInfo,
        bytes: usize,
    ) -> Result<(), VortexError> {
        let name = var.full.as_str();
        let refuse = |why: String| refused(format!("NetCDF variable {name}"), why);
        let mut bytes = bytes;
        if let Some(found) = self.chunked.get(&var.address) {
            let lengths: Vec<u64> = var.dims.iter().map(|d| d.len).collect();
            let plan = budget::plan_read(
                found,
                &lengths,
                selection,
                element_size(var) as u64,
                &self.limits,
                self.chunk_cache_stats()
                    .map_or(0, |stats| stats.current_bytes as u64),
            )
            .map_err(refuse)?;
            bytes = bytes.saturating_add(usize::try_from(plan.read_bytes).unwrap_or(usize::MAX));
        }
        let mut probe: Vec<u8> = Vec::new();
        probe.try_reserve_exact(bytes).map_err(|_| {
            refused(
                format!("NetCDF variable {name}"),
                format!("could not allocate {bytes} bytes to read it"),
            )
        })
    }

    /// Stored values of one numeric variable, in file order, each as an `f64`, mapped by `f`.
    fn read_stored<R>(
        &self,
        var: &Var,
        selection: &NcSliceInfo,
        count: usize,
        f: impl Fn(f64) -> R,
    ) -> Result<Vec<R>, VortexError> {
        fn collect<T: NcReadable + Copy, R>(
            source: &NetcdfFile,
            name: &str,
            selection: &NcSliceInfo,
            count: usize,
            cast: impl Fn(T) -> f64,
            f: impl Fn(f64) -> R,
        ) -> Result<Vec<R>, VortexError> {
            let array = guarded_read(source, || {
                source.file.read_variable_slice::<T>(name, selection)
            })?;
            if array.len() != count {
                return Err(decode_error(format!(
                    "{name}: read {} values, expected {count}",
                    array.len()
                )));
            }
            let mut out = Vec::new();
            out.try_reserve_exact(count).map_err(|_| {
                refused(
                    format!("NetCDF variable {name}"),
                    format!("could not allocate {count} cells"),
                )
            })?;
            out.extend(array.iter().map(|v| f(cast(*v))));
            Ok(out)
        }
        let file = self;
        let name = var.full.as_str();
        // The stored array and the result exist together.
        self.check_read(
            var,
            selection,
            count.saturating_mul(element_size(var) + size_of::<R>()),
        )?;
        match Kind::of(&var.dtype) {
            Some(Kind::Byte) => collect(file, name, selection, count, |v: i8| f64::from(v), f),
            Some(Kind::UByte) => collect(file, name, selection, count, |v: u8| f64::from(v), f),
            Some(Kind::Short) => collect(file, name, selection, count, |v: i16| f64::from(v), f),
            Some(Kind::UShort) => collect(file, name, selection, count, |v: u16| f64::from(v), f),
            Some(Kind::Int) => collect(file, name, selection, count, |v: i32| f64::from(v), f),
            Some(Kind::UInt) => collect(file, name, selection, count, |v: u32| f64::from(v), f),
            Some(Kind::Float) => collect(file, name, selection, count, |v: f32| f64::from(v), f),
            Some(Kind::Double) => collect(file, name, selection, count, |v: f64| v, f),
            _ => Err(refused(
                format!("NetCDF variable {name}"),
                format!("data type {:?} has no reader here", var.dtype),
            )),
        }
    }

    /// Whole axis-sized variable, enhanced as axes are.
    fn read_axis(&self, index: usize, missing: bool) -> Result<Vec<f64>, VortexError> {
        let var = &self.model.vars[index];
        let count = checked_count(var, self.limits.max_axis_points)?;
        // No sample has one, and such an axis would be read whole for every variable on it.
        if var.dims.iter().filter(|d| d.len > 1).count() > 2 {
            return Err(refused(
                format!("NetCDF variable {}", var.full),
                "an axis over more than two dimensions",
            ));
        }
        let enhance = Enhance::new(var, true)?;
        let selection = NcSliceInfo::all(var.dims.len());
        if missing {
            self.read_stored(var, &selection, count, |v| enhance.to_f64(v))
        } else {
            self.read_stored(var, &selection, count, |v| enhance.to_f64_keep_missing(v))
        }
    }

    /// `CoordinateAxis1D.makeBoundsFromAux`, or `None` when it returns false.
    fn read_bounds(&self, axis: usize) -> Result<Option<Bounds>, VortexError> {
        let var = &self.model.vars[axis];
        let Some(name) = string_attr(&var.attrs, "bounds") else {
            return Ok(None);
        };
        let Some(index) = self.model.find_local(&name, var.group) else {
            return Ok(None);
        };
        let bounds = &self.model.vars[index];
        let (Some(first), Some(second), 2) =
            (bounds.dims.first(), bounds.dims.get(1), bounds.dims.len())
        else {
            return Ok(None);
        };
        if var.dims.first() != Some(first) || second.len != 2 {
            return Ok(None);
        }
        let values = self.read_axis(index, false)?;
        let n = var.dims[0].len as usize;
        // The two lists below.
        Vec::<u8>::new()
            .try_reserve_exact(values.len().saturating_mul(16))
            .map_err(|_| {
                refused(
                    format!("NetCDF variable {}", bounds.full),
                    "could not allocate its lists of bounds",
                )
            })?;
        let lower = values.iter().step_by(2).copied().collect::<Vec<f64>>();
        let upper = values
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect::<Vec<f64>>();
        if lower.len() != n || upper.len() != n {
            return Err(decode_error(format!("{}: bounds length", bounds.full)));
        }
        Ok(Some((lower, upper)))
    }

    /// The stored bytes of one step of a variable, little-endian, in file order, for the decode
    /// comparison against an independent reader. No scale, offset, missing or reordering.
    pub fn stored_step(&self, name: &str, step: usize) -> Result<Vec<u8>, VortexError> {
        let v = self.variable(name)?;
        let selection = v.selection(step)?;
        let var = v.var;
        let count = v.nx * v.ny;
        fn bytes<T: NcReadable + Copy>(
            source: &NetcdfFile,
            name: &str,
            selection: &NcSliceInfo,
            to_le: impl Fn(T, &mut Vec<u8>),
        ) -> Result<Vec<u8>, VortexError> {
            let array = guarded_read(source, || {
                source.file.read_variable_slice::<T>(name, selection)
            })?;
            let mut out = Vec::new();
            for value in array.iter() {
                to_le(*value, &mut out);
            }
            Ok(out)
        }
        let file = self;
        let full = var.full.as_str();
        self.check_read(var, &selection, count.saturating_mul(element_size(var) * 2))?;
        let out = match Kind::of(&var.dtype) {
            Some(Kind::Byte) => bytes(file, full, &selection, |v: i8, o| o.extend(v.to_le_bytes())),
            Some(Kind::UByte) => {
                bytes(file, full, &selection, |v: u8, o| o.extend(v.to_le_bytes()))
            }
            Some(Kind::Short) => bytes(file, full, &selection, |v: i16, o| {
                o.extend(v.to_le_bytes())
            }),
            Some(Kind::UShort) => bytes(file, full, &selection, |v: u16, o| {
                o.extend(v.to_le_bytes())
            }),
            Some(Kind::Int) => bytes(file, full, &selection, |v: i32, o| {
                o.extend(v.to_le_bytes())
            }),
            Some(Kind::UInt) => bytes(file, full, &selection, |v: u32, o| {
                o.extend(v.to_le_bytes())
            }),
            Some(Kind::Float) => bytes(file, full, &selection, |v: f32, o| {
                o.extend(v.to_le_bytes())
            }),
            Some(Kind::Double) => bytes(file, full, &selection, |v: f64, o| {
                o.extend(v.to_le_bytes())
            }),
            _ => Err(refused(format!("NetCDF variable {full}"), "not numeric")),
        }?;
        let width = out.len().checked_div(count).unwrap_or(0);
        if width == 0 || width * count != out.len() {
            return Err(decode_error(format!("{full}: {} stored bytes", out.len())));
        }
        Ok(out)
    }
}

/// A window and an edge.
const TIME_BYTES_PER_STEP: usize = 24 + 8;

fn element_size(var: &Var) -> usize {
    var.dtype.size().unwrap_or(8)
}

fn too_large(size: u64, limits: &Limits) -> VortexError {
    refused(
        "NetCDF file",
        format!(
            "{size} bytes is over the limit of {}",
            limits.max_input_bytes
        ),
    )
}

fn checked_count(var: &Var, limit: usize) -> Result<usize, VortexError> {
    let mut count: u64 = 1;
    for d in &var.dims {
        count = count.checked_mul(d.len).ok_or_else(|| {
            refused(
                format!("NetCDF variable {}", var.full),
                "its size overflows",
            )
        })?;
    }
    usize::try_from(count)
        .ok()
        .filter(|c| *c <= limit)
        .ok_or_else(|| {
            refused(
                format!("NetCDF variable {}", var.full),
                format!("{count} values is over the limit of {limit}"),
            )
        })
}

/// `bound1` and `bound2`.
type Bounds = (Vec<f64>, Vec<f64>);

/// `CoordinateAxis1D` for one horizontal axis.
struct Axis1D {
    edges: Vec<f64>,
    regular: bool,
}

/// `CoordinateAxis1D.makeEdges`.
fn make_edges(coords: &[f64]) -> Vec<f64> {
    let size = coords.len();
    let mut edge = vec![0.0; size + 1];
    if size == 0 {
        return edge;
    }
    if size == 1 {
        edge[0] = coords[0];
        edge[1] = coords[0];
        return edge;
    }
    for i in 1..size {
        edge[i] = (coords[i - 1] + coords[i]) / 2.0;
    }
    edge[0] = coords[0] - (edge[1] - coords[0]);
    edge[size] = coords[size - 1] + (coords[size - 1] - edge[size - 1]);
    edge
}

/// `Misc.defaultMaxRelativeDiffDouble`, for `nearlyEquals` with no stated tolerance.
const MAX_RELATIVE_DIFF_DOUBLE: f64 = 1.0e-8;

/// `makeBoundsFromAux`'s edges and `isContiguous`.
fn edges_from_bounds(lower: &[f64], upper: &[f64]) -> (Vec<f64>, bool) {
    let n = lower.len();
    let contiguous = (0..n.saturating_sub(1))
        .all(|i| nearly_equals_f64(lower[i + 1], upper[i], MAX_RELATIVE_DIFF_DOUBLE));
    let mut edge = vec![0.0; n + 1];
    if n == 0 {
        return (edge, contiguous);
    }
    edge[0] = lower[0];
    if contiguous {
        edge[1..(n + 1)].copy_from_slice(&upper[..n]);
    } else {
        for i in 1..n {
            edge[i] = (lower[i] + upper[i - 1]) / 2.0;
        }
        edge[n] = upper[n - 1];
    }
    (edge, contiguous)
}

/// `CoordinateAxis1D.calcIsRegular`.
fn is_regular(coords: &[f64]) -> bool {
    let n = coords.len();
    if n < 2 {
        return true;
    }
    let increment = (coords[n - 1] - coords[0]) / (n - 1) as f64;
    (1..n).all(|i| nearly_equals_f64(coords[i] - coords[i - 1], increment, 5.0e-3))
}

/// `CoordinateAxis1D.correctLongitudeWrap`.
fn correct_longitude_wrap(coords: &mut [f64]) -> bool {
    if coords.len() < 2 {
        return false;
    }
    let ascending = coords[0] < coords[1];
    let monotonic = coords
        .windows(2)
        .all(|w| if ascending { w[0] < w[1] } else { w[0] > w[1] });
    if monotonic {
        return false;
    }
    let mut cross = false;
    for i in 0..coords.len() {
        if cross {
            coords[i] += if ascending { 360.0 } else { -360.0 };
        }
        if !cross && i + 1 < coords.len() {
            let turned = if ascending {
                coords[i] > coords[i + 1]
            } else {
                coords[i] < coords[i + 1]
            };
            cross = turned;
        }
    }
    true
}

impl<'a> Variable<'a> {
    fn resolve(source: &'a NetcdfFile, index: usize) -> Result<Self, VortexError> {
        let model = &source.model;
        let var = &model.vars[index];
        let what = || format!("NetCDF variable {}", var.full);
        let axes_of = &source.systems.system[index];
        let picked = axes::pick(model, &source.systems, axes_of);
        let projection = wkt::projection(model, &source.systems, axes_of, &picked)?;

        // GridCoordSys.isGridCoordSys.
        let lat_lon = picked.lat.is_some() && picked.lon.is_some();
        let geo_xy = picked.x.is_some() && picked.y.is_some() && projection.is_some();
        if !lat_lon && !geo_xy {
            return Err(refused(
                what(),
                "not a grid: no lat and lon, nor x, y and a projection",
            ));
        }
        let (x, y) = if geo_xy {
            (picked.x, picked.y)
        } else {
            (picked.lon, picked.lat)
        };
        let (x, y) = (x.unwrap_or(index), y.unwrap_or(index));
        let (x_var, y_var) = (&model.vars[x], &model.vars[y]);
        if x_var.dims.len() != 1 || y_var.dims.len() != 1 {
            // GridDatasetReader.getGridDefinition's CoordinateAxis2D branch, then IndexSearcher.
            return Err(refused(
                what(),
                "its horizontal axes are two-dimensional. Vortex resamples these by a nearest-cell \
                 search, which is not implemented here",
            ));
        }
        let time_axis = picked.time;
        if picked.ensemble.is_some() {
            return Err(refused(what(), "an ensemble axis has no sample here"));
        }
        let mut used = vec![x, y];
        used.extend([picked.z, time_axis, picked.run_time].into_iter().flatten());
        if !axes::covers(model, &used, &var.dims) {
            return Err(refused(
                what(),
                "not a grid: it has a dimension outside its grid axes",
            ));
        }
        if let Some(t) = time_axis {
            if model.vars[t].dims.len() != 1 {
                return Err(refused(what(), "its time axis is not one-dimensional"));
            }
        }
        if let Some(t) = picked.time_offset {
            if time_axis.is_none() || model.vars[t].dims.len() != 1 {
                return Err(refused(what(), "forecast-period axes have no sample here"));
            }
        }

        // Which dimension is which. GeoGrid's constructor, by name.
        let find_dim = |axis: usize| {
            let d = model.vars[axis].dims.first()?;
            var.dims.iter().position(|v| v == d)
        };
        let x_at = find_dim(x);
        let y_at = find_dim(y);
        let (Some(x_at), Some(y_at)) = (x_at, y_at) else {
            return Err(refused(what(), "its grid axes do not span its dimensions"));
        };
        if x_at == y_at {
            return Err(refused(what(), "x and y share a dimension"));
        }
        let time_dim = time_axis.and_then(find_dim);
        let z_dim = picked.z.and_then(find_dim);
        let run_dim = picked.run_time.and_then(find_dim);
        for (at, d) in var.dims.iter().enumerate() {
            if at == x_at || at == y_at || Some(at) == time_dim {
                continue;
            }
            // readDataSlice(0, 0, t, -1, -1, -1): every level, so more than one is not a grid.
            if d.len != 1 {
                let name = if Some(at) == z_dim {
                    "several vertical levels"
                } else if Some(at) == run_dim {
                    "several run times"
                } else {
                    "another dimension"
                };
                return Err(refused(what(), format!("{name} in dimension {}", d.name)));
            }
        }
        let layout = if x_at < y_at {
            Layout::XFirst
        } else {
            Layout::YFirst
        };

        let (nx, ny) = (var.dims[x_at].len as usize, var.dims[y_at].len as usize);
        let cells = nx.checked_mul(ny).filter(|c| *c <= source.limits.max_cells);
        if cells.is_none() || nx == 0 || ny == 0 {
            return Err(refused(
                what(),
                format!(
                    "{nx} x {ny} is empty or over the limit of {} cells",
                    source.limits.max_cells
                ),
            ));
        }
        let enhance = Enhance::new(var, true)?;

        let wkt_text = match &projection {
            Some(p) if geo_xy => p.wkt.clone(),
            _ => wkt::geographic()?,
        };
        let km_cell = geo_xy && !projection.as_ref().is_some_and(|p| p.rotated);
        let x_axis = source.horizontal_axis(x, !geo_xy, km_cell)?;
        let y_axis = source.horizontal_axis(y, false, km_cell)?;
        if !x_axis.regular || !y_axis.regular {
            // GridDatasetReader.getGridData's IndexSearcher path.
            return Err(refused(
                what(),
                "its horizontal axes are not evenly spaced. Vortex resamples these by a \
                 nearest-cell search, which is not implemented here",
            ));
        }

        // getGridDefinition.
        let (ulx, urx) = (x_axis.edges[0], x_axis.edges[nx]);
        let dx = (urx - ulx) / nx as f64;
        let (uly, lly) = (y_axis.edges[0], y_axis.edges[ny]);
        let dy = (lly - uly) / ny as f64;
        // scaleGrid: km on the axes, metres in the WKT, converted by indriya as Vortex does.
        let scale = |v: f64| {
            if km_cell {
                km_to_m_or_refuse(v, &what())
            } else {
                Ok(v)
            }
        };
        let (mut origin_x, origin_y, dx, dy) = (scale(ulx)?, scale(uly)?, scale(dx)?, scale(dy)?);
        // shiftGrid.
        let geographic = !km_cell;
        if geographic && (origin_x > 180.0 || origin_x + dx * nx as f64 > 180.0) {
            origin_x -= 360.0;
        }

        let special = time::special_file(&source.file_name, &var.short);
        let time_name = time_axis.map(|t| model.vars[t].short.clone());
        let steps = match time_dim {
            Some(at) => var.dims[at].len as usize,
            None => 1,
        };
        let windows = source.windows(var, time_axis, special, steps)?;
        let own_windows = match special {
            Special::Undefined => None,
            _ => Some(source.windows(var, time_axis, Special::Undefined, steps)?),
        };

        // getNoDataValue: missing_value as stored, else the scaled fill.
        // Attribute.getNumericValue also reads a number held as text.
        let nodata = model::find_attr(&var.attrs, "missing_value")
            .and_then(|a| {
                model::attr_numbers(a)
                    .and_then(|v| v.first().copied())
                    .or_else(|| attr_string(a).and_then(|t| t.trim().parse().ok()))
            })
            .unwrap_or(enhance.fill);

        let method = string_attr(&var.attrs, "cell_methods").unwrap_or_default();
        let declared = time::declared_type(&parse_time_cell_method(&method, time_name.as_deref()));

        Ok(Variable {
            source,
            var,
            enhance,
            nx,
            ny,
            layout,
            time_dim,
            steps,
            windows,
            own_windows,
            special,
            origin_x,
            origin_y,
            dx,
            dy,
            wkt: wkt_text,
            declared,
            nodata,
        })
    }

    /// `getDtoCount`.
    pub fn step_count(&self) -> usize {
        self.steps
    }

    /// Which of the variable's dimensions is time, in file order.
    pub fn time_dimension(&self) -> Option<usize> {
        self.time_dim
    }

    fn selection(&self, step: usize) -> Result<NcSliceInfo, VortexError> {
        if step >= self.steps {
            return Err(refused(
                format!("NetCDF variable {}", self.var.full),
                format!("step {step} of {}", self.steps),
            ));
        }
        let mut selections = Vec::with_capacity(self.var.dims.len());
        for (at, d) in self.var.dims.iter().enumerate() {
            selections.push(if Some(at) == self.time_dim {
                NcSliceInfoElem::Slice {
                    start: step as u64,
                    end: step as u64 + 1,
                    step: 1,
                }
            } else {
                NcSliceInfoElem::Slice {
                    start: 0,
                    end: d.len,
                    step: 1,
                }
            });
        }
        Ok(NcSliceInfo { selections })
    }

    /// `getDataType` of `VortexGrid`: the declared type, or inferred from the interval and name.
    fn data_type(&self, window: Option<(i64, i64)>) -> DataType {
        match self.declared {
            Some(DeclaredType::Instantaneous) => DataType::InstVal,
            Some(DeclaredType::Accumulation) => DataType::PerCum,
            Some(DeclaredType::Average) => DataType::PerAver,
            None => match window {
                Some((start, end)) if start != end => {
                    if crate::products::is_precipitation_name(&self.var.short) {
                        DataType::PerCum
                    } else {
                        DataType::PerAver
                    }
                }
                _ => DataType::InstVal,
            },
        }
    }

    /// `DataReader.getDto(step)`.
    pub fn step(&self, step: usize) -> Result<Step, VortexError> {
        catch_unwind(AssertUnwindSafe(|| self.read_step(step)))
            .unwrap_or_else(|_| Err(decode_error("this reader panicked on this input")))
    }

    fn read_step(&self, step: usize) -> Result<Step, VortexError> {
        let selection = self.selection(step)?;
        let count = self.nx * self.ny;
        let enhance = &self.enhance;
        let mut cells = self
            .source
            .read_stored(self.var, &selection, count, |v| enhance.to_f32(v))?;
        if matches!(self.layout, Layout::XFirst) {
            cells = transpose(&cells, self.nx, self.ny)?;
        }
        let window = self.windows.get(step).copied().flatten();
        let units = self.step_units();
        Ok(Step {
            cells,
            nx: self.nx,
            ny: self.ny,
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            dx: self.dx,
            dy: self.dy,
            wkt: self.wkt.clone(),
            nodata: self.nodata,
            units,
            short_name: self.var.short.clone(),
            full_name: self.var.full.clone(),
            description: model::description(self.var),
            standard_name: string_attr(&self.var.attrs, "standard_name"),
            cell_methods: string_attr(&self.var.attrs, "cell_methods"),
            start: window.map(|w| w.0),
            end: window.map(|w| w.1),
            data_type: self.data_type(window),
        })
    }

    /// What [`Self::frame`] states about a step, without reading a cell: the same fields, the same
    /// refusals. A run can plan every record from these. `tests/import_netcdf.rs` holds the two to
    /// the same answer on every step of every fixture.
    pub fn frame_header(&self, step: usize) -> Result<FrameHeader, VortexError> {
        if step >= self.steps {
            return Err(refused(
                format!("NetCDF variable {}", self.var.full),
                format!("step {step} of {}", self.steps),
            ));
        }
        let window = self.windows.get(step).copied().flatten();
        let row_order = if self.dy < 0.0 {
            RowOrder::NorthToSouth
        } else {
            RowOrder::SouthToNorth
        };
        let split = |t: i64| -> Result<(i64, i64), VortexError> {
            frame_stamp(t).ok_or_else(|| {
                refused(
                    format!("NetCDF variable {}", self.var.full),
                    "a time with a fraction of a second has no frame time",
                )
            })
        };
        let time = match window {
            Some((a, b)) if a == b => FrameTime::Instant(split(b)?),
            Some((a, b)) => FrameTime::Period {
                start: split(a)?,
                end: split(b)?,
            },
            None => FrameTime::Invariant,
        };
        let header = FrameHeader {
            nx: self.nx,
            ny: self.ny,
            row_order,
            transform: GeoTransform::axis_aligned(self.origin_x, self.dx, self.origin_y, self.dy),
            crs: Crs::Wkt(self.wkt.clone()),
            nodata: Some(self.nodata as f32),
            time,
            data_type: self.data_type(window),
            units: self.step_units().unwrap_or_default(),
            variable: self.var.full.clone(),
            description: model::description(self.var),
            short_name: self.var.short.clone(),
            file_window: match &self.own_windows {
                Some(own) => own.get(step).copied().flatten(),
                None => window,
            },
            name_rule: self.special.name(),
        };
        crate::frame::check_header(
            header.nx,
            header.ny,
            header.row_order,
            &header.transform,
            &header.time,
            header.data_type,
        )?;
        Ok(header)
    }

    // GridDatasetReader.getUnits.
    fn step_units(&self) -> Option<String> {
        if self.var.full.to_lowercase().contains("var209-6") {
            Some("mm".to_string())
        } else {
            units(self.var)
        }
    }

    /// The step as a frame. A frame cannot hold a period type on an instant, which Vortex reports
    /// for a variable whose `cell_methods` gives `time: sum` or `time: mean` and whose time axis
    /// has no bounds. Those are refused here; [`Self::step`] still returns what Vortex reports.
    pub fn frame(&self, step: usize) -> Result<GridFrame, VortexError> {
        let s = self.step(step)?;
        let row_order = if s.dy < 0.0 {
            RowOrder::NorthToSouth
        } else {
            RowOrder::SouthToNorth
        };
        let split = |t: i64| -> Result<(i64, i64), VortexError> {
            frame_stamp(t).ok_or_else(|| {
                refused(
                    format!("NetCDF variable {}", self.var.full),
                    "a time with a fraction of a second has no frame time",
                )
            })
        };
        let time = match (s.start, s.end) {
            (Some(a), Some(b)) if a == b => FrameTime::Instant(split(b)?),
            (Some(a), Some(b)) => FrameTime::Period {
                start: split(a)?,
                end: split(b)?,
            },
            _ => FrameTime::Invariant,
        };
        Ok(GridFrame::new(GridFrameParts {
            cells: s.cells,
            nx: s.nx,
            ny: s.ny,
            row_order,
            transform: GeoTransform::axis_aligned(s.origin_x, s.dx, s.origin_y, s.dy),
            crs: Crs::Wkt(s.wkt),
            nodata: Some(s.nodata as f32),
            time,
            data_type: s.data_type,
            units: s.units.unwrap_or_default(),
            variable: s.full_name,
            description: s.description,
        })?)
    }
}

/// HEC's julian day of 1970-01-01, the day a frame's time counts from (`dss_core::time::julian_to_ymd`:
/// day 0 is 1899-12-31).
const JULIAN_OF_1970: i64 = 25_568;

/// Milliseconds since 1970 as a frame's (julian day, seconds past midnight), or `None` for a time
/// with a fraction of a second, which a frame cannot hold. Midnight is 0 of its own day.
pub(crate) fn frame_stamp(ms: i64) -> Option<(i64, i64)> {
    if ms.rem_euclid(1000) != 0 {
        return None;
    }
    let seconds = ms.div_euclid(1000);
    Some((
        seconds.div_euclid(86_400).checked_add(JULIAN_OF_1970)?,
        seconds.rem_euclid(86_400),
    ))
}

/// x-first file order to `readDataSlice`'s y, x.
fn transpose(cells: &[f32], nx: usize, ny: usize) -> Result<Vec<f32>, VortexError> {
    let mut out = Vec::new();
    out.try_reserve_exact(cells.len()).map_err(|_| {
        refused(
            "NetCDF grid",
            format!("could not allocate {} cells", cells.len()),
        )
    })?;
    for y in 0..ny {
        for x in 0..nx {
            out.push(cells[x * ny + y]);
        }
    }
    Ok(out)
}

impl NetcdfFile {
    /// Coordinates and edges as `GridCoordSys` hands them on. `km`: `convertUnits` to kilometres.
    fn horizontal_axis(
        &self,
        index: usize,
        wrap_longitude: bool,
        km: bool,
    ) -> Result<Axis1D, VortexError> {
        let var = &self.model.vars[index];
        let what = || format!("NetCDF axis {}", var.full);
        if var.dims.len() != 1 || var.dtype == NcType::Char {
            return Err(refused(what(), "not a numeric one-dimensional axis"));
        }
        let mut coords = self.read_axis(index, true)?;
        let bounds = self.read_bounds(index)?;
        let mut edges_from_aux = None;
        if let Some((lower, upper)) = &bounds {
            edges_from_aux = Some(edges_from_bounds(lower, upper).0);
        }
        if km {
            // convertUnits, on a floating-point axis stores through the array's own type.
            let factor = match units(var)
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("km") | Some("kilometer") | Some("kilometers") => 1.0,
                Some("m") | Some("meter") | Some("meters") | Some("metre") | Some("metres") => {
                    0.001
                }
                other => {
                    return Err(refused(
                        what(),
                        format!("units {other:?} have no sample here"),
                    ))
                }
            };
            if factor != 1.0 {
                if bounds.is_some() {
                    return Err(refused(
                        what(),
                        "bounds on an axis in metres have no sample here",
                    ));
                }
                let float = var.dtype == NcType::Float;
                for c in &mut coords {
                    let v = factor * *c;
                    *c = if float { f64::from(v as f32) } else { v };
                }
            }
        }
        if wrap_longitude {
            correct_longitude_wrap(&mut coords);
        }
        if coords.iter().any(|c| c.is_nan()) {
            return Err(refused(what(), "it has missing coordinates"));
        }
        let regular = is_regular(&coords);
        let edges = edges_from_aux.unwrap_or_else(|| make_edges(&coords));
        Ok(Axis1D { edges, regular })
    }

    /// `GridDatasetReader.getTimeBounds`.
    fn windows(
        &self,
        var: &Var,
        time_axis: Option<usize>,
        special: Special,
        steps: usize,
    ) -> Result<Vec<Option<(i64, i64)>>, VortexError> {
        let Some(t) = time_axis else {
            return self.windows_from_attributes(var).map(|w| vec![w]);
        };
        let axis = &self.model.vars[t];
        let what = || format!("NetCDF time axis {}", axis.full);
        if axis.dtype == NcType::Char || axis.dtype == NcType::String {
            return Err(refused(what(), "text time coordinates have no sample here"));
        }
        let unit_text = units(axis).ok_or_else(|| refused(what(), "it has no units"))?;
        let calendar_name = find_attr_ignore_case(&axis.attrs, "calendar").and_then(attr_string);
        let calendar: Calendar = time::calendar(calendar_name.as_deref())?;
        let unit = DateUnit::parse(calendar, &unit_text)?;
        let coords = self.read_axis(t, true)?;
        if coords.len() != steps {
            return Err(refused(
                what(),
                format!("{} coordinates for {steps} steps", coords.len()),
            ));
        }
        if coords.is_empty() {
            return Ok(Vec::new());
        }
        let bounds = self.read_bounds(t)?;
        if bounds.is_none() && special == Special::Undefined {
            // getTimeInstants.
            let mut out = Vec::new();
            out.try_reserve_exact(coords.len())
                .map_err(|_| refused(what(), "could not allocate its windows"))?;
            for c in &coords {
                let instant = unit.instant(*c)?;
                out.push(Some((instant, instant)));
            }
            return Ok(out);
        }
        // Its edges are made next, infallibly, and then one window per step. A file names the step
        // count, and a valid one can have millions.
        let room = coords.len().saturating_mul(TIME_BYTES_PER_STEP);
        let mut probe: Vec<u8> = Vec::new();
        probe.try_reserve_exact(room).map_err(|_| {
            refused(
                what(),
                format!("could not allocate {room} bytes for its time"),
            )
        })?;
        drop(probe);
        let (edges, contiguous, aux) = match &bounds {
            Some((lower, upper)) => {
                let (edges, contiguous) = edges_from_bounds(lower, upper);
                (edges, contiguous, Some((lower, upper)))
            }
            None => (make_edges(&coords), true, None),
        };
        let initial = unit.instant(*coords.first().ok_or_else(|| refused(what(), "no steps"))?)?;
        let mut out = Vec::new();
        out.try_reserve_exact(coords.len())
            .map_err(|_| refused(what(), "could not allocate its windows"))?;
        for (i, c) in coords.iter().enumerate() {
            // getCoordBounds.
            let (low, high) = match aux {
                Some((lower, upper)) if !contiguous => (lower[i], upper[i]),
                _ => (edges[i], edges[i + 1]),
            };
            let (a, b) = time::adjust(
                special,
                unit.instant(low)?,
                unit.instant(high)?,
                unit.instant(*c)?,
                initial,
            );
            out.push(Some((a, b)));
        }
        Ok(out)
    }

    /// `getTimeRecordFromAttributes`.
    fn windows_from_attributes(&self, var: &Var) -> Result<Option<(i64, i64)>, VortexError> {
        let text = |name: &str| find_attr_ignore_case(&var.attrs, name).and_then(attr_string);
        if let (Some(start), Some(stop)) = (text("start_date"), text("stop_date")) {
            let parse = |s: &str| {
                time::parse_attribute_date(s).ok_or_else(|| {
                    refused(
                        format!("NetCDF variable {}", var.full),
                        format!("date {s:?} is not in the form Vortex reads"),
                    )
                })
            };
            return Ok(Some((parse(&start)?, parse(&stop)?)));
        }
        let global = find_attr_ignore_case(&self.model.root_attrs, "nominal_product_time")
            .and_then(attr_string);
        if let Some(time) = global {
            let unit = DateUnit::parse(Calendar::Iso, &format!("seconds since {time}"))?;
            let at = unit.instant(0.0)?;
            return Ok(Some((at, at)));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_follow_make_edges() {
        assert_eq!(make_edges(&[]), vec![0.0]);
        assert_eq!(make_edges(&[5.0]), vec![5.0, 5.0]);
        assert_eq!(make_edges(&[0.0, 1.0, 2.0]), vec![-0.5, 0.5, 1.5, 2.5]);
        assert_eq!(make_edges(&[2.0, 1.0]), vec![2.5, 1.5, 0.5]);
    }

    #[test]
    fn regularity_uses_the_axis_tolerance() {
        assert!(is_regular(&[0.0, 1.0, 2.0, 3.0]));
        assert!(!is_regular(&[0.0, 1.0, 2.0, 4.0]));
        assert!(is_regular(&[7.0]));
    }

    #[test]
    fn longitudes_wrap_as_netcdf_java_does() {
        let mut lon = vec![170.0, 180.0, -170.0, -160.0];
        assert!(correct_longitude_wrap(&mut lon));
        assert_eq!(lon, vec![170.0, 180.0, 190.0, 200.0]);
        let mut monotonic = vec![1.0, 2.0];
        assert!(!correct_longitude_wrap(&mut monotonic));
    }

    #[test]
    fn x_first_cells_reorder_to_rows() {
        // x, y order: nx = 3, ny = 2.
        let cells = [0.0, 1.0, 10.0, 11.0, 20.0, 21.0];
        assert_eq!(
            transpose(&cells, 3, 2).unwrap(),
            vec![0.0, 10.0, 20.0, 1.0, 11.0, 21.0]
        );
    }

    #[test]
    fn contiguous_bounds_give_edges() {
        let (e, c) = edges_from_bounds(&[0.0, 1.0], &[1.0, 2.0]);
        assert!(c);
        assert_eq!(e, vec![0.0, 1.0, 2.0]);
        let (e, c) = edges_from_bounds(&[0.0, 2.0], &[1.0, 3.0]);
        assert!(!c);
        assert_eq!(e, vec![0.0, 1.5, 3.0]);
    }
}

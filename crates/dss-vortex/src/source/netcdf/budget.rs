//! What a file may make `netcdf-reader` and `hdf5-reader` do, as CLB policy, around the protection
//! they now have themselves.
//!
//! The readers are vendored and patched (`vendor/PATCHES.md`). What a file sizes is allocated
//! fallibly, group recursion is bounded inside the traversal, and they charge [`Metered`] for what
//! they keep and report each chunk before decoding it. That closes the aborts this module could only
//! try to foresee. `tests/nc_reader_patch.rs` holds the readers to it with no walk before them.
//!
//! What stays here is the decision about what this reader should spend, which they cannot make:
//! - [`Metered`] is the budget: a limit on what is charged and on bytes decoded in one read, a time
//!   limit and a stop flag, and each megabyte charged, a check that memory is left.
//! - [`plan_read`] refuses a read that would take too many chunks or decode too much, at once, rather
//!   than after minutes, and sizes the reservation that gives a typed error first.
//! - `user_defined.rs` builds every field of every element of a user-defined attribute, and each kept
//!   copy of the metadata is not charged, so the walk still weighs attributes and refuses a type
//!   with overlapping members.
//! - `filters/mod.rs` leaves a stage's output limited only by HDF5's largest chunk when an earlier
//!   filter's size is unknown, and `check_work` counts the final size, so such a pipeline is refused.
//!
//! So [`preflight`] walks the same structures first, with the reader's own public parsers over a
//! [`Metered`] storage that fails once its allowance of bytes is used, keeping only sizes.
//!
//! **The walk is an allowlist.** Every gap found so far was the walk's model of a file disagreeing
//! with what the reader then did on it. So the walk does not estimate what it has not modelled: a
//! message whose use by the reader it does not know is refused, `read_object`'s match has no
//! catch-all, and a header the reader could read two ways is refused. The estimate is compared to
//! [`Limits::max_metadata_bytes`](super::Limits) and reserved, then released. A data read is decided
//! in [`plan_read`] with the dimension lengths the reader itself reports, not the dataset's own
//! shape. `tests/nc_header_bounds.rs` holds the estimate to the allocator's measured peak, including
//! on mutated files, and `tests/nc_hostile.rs` holds each shape to a typed error.
//!
//! Refused here, where a real NetCDF writer has no reason to go and the reader's cost cannot be
//! stated:
//! - a group reachable by more than one link, soft and external links, a filtered fractal heap;
//! - a Shared message stored in the file's shared-message table, holding anything but a datatype,
//!   fill value or filter, or whose target's selected message is not the declared type;
//! - a repeated dataspace, datatype, layout, filter pipeline, symbol table, link info or attribute
//!   info message, external data files, an object that is both a group and a dataset, and a chunked
//!   layout whose rank is not the dataspace's;
//! - variable-length data inside another type, a user-defined attribute type with overlapping or
//!   zero-width members, and a variable-length attribute of anything but plain numbers;
//! - an N-bit or scale-offset filter with other filters beside it, and a filter the reader cannot
//!   bound before another stage.

use super::{decode_error, refused, Limits};
use crate::error::VortexError;
use hdf5_reader::btree_v1::collect_btree_v1_leaves_storage;
use hdf5_reader::btree_v2::{collect_btree_v2_records_storage, BTreeV2Header, BTreeV2Record};
use hdf5_reader::filters::{
    FILTER_DEFLATE, FILTER_FLETCHER32, FILTER_NBIT, FILTER_SCALEOFFSET, FILTER_SHUFFLE,
};
use hdf5_reader::fractal_heap::{FractalHeap, FractalHeapDirectBlockCache};
use hdf5_reader::global_heap::GlobalHeapCollection;
use hdf5_reader::io::Cursor;
use hdf5_reader::local_heap::LocalHeap;
use hdf5_reader::messages::attribute::{self, AttributeMessage};
use hdf5_reader::messages::attribute_info::AttributeInfoMessage;
use hdf5_reader::messages::datatype::{Datatype, StringSize, VarLenKind};
use hdf5_reader::messages::filter_pipeline::FilterPipelineMessage;
use hdf5_reader::messages::layout::{ChunkIndexing, DataLayout};
use hdf5_reader::messages::link::{self, LinkMessage, LinkTarget};
use hdf5_reader::messages::link_info::LinkInfoMessage;
use hdf5_reader::messages::shared::SharedMessage;
use hdf5_reader::messages::symbol_table_msg::SymbolTableMessage;
use hdf5_reader::messages::{
    HdfMessage, MSG_DATATYPE, MSG_FILL_VALUE, MSG_FILL_VALUE_OLD, MSG_FILTER_PIPELINE,
};
use hdf5_reader::object_header::ObjectHeader;
use hdf5_reader::storage::{DynStorage, Storage, StorageBuffer};
use hdf5_reader::superblock::Superblock;
use netcdf_reader::nc4::attributes::is_internal_attribute;
use netcdf_reader::{
    NcAttribute, NcCompoundValueField, NcDimension, NcGroup, NcSliceInfo, NcSliceInfoElem, NcValue,
    NcVariable,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Deepest group nesting. `Model::add` holds the same limit, after the reader has recursed.
pub(super) const MAX_GROUP_DEPTH: usize = 32;

/// The reader keeps what it parsed several times over, and all of it is kept while [`Model`] copies
/// it: the header cache, the dataset templates, the `NcGroup` tree and the model. Held to measured
/// peaks by `tests/nc_header_bounds.rs`.
///
/// [`Model`]: super::model::Model
const KEPT_COPIES: u64 = 6;

/// Allocations no count in the file scales: the reader's `with_capacity` of a 16-bit count before
/// the entries are read, in one structure at a time.
pub(super) const TRANSIENT_BYTES: u64 = 32 << 20;

/// Bytes the reader may read while opening a file, for each byte the walk read. It lists a group's
/// links three times and rereads dense attributes for each.
const OPEN_READS_PER_WALK_READ: u64 = 16;

/// What one chunk of a read's selection may cost in the reader's tables: an entry in the target list
/// while it doubles, its offsets, the found entry, and the copy the entry cache keeps. Held to measured
/// peaks by `tests/nc_header_bounds.rs`.
const CHUNK_TABLE_BYTES: u64 = 512;

/// Decoded chunks alive at once while a chunk is read: the stored or inflated bytes, one filter's
/// output, and the cache's copy being replaced.
const LIVE_CHUNKS: u64 = 3;

/// Storage that fails a read once its allowance is used. All the reader's metadata comes through
/// [`Storage::read_range`], so its work is bounded even where it caches results or discards errors.
pub(super) struct Metered {
    bytes: DynStorage,
    base: u64,
    remaining: AtomicU64,
    tripped: AtomicBool,
    /// What the reader has charged since the last [`Metered::allow`].
    charged: AtomicU64,
    max_charged: u64,
    /// What it has been about to decode since then.
    worked: AtomicU64,
    max_worked: u64,
    /// When this read must stop.
    deadline: Mutex<Option<Instant>>,
    stop: Option<Arc<AtomicBool>>,
}

/// How often, in bytes charged, [`Metered::charge`] checks that memory is left. The reader's small
/// allocations are infallible, and one that fails ends the process, so they must never be what
/// finds the process out of memory.
const PROBE_EVERY: u64 = 1 << 20;

/// Memory that must be available at each check. It is far more than what can be allocated between
/// two checks, since the reader charges at least what it keeps of each item.
const PROBE_BYTES: u64 = 16 << 20;

impl Metered {
    pub(super) fn new(bytes: DynStorage, base: u64, allowance: u64) -> Self {
        Metered {
            bytes,
            base,
            remaining: AtomicU64::new(allowance),
            tripped: AtomicBool::new(false),
            charged: AtomicU64::new(0),
            max_charged: u64::MAX,
            worked: AtomicU64::new(0),
            max_worked: u64::MAX,
            deadline: Mutex::new(None),
            stop: None,
        }
    }

    /// With the limits on what the reader may charge and decode.
    pub(super) fn with_limits(mut self, limits: &Limits, stop: Option<Arc<AtomicBool>>) -> Self {
        self.max_charged = limits.max_reader_bytes;
        self.max_worked = limits.max_decoded_bytes_per_read;
        self.stop = stop;
        self
    }

    /// Start a new read: its allowance of the file, and fresh counts.
    pub(super) fn allow(&self, allowance: u64, seconds: Option<u64>) {
        self.remaining.store(allowance, Ordering::SeqCst);
        self.charged.store(0, Ordering::SeqCst);
        self.worked.store(0, Ordering::SeqCst);
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) =
            seconds.map(|s| Instant::now() + Duration::from_secs(s));
    }

    fn refuse(&self, why: String) -> hdf5_reader::error::Error {
        self.tripped.store(true, Ordering::SeqCst);
        hdf5_reader::error::Error::ResourceLimit(why)
    }

    /// Whether a read failed for lack of allowance, whatever the reader did with the error.
    pub(super) fn tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    fn remaining(&self) -> u64 {
        self.remaining.load(Ordering::SeqCst)
    }
}

impl Storage for Metered {
    fn len(&self) -> u64 {
        self.bytes.len().saturating_sub(self.base)
    }

    fn charge(&self, bytes: u64) -> hdf5_reader::error::Result<()> {
        let before = self.charged.fetch_add(bytes, Ordering::SeqCst);
        let after = before.saturating_add(bytes);
        if after > self.max_charged {
            return Err(self.refuse(format!(
                "the reader's structures are over the limit of {} bytes",
                self.max_charged
            )));
        }
        if after / PROBE_EVERY != before / PROBE_EVERY {
            let room = PROBE_BYTES.saturating_add(bytes);
            let mut probe: Vec<u8> = Vec::new();
            if probe
                .try_reserve_exact(usize::try_from(room).unwrap_or(usize::MAX))
                .is_err()
            {
                return Err(self.refuse(format!(
                    "could not allocate {room} bytes for the reader's structures"
                )));
            }
        }
        Ok(())
    }

    fn check_work(&self, bytes: u64) -> hdf5_reader::error::Result<()> {
        let after = self
            .worked
            .fetch_add(bytes, Ordering::SeqCst)
            .saturating_add(bytes);
        if after > self.max_worked {
            return Err(self.refuse(format!(
                "a read decoded {after} bytes, over the limit of {} (Limits::max_decoded_bytes_per_read)",
                self.max_worked
            )));
        }
        if self.stop.as_ref().is_some_and(|s| s.load(Ordering::SeqCst)) {
            return Err(self.refuse("the read was stopped".to_string()));
        }
        let deadline = *self.deadline.lock().unwrap_or_else(|e| e.into_inner());
        if deadline.is_some_and(|d| Instant::now() > d) {
            return Err(self.refuse("the read ran past its time limit".to_string()));
        }
        Ok(())
    }

    fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<StorageBuffer> {
        let wanted = len as u64;
        let mut left = self.remaining.load(Ordering::SeqCst);
        loop {
            let Some(after) = left.checked_sub(wanted) else {
                self.tripped.store(true, Ordering::SeqCst);
                return Err(hdf5_reader::error::Error::ResourceLimit(
                    "metadata read allowance used".to_string(),
                ));
            };
            match self
                .remaining
                .compare_exchange(left, after, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(now) => left = now,
            }
        }
        let offset = offset
            .checked_add(self.base)
            .ok_or(hdf5_reader::error::Error::OffsetOutOfBounds(offset))?;
        // `FileStorage` makes a buffer of the length asked for, which a file can name, infallibly.
        Vec::<u8>::new().try_reserve_exact(len).map_err(|_| {
            hdf5_reader::error::Error::ResourceLimit(format!("could not allocate {len} bytes"))
        })?;
        self.bytes.read_range(offset, len)
    }
}

/// What [`preflight`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Preflight {
    /// Bytes the reader will keep, at most, once the file's metadata is built.
    pub estimate: u64,
    /// Bytes the reader may read while opening the file.
    pub open_reads: u64,
    /// Each chunked dataset, by the object header address the reader reports as `data_offset`.
    pub chunked: HashMap<u64, Chunked>,
}

/// `DEFAULT_CHUNK_CACHE_SLOTS`, the fewest slots the cache has.
const FEWEST_CACHE_SLOTS: usize = 521;

/// The most slots a cache derives. The reader makes a map of this many when it opens the file.
const MOST_CACHE_SLOTS: usize = 1 << 16;

/// The cache's bytes and slots for this file. A chunk that holds several steps is what a cache saves.
/// A step that fits [`Limits::max_sized_chunk_cache_bytes`], with twice its size free now, gets a cache
/// that holds all of it. Holding part of a step gets no hits on a sequential read, so a larger step
/// keeps the floor. A cache takes its bytes only as chunks are read.
pub(super) fn size_chunk_cache(found: &Preflight, limits: &Limits) -> (usize, usize) {
    let mut step = 0u64;
    let mut smallest = u64::MAX;
    for chunked in found.chunked.values() {
        smallest = smallest.min(chunked.chunk_bytes().max(1));
        step = step.max(chunked.step_bytes());
    }
    let mut bytes = limits.chunk_cache_bytes;
    let fits = usize::try_from(step)
        .ok()
        .filter(|step| *step > bytes && *step <= limits.max_sized_chunk_cache_bytes)
        .filter(|step| {
            Vec::<u8>::new()
                .try_reserve_exact(step.saturating_mul(2))
                .is_ok()
        });
    if let Some(step) = fits {
        bytes = step;
    }
    let slots = if limits.chunk_cache_slots > 0 {
        limits.chunk_cache_slots
    } else {
        let wanted = (bytes as u64).checked_div(smallest).unwrap_or(0);
        usize::try_from(wanted)
            .unwrap_or(usize::MAX)
            .clamp(FEWEST_CACHE_SLOTS, MOST_CACHE_SLOTS)
    };
    (bytes, slots)
}

/// A chunked dataset's layout, as `normalize_layout` leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Chunked {
    dims: Vec<u32>,
    /// Filters in its pipeline, each of which decodes a whole chunk.
    stages: u64,
    /// No chunk was ever written, so a read makes fill values and decodes nothing.
    unwritten: bool,
    /// Bytes in one cell, from the datatype, or 8 when the walk did not see one.
    element: u64,
    /// The dataset's own shape, which a read's lengths can only pad.
    shape: Vec<u64>,
}

impl Chunked {
    fn chunk_bytes(&self) -> u64 {
        self.dims
            .iter()
            .fold(self.element, |all, d| all.saturating_mul(u64::from(*d)))
    }

    /// What holding one step of this dataset takes, or 0 when a cache cannot help: time is taken to
    /// be the first of three or more dimensions, and with one step per chunk no step repeats a chunk.
    fn step_bytes(&self) -> u64 {
        if self.unwritten || self.dims.len() < 3 || self.dims[0] < 2 {
            return 0;
        }
        self.shape
            .iter()
            .zip(&self.dims)
            .skip(1)
            .fold(self.chunk_bytes(), |all, (extent, chunk)| {
                all.saturating_mul(extent.div_ceil(u64::from(*chunk)))
            })
    }
}

/// What one read costs, decided from the selection and the lengths the reader reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadPlan {
    /// Bytes for chunk tables and decoded chunks, beyond the cells the read returns.
    pub read_bytes: u64,
}

fn over_limit(estimate: u64, limits: &Limits) -> VortexError {
    refused(
        "NetCDF file",
        format!(
            "metadata needing about {estimate} bytes is over the limit of {}",
            limits.max_metadata_bytes
        ),
    )
}

/// Walk the file's metadata. Called before anything in `netcdf-reader` sees the file.
pub(super) fn preflight(bytes: &DynStorage, limits: &Limits) -> Result<Preflight, VortexError> {
    let head = bytes
        .read_range(0, bytes.len().min(4) as usize)
        .map_err(|e| decode_error(e.to_string()))?;
    let found = match head.as_ref() {
        b"CDF\x01" => classic(bytes.as_ref(), 4, 4, limits),
        b"CDF\x02" => classic(bytes.as_ref(), 4, 8, limits),
        b"CDF\x05" => classic(bytes.as_ref(), 8, 8, limits),
        _ => hdf5(bytes, limits),
    }?;
    if found.estimate > limits.max_metadata_bytes {
        return Err(over_limit(found.estimate, limits));
    }
    Ok(found)
}

// ---------------------------------------------------------------------------------------------
// Classic: the grammar of `classic/header.rs`, counting instead of allocating.
// ---------------------------------------------------------------------------------------------

struct Grammar<'a> {
    data: &'a [u8],
    at: usize,
    /// 4 for CDF-1 and CDF-2, 8 for CDF-5: `read_count`.
    count_width: usize,
    /// `begin`: 4 for CDF-1, 8 otherwise.
    begin_width: usize,
    kept: u64,
    limit: u64,
    /// The bytes ended before the grammar did, or a count asked for more than they hold.
    ran_out: &'a Cell<bool>,
}

const NC_DIMENSION: u32 = 0x0A;
const NC_VARIABLE: u32 = 0x0B;
const NC_ATTRIBUTE: u32 = 0x0C;

impl Grammar<'_> {
    fn truncated(&self) -> VortexError {
        self.ran_out.set(true);
        decode_error(format!("the classic header ends early, at {}", self.at))
    }

    fn take(&mut self, n: usize) -> Result<&[u8], VortexError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| self.truncated())?;
        let out = &self.data[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn word(&mut self) -> Result<u32, VortexError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn count(&mut self) -> Result<u64, VortexError> {
        if self.count_width == 4 {
            return self.word().map(u64::from);
        }
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Add what the reader keeps. Refused at once, so the walk's own work stays small.
    fn keep(&mut self, bytes: u64) -> Result<(), VortexError> {
        self.kept = self.kept.saturating_add(bytes);
        if self.kept.saturating_mul(KEPT_COPIES) > self.limit {
            return Err(refused(
                "NetCDF file",
                format!(
                    "its classic header needs over the metadata limit of {} bytes",
                    self.limit
                ),
            ));
        }
        Ok(())
    }

    /// A list's tag and count, or `None` for `ABSENT`. A count must fit in the remaining bytes.
    fn list(&mut self, tag: u32, entry: u64) -> Result<Option<u64>, VortexError> {
        let found = self.word()?;
        if found == 0 {
            self.count()?;
            return Ok(None);
        }
        if found != tag {
            return Err(decode_error(format!(
                "expected classic header tag {tag:#x}, found {found:#x}"
            )));
        }
        let count = self.count()?;
        let left = (self.data.len() - self.at) as u64;
        if count.checked_mul(entry).is_none_or(|needed| needed > left) {
            return Err(self.truncated());
        }
        Ok(Some(count))
    }

    /// `read_name`.
    fn name(&mut self) -> Result<u64, VortexError> {
        let len = self.count()?;
        let n = usize::try_from(len).map_err(|_| self.truncated())?;
        let padded = n
            .checked_add(n.wrapping_neg() % 4)
            .ok_or_else(|| self.truncated())?;
        self.take(padded)?;
        Ok(len)
    }

    fn attributes(&mut self) -> Result<(), VortexError> {
        let Some(count) = self.list(NC_ATTRIBUTE, 12)? else {
            return Ok(());
        };
        self.keep(count.saturating_mul(size_of::<NcAttribute>() as u64))?;
        for _ in 0..count {
            let name = self.name()?;
            let code = self.word()?;
            let values = self.count()?;
            // nc_type_from_code. The reader refuses anything else.
            let width: u64 = match code {
                1..=2 | 7 => 1,
                3 | 8 => 2,
                4 | 5 | 9 => 4,
                6 | 10 | 11 => 8,
                _ => return Err(decode_error(format!("classic attribute type {code}"))),
            };
            let bytes = values.checked_mul(width).ok_or_else(|| self.truncated())?;
            let n = usize::try_from(bytes).map_err(|_| self.truncated())?;
            let padded = n
                .checked_add(n.wrapping_neg() % 4)
                .ok_or_else(|| self.truncated())?;
            self.take(padded)?;
            self.keep(name.saturating_add(bytes))?;
        }
        Ok(())
    }
}

/// The header is at the file's start and the records are not needed, so only a prefix is read,
/// doubling until the grammar ends inside it. Nearly every byte of a header is kept, so `keep` refuses
/// one longer than the metadata limit before the prefix reaches that length.
fn classic(
    bytes: &dyn Storage,
    count_width: usize,
    begin_width: usize,
    limits: &Limits,
) -> Result<Preflight, VortexError> {
    let most = bytes.len().min(limits.max_metadata_bytes);
    let mut prefix = most.min(1 << 20);
    loop {
        let all = bytes
            .read_range(0, usize::try_from(prefix).unwrap_or(usize::MAX))
            .map_err(|e| decode_error(e.to_string()))?;
        let ran_out = Cell::new(false);
        let walked = classic_prefix(all.as_ref(), count_width, begin_width, limits, &ran_out);
        if walked.is_ok() || !ran_out.get() {
            return walked;
        }
        if prefix < most {
            prefix = prefix.saturating_mul(2).min(most);
        } else if prefix < bytes.len() {
            return Err(over_limit(prefix, limits));
        } else {
            return walked;
        }
    }
}

fn classic_prefix(
    data: &[u8],
    count_width: usize,
    begin_width: usize,
    limits: &Limits,
    ran_out: &Cell<bool>,
) -> Result<Preflight, VortexError> {
    let mut g = Grammar {
        data,
        at: 4,
        count_width,
        begin_width,
        kept: 0,
        limit: limits.max_metadata_bytes,
        ran_out,
    };
    g.count()?; // numrecs
    let mut names: Vec<u64> = Vec::new();
    if let Some(count) = g.list(NC_DIMENSION, 16)? {
        g.keep(count.saturating_mul(size_of::<NcDimension>() as u64))?;
        let count = usize::try_from(count).map_err(|_| g.truncated())?;
        names.try_reserve_exact(count).map_err(|_| {
            refused(
                "NetCDF file",
                format!("could not allocate room to check {count} dimensions"),
            )
        })?;
        for _ in 0..count {
            let name = g.name()?;
            g.count()?;
            g.keep(name)?;
            names.push(name);
        }
    }
    g.attributes()?;
    if let Some(count) = g.list(NC_VARIABLE, 28)? {
        g.keep(count.saturating_mul(size_of::<NcVariable>() as u64))?;
        for _ in 0..count {
            let name = g.name()?;
            g.keep(name)?;
            let rank = g.count()?;
            let left = (g.data.len() - g.at) as u64;
            if rank
                .checked_mul(g.count_width as u64)
                .is_none_or(|needed| needed > left)
            {
                return Err(g.truncated());
            }
            for _ in 0..rank {
                let id = g.count()?;
                let name = usize::try_from(id)
                    .ok()
                    .and_then(|id| names.get(id).copied())
                    .ok_or_else(|| {
                        decode_error(format!("classic variable names dimension {id}"))
                    })?;
                // var_dims.push(dims[dimid].clone()), and `Model::add` copies it again.
                g.keep(name.saturating_add(size_of::<NcDimension>() as u64))?;
            }
            g.attributes()?;
            g.word()?; // nc_type
            g.count()?; // vsize
            g.take(g.begin_width)?;
        }
    }
    // The reader's header is not held twice by itself, but the model copies it. The prefix doubling
    // in `parse_header_from_storage` reads the header at most twice over.
    Ok(Preflight {
        estimate: g.kept.saturating_mul(KEPT_COPIES),
        open_reads: (g.at as u64).saturating_mul(4).saturating_add(1 << 20),
        chunked: HashMap::new(),
    })
}

// ---------------------------------------------------------------------------------------------
// HDF5: the reader's own parsers, one object at a time, keeping sizes.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Group,
    Dataset,
    Other,
}

/// One object header's summary. The parsed header is dropped before the next is read.
struct Object {
    kind: Kind,
    /// Bytes the reader keeps for this object each time a link reaches it.
    kept: u64,
    /// Hard links, for a group.
    children: Vec<u64>,
    rank: u64,
    /// Whether this may be a dimension scale, with the length of its name.
    dimension: Option<u64>,
}

struct Walker<'a> {
    storage: &'a Metered,
    offset_size: u8,
    length_size: u8,
    limits: &'a Limits,
    objects: HashMap<u64, Rc<Object>>,
    /// Object lengths of each global heap collection read.
    heaps: HashMap<u64, Rc<HashMap<u16, u64>>>,
    groups: HashSet<u64>,
    links: u64,
    kept: u64,
    chunked: HashMap<u64, Chunked>,
}

fn hdf5_error(storage: &Metered, error: impl std::fmt::Display) -> VortexError {
    if storage.tripped() {
        return refused(
            "NetCDF file",
            "its metadata needs more reading than the metadata limit allows",
        );
    }
    decode_error(error.to_string())
}

fn hdf5(bytes: &DynStorage, limits: &Limits) -> Result<Preflight, VortexError> {
    // First with no base: `Superblock::parse_from_storage` finds its own, past any user block.
    let located =
        Metered::new(bytes.clone(), 0, limits.max_metadata_bytes).with_limits(limits, None);
    let superblock =
        Superblock::parse_from_storage(&located).map_err(|e| hdf5_error(&located, e))?;
    let root = superblock
        .root_object_header_address()
        .map_err(|e| hdf5_error(&located, e))?;
    let storage = Metered::new(
        bytes.clone(),
        superblock.base_address,
        limits.max_metadata_bytes,
    )
    .with_limits(limits, None);
    let mut walker = Walker {
        storage: &storage,
        offset_size: superblock.offset_size,
        length_size: superblock.length_size,
        limits,
        objects: HashMap::new(),
        heaps: HashMap::new(),
        groups: HashSet::new(),
        links: 0,
        kept: 0,
        chunked: HashMap::new(),
    };
    walker.visit_group(root, 0, 0, 0)?;
    let read = limits.max_metadata_bytes - storage.remaining();
    Ok(Preflight {
        estimate: walker.kept.saturating_mul(KEPT_COPIES),
        open_reads: read
            .saturating_mul(OPEN_READS_PER_WALK_READ)
            .saturating_add(1 << 20),
        chunked: walker.chunked,
    })
}

impl Walker<'_> {
    fn fail(&self, error: impl std::fmt::Display) -> VortexError {
        hdf5_error(self.storage, error)
    }

    fn keep(&mut self, bytes: u64) -> Result<(), VortexError> {
        self.kept = self.kept.saturating_add(bytes);
        let estimate = self.kept.saturating_mul(KEPT_COPIES);
        if estimate > self.limits.max_metadata_bytes {
            return Err(over_limit(estimate, self.limits));
        }
        Ok(())
    }

    /// `inherited` is what the dimensions visible from the parent weigh, and `name` the longest of
    /// their names. A group keeps a copy of all of them, and a variable one per axis.
    fn visit_group(
        &mut self,
        address: u64,
        depth: usize,
        inherited: u64,
        name: u64,
    ) -> Result<(), VortexError> {
        if depth > MAX_GROUP_DEPTH {
            return Err(refused(
                "NetCDF groups",
                format!("nested over {MAX_GROUP_DEPTH} levels"),
            ));
        }
        // A cycle, or a chain of doubled links, reaches a group twice. `build_group_recursive` does
        // not notice, and this also makes the number of visits below the number of links.
        if !self.groups.insert(address) {
            return Err(refused(
                "NetCDF groups",
                format!("the group at {address:#x} is reached by more than one link"),
            ));
        }
        let group = self.object(address)?;
        self.keep(group.kept.saturating_add(size_of::<NcGroup>() as u64))?;
        let mut datasets = Vec::new();
        let mut subgroups = Vec::new();
        let (mut weight, mut longest) = (inherited, name);
        for child in &group.children {
            self.links += 1;
            let most = (self.limits.max_variables as u64).saturating_mul(2);
            if self.links > most {
                return Err(refused(
                    "NetCDF file",
                    format!("over {most} links to objects"),
                ));
            }
            let object = self.object(*child)?;
            match object.kind {
                Kind::Group => subgroups.push(*child),
                Kind::Dataset => {
                    if let Some(len) = object.dimension {
                        // The dimension list, and the maps by address and by id.
                        weight = weight
                            .saturating_add(len.saturating_add(size_of::<NcDimension>() as u64));
                        longest = longest.max(len);
                    }
                    datasets.push(object);
                }
                Kind::Other => {}
            }
        }
        // visible_dimensions, visible_dim_addr_map and visible_dim_id_map, each cloned per group.
        self.keep(weight.saturating_mul(3))?;
        for object in datasets {
            let per_axis = longest.saturating_add(size_of::<NcDimension>() as u64);
            self.keep(
                object
                    .kept
                    .saturating_add(size_of::<NcVariable>() as u64)
                    .saturating_add(object.rank.saturating_mul(per_axis)),
            )?;
        }
        for child in subgroups {
            self.visit_group(child, depth + 1, weight, longest)?;
        }
        Ok(())
    }

    fn object(&mut self, address: u64) -> Result<Rc<Object>, VortexError> {
        if let Some(found) = self.objects.get(&address) {
            return Ok(found.clone());
        }
        let object = Rc::new(self.read_object(address)?);
        self.objects.insert(address, object.clone());
        Ok(object)
    }

    fn read_object(&mut self, address: u64) -> Result<Object, VortexError> {
        let before = self.storage.remaining();
        let header = ObjectHeader::parse_at_storage_with_limit(
            self.storage,
            address,
            self.offset_size,
            self.length_size,
            self.limits.max_metadata_bytes as usize,
        )
        .map_err(|e| self.fail(e))?;
        let mut object = Object {
            kind: Kind::Other,
            kept: (header.messages.len() as u64).saturating_mul(size_of::<HdfMessage>() as u64),
            children: Vec::new(),
            rank: 0,
            dimension: None,
        };
        let mut scale = false;
        let mut name = 0u64;
        let mut note = |attribute: &AttributeMessage, kept: u64| {
            if attribute.name == "CLASS" {
                scale = true;
            }
            if attribute.name == "NAME" {
                name = kept;
            }
        };
        let mut space = None;
        let mut layout = None;
        let mut links = Vec::new();
        let mut link_info = None;
        let mut symbols = None;
        let mut stages = 0u64;
        let mut element = 8u64;
        let mut once = Once::default();
        // `classify_child_header`, on the messages the reader will hold once Shared is resolved.
        let (mut is_group, mut is_dataset, mut has_attribute) = (false, false, false);
        for message in &header.messages {
            match message {
                HdfMessage::Shared(shared) => match self.shared(shared)? {
                    Selected::Datatype(size) => {
                        once.see("datatype")?;
                        element = size;
                    }
                    Selected::FillValue => is_dataset = true,
                    Selected::Filters(count) => {
                        once.see("filter pipeline")?;
                        is_dataset = true;
                        stages = count;
                    }
                },
                HdfMessage::Attribute(a) => {
                    has_attribute = true;
                    let kept = self.attribute(a)?;
                    note(a, kept);
                    object.kept = object.kept.saturating_add(kept);
                }
                HdfMessage::AttributeInfo(info) => {
                    has_attribute = true;
                    once.see("attribute info")?;
                    for a in self.dense_attributes(info)? {
                        let kept = self.attribute(&a)?;
                        note(&a, kept);
                        object.kept = object.kept.saturating_add(kept);
                    }
                }
                HdfMessage::Dataspace(found) => {
                    is_dataset = true;
                    once.see("dataspace")?;
                    object.rank = found.dims.len() as u64;
                    space = Some((found.dims.clone(), found.max_dims.clone()));
                }
                HdfMessage::Datatype(found) => {
                    once.see("datatype")?;
                    element = u64::from(found.size);
                }
                HdfMessage::DataLayout(found) => {
                    is_dataset = true;
                    once.see("data layout")?;
                    layout = Some(found.layout.clone());
                }
                HdfMessage::FilterPipeline(found) => {
                    is_dataset = true;
                    once.see("filter pipeline")?;
                    check_filters(found)?;
                    stages = found.filters.len() as u64;
                }
                // An old and a new fill value are both written by HDF5 1.6 and later.
                HdfMessage::FillValue(_) => is_dataset = true,
                HdfMessage::Link(l) => {
                    is_group = true;
                    links.push(l.clone());
                }
                HdfMessage::LinkInfo(info) => {
                    is_group = true;
                    once.see("link info")?;
                    link_info = Some(info.clone());
                }
                HdfMessage::SymbolTable(table) => {
                    is_group = true;
                    once.see("symbol table")?;
                    symbols = Some(table.clone());
                }
                HdfMessage::GroupInfo(_) => is_group = true,
                // Neither `netcdf-reader` nor the dataset and group paths look at these. What they
                // weigh is in what was read for this object.
                HdfMessage::Nil
                | HdfMessage::Continuation(_)
                | HdfMessage::ObjectHeaderContinuation
                | HdfMessage::ModificationTime(_)
                | HdfMessage::BTreeK(_)
                | HdfMessage::Comment(_)
                | HdfMessage::ReferenceCount(_)
                | HdfMessage::Unknown { .. } => {}
                HdfMessage::ExternalFiles(_) => {
                    return Err(refused("NetCDF file", "a dataset is stored in other files"));
                }
                HdfMessage::SharedTable(_) => {
                    return Err(refused(
                        "NetCDF file",
                        "an object header holds a shared-message table",
                    ));
                }
            }
        }
        drop(header);
        if is_group && is_dataset {
            return Err(refused(
                "NetCDF file",
                format!("the object at {address:#x} is both a group and a dataset"),
            ));
        }
        object.kind = if is_group {
            Kind::Group
        } else if is_dataset {
            Kind::Dataset
        } else if has_attribute {
            Kind::Group
        } else {
            Kind::Other
        };
        if let (Some((dims, max_dims)), Some(layout)) = (&space, &layout) {
            self.chunks(address, dims, max_dims.as_deref(), layout, stages, element)?;
        }
        // `resolve_children_with_link_depth`: a symbol table, else compact and dense links.
        if let Some(table) = &symbols {
            if !links.is_empty() || link_info.is_some() {
                return Err(refused(
                    "NetCDF file",
                    "a group holds a symbol table and links",
                ));
            }
            self.symbol_table(table, &mut object)?;
        } else {
            for l in &links {
                self.link(l, &mut object)?;
            }
            if let Some(info) = &link_info {
                self.dense_links(info, &mut object)?;
            }
        }
        if scale {
            object.dimension = Some(name);
        }
        // Everything read for this object is the most it can keep of the file's bytes.
        let read = before.saturating_sub(self.storage.remaining());
        object.kept = object.kept.saturating_add(read);
        Ok(object)
    }

    /// Keep what [`plan_read`] needs to decide each read. Nothing is refused for its chunk count here:
    /// netCDF-C's default `(1, ny, nx)` chunks give a long record more chunks than any one read takes,
    /// and a read's own selection is what the reader lists.
    fn chunks(
        &mut self,
        object: u64,
        dims: &[u64],
        max_dims: Option<&[u64]>,
        layout: &DataLayout,
        stages: u64,
        element: u64,
    ) -> Result<(), VortexError> {
        let DataLayout::Chunked {
            address,
            dims: chunk_dims,
            chunk_indexing,
            ..
        } = layout
        else {
            return Ok(());
        };
        // `normalize_layout`: a legacy layout's last entry is the element's size.
        let chunk_dims = if chunk_dims.len() == dims.len() + 1 {
            &chunk_dims[..dims.len()]
        } else {
            &chunk_dims[..]
        };
        if chunk_dims.len() != dims.len() || chunk_dims.contains(&0) {
            return Err(refused(
                "NetCDF file",
                format!(
                    "a dataset of rank {} has chunks of shape {chunk_dims:?}",
                    dims.len()
                ),
            ));
        }
        // `chunks_per_dim`.
        let counts = |shape: &[u64]| -> Vec<u64> {
            shape
                .iter()
                .zip(chunk_dims)
                .map(|(extent, chunk)| extent.div_ceil(u64::from(*chunk)))
                .collect()
        };
        let unwritten = Cursor::is_undefined_offset(*address, self.offset_size);
        if matches!(chunk_indexing, Some(ChunkIndexing::FixedArray { .. })) && !unwritten {
            self.fixed_array(*address, &counts(max_dims.unwrap_or(dims)))?;
        }
        self.chunked.insert(
            object,
            Chunked {
                dims: chunk_dims.to_vec(),
                stages,
                unwritten,
                element,
                shape: dims.to_vec(),
            },
        );
        Ok(())
    }

    /// `FaGeometry::new` takes its entry count from the array's header, and `page_offsets` makes a
    /// table of that many pages with the bitmap as its only check. HDF5 sizes the array from the
    /// dataset (`H5D__farray_idx_create`), so more entries than chunks is not a real array.
    fn fixed_array(&mut self, address: u64, counts: &[u64]) -> Result<(), VortexError> {
        let width = 8 + usize::from(self.length_size);
        let bytes = self
            .storage
            .read_range(address, width)
            .map_err(|e| self.fail(e))?;
        let mut cursor = Cursor::new(bytes.as_ref());
        cursor.set_position(7);
        let page_bits = cursor.read_u8().map_err(|e| self.fail(e))?;
        let entries = cursor
            .read_length(self.length_size)
            .map_err(|e| self.fail(e))?;
        let chunks = counts
            .iter()
            .fold(1u64, |all, count| all.saturating_mul(*count));
        if entries > chunks {
            return Err(refused(
                "NetCDF file",
                format!("a fixed array of chunks declares {entries} entries for {chunks} chunks"),
            ));
        }
        let pages = entries
            .checked_shr(u32::from(page_bits))
            .unwrap_or(0)
            .saturating_add(1);
        self.keep(pages.saturating_mul(size_of::<Option<u64>>() as u64))
    }

    /// `resolve_shared_messages_storage_with_sohm` parses the target header for every reference, and
    /// `select_shared_message` returns its first real message when none has the declared type. So
    /// what a Shared message becomes is read from its target, and it must be what it declares.
    fn shared(&mut self, shared: &SharedMessage) -> Result<Selected, VortexError> {
        let refuse = |why: String| refused("NetCDF file", why);
        let (message_type, address) = match shared {
            SharedMessage::SharedInSohm { .. } => {
                return Err(refuse(
                    "it stores a message in the file's shared-message table".to_string(),
                ))
            }
            SharedMessage::SharedInOhdr {
                message_type,
                address,
            } => (*message_type, *address),
        };
        // Datatype, fill value (old and new) and filter pipeline: what HDF5 commits.
        if !matches!(
            message_type,
            MSG_DATATYPE | MSG_FILL_VALUE_OLD | MSG_FILL_VALUE | MSG_FILTER_PIPELINE
        ) {
            return Err(refuse(format!(
                "it shares a message of type {message_type:#x}"
            )));
        }
        // The reader keeps an unreadable target's reference, and a dataset then lacks the message.
        let target = ObjectHeader::parse_at_storage_with_limit(
            self.storage,
            address,
            self.offset_size,
            self.length_size,
            self.limits.max_metadata_bytes as usize,
        )
        .map_err(|e| {
            if self.storage.tripped() {
                self.fail(e)
            } else {
                refuse(format!("a shared message's target at {address:#x}: {e}"))
            }
        })?;
        // `select_shared_message`: the first of the declared type, whatever comes before it.
        for message in &target.messages {
            match (message_type, message) {
                (MSG_DATATYPE, HdfMessage::Datatype(found)) => {
                    return Ok(Selected::Datatype(u64::from(found.size)));
                }
                (MSG_FILL_VALUE | MSG_FILL_VALUE_OLD, HdfMessage::FillValue(_)) => {
                    return Ok(Selected::FillValue);
                }
                (MSG_FILTER_PIPELINE, HdfMessage::FilterPipeline(found)) => {
                    check_filters(found)?;
                    return Ok(Selected::Filters(found.filters.len() as u64));
                }
                _ => {}
            }
        }
        Err(refuse(format!(
            "a shared message of type {message_type:#x} has a target at {address:#x} without one"
        )))
    }

    /// What the reader keeps for one attribute, by `convert_attribute_value_with_source`.
    fn attribute(&mut self, attribute: &AttributeMessage) -> Result<u64, VortexError> {
        self.attribute_as(attribute, &attribute.datatype)
    }

    fn attribute_as(
        &mut self,
        attribute: &AttributeMessage,
        datatype: &Datatype,
    ) -> Result<u64, VortexError> {
        let raw = attribute.raw_data.len() as u64;
        match datatype {
            Datatype::VarLen { base, kind, .. } => {
                if matches!(kind, VarLenKind::String) && base.is_vlen_string_base() {
                    return self.references(attribute, 1);
                }
                // The dimension lists are read by address and never decoded as values.
                if is_internal_attribute(&attribute.name) {
                    return self.references(attribute, 1);
                }
                if !is_plain(base) {
                    return Err(refused(
                        "NetCDF attribute",
                        format!(
                            "{} is variable-length data of a type this reader does not size",
                            attribute.name
                        ),
                    ));
                }
                // `decode_vlen_values` makes a value of each element in the heap object.
                let per_byte = (size_of::<NcValue>() as u64).div_ceil(element_width(base));
                self.references(attribute, per_byte)
            }
            Datatype::String {
                size: StringSize::Variable,
                ..
            } => self.references(attribute, 1),
            Datatype::String {
                size: StringSize::Fixed(len),
                ..
            } => {
                // `read_strings` makes a `String` per element present, to the end of the payload.
                let elements = raw.div_ceil(u64::from(*len).max(1)).max(1);
                Ok(raw.saturating_add(elements.saturating_mul(size_of::<String>() as u64)))
            }
            Datatype::FixedPoint { .. } | Datatype::FloatingPoint { .. } => Ok(raw),
            // Read as its base's integers.
            Datatype::Enum { base, .. } => self.attribute_as(attribute, base),
            // No NetCDF type: an error in strict mode unless the attribute is internal and skipped.
            Datatype::Bitfield { .. } | Datatype::Reference { .. } => Ok(raw),
            Datatype::Compound { .. } | Datatype::Opaque { .. } | Datatype::Array { .. } => {
                if holds_references(datatype) {
                    return Err(refused(
                        "NetCDF attribute",
                        format!(
                            "{} holds variable-length data inside a type",
                            attribute.name
                        ),
                    ));
                }
                check_members(datatype).map_err(|why| {
                    refused("NetCDF attribute", format!("{}: {why}", attribute.name))
                })?;
                // `read_attribute_values` decodes every element the dataspace declares, which must
                // be there to read.
                let declared = attribute
                    .dataspace
                    .num_elements()
                    .map_err(|e| self.fail(e))?;
                if declared.saturating_mul(element_width(datatype)) > raw {
                    return Err(refused(
                        "NetCDF attribute",
                        format!(
                            "{} declares {declared} elements in {raw} bytes",
                            attribute.name
                        ),
                    ));
                }
                Ok(raw.saturating_add(declared.saturating_mul(value_cost(datatype))))
            }
        }
    }

    /// `decoded_vlen_strings_storage` and `resolve_vlen_reference_bytes` copy the heap object for
    /// every reference, and the reader's cache means the copies cost no reads. `per_byte` is what
    /// the reader makes of each byte of the object.
    fn references(
        &mut self,
        attribute: &AttributeMessage,
        per_byte: u64,
    ) -> Result<u64, VortexError> {
        let width = 4 + usize::from(self.offset_size) + 4;
        let declared = attribute
            .dataspace
            .num_elements()
            .map_err(|e| self.fail(e))?;
        let raw = &attribute.raw_data;
        let n = (raw.len() / width) as u64;
        let mut kept = raw.len() as u64;
        for i in 0..declared.min(n) as usize {
            let mut cursor = Cursor::new(&raw[i * width..]);
            let parts = (|| {
                let _ = cursor.read_u32_le()?;
                let address = cursor.read_offset(self.offset_size)?;
                let index = cursor.read_u32_le()? as u16;
                Ok::<_, hdf5_reader::error::Error>((address, index))
            })();
            let (address, index) = parts.map_err(|e| self.fail(e))?;
            kept = kept.saturating_add(size_of::<String>() as u64);
            if Cursor::is_undefined_offset(address, self.offset_size) || index == 0 {
                continue;
            }
            let lengths = self.heap(address)?;
            let length = lengths.get(&index).copied().unwrap_or(0);
            kept = kept.saturating_add(length.saturating_mul(per_byte));
            let estimate = kept.saturating_mul(KEPT_COPIES);
            if estimate > self.limits.max_metadata_bytes {
                return Err(over_limit(estimate, self.limits));
            }
        }
        Ok(kept)
    }

    fn heap(&mut self, address: u64) -> Result<Rc<HashMap<u16, u64>>, VortexError> {
        if let Some(found) = self.heaps.get(&address) {
            return Ok(found.clone());
        }
        let lengths = match GlobalHeapCollection::parse_at_storage(
            self.storage,
            address,
            self.offset_size,
            self.length_size,
        ) {
            Ok(collection) => collection
                .objects
                .iter()
                .map(|o| (o.index, o.data.len() as u64))
                .collect(),
            Err(error) if self.storage.tripped() => return Err(self.fail(error)),
            // The reader's decode falls back or fails on it, and copies nothing.
            Err(_) => HashMap::new(),
        };
        let lengths = Rc::new(lengths);
        self.heaps.insert(address, lengths.clone());
        Ok(lengths)
    }

    fn fractal_heap(&self, address: u64) -> Result<FractalHeap, VortexError> {
        let heap = FractalHeap::parse_at_storage(
            self.storage,
            address,
            self.offset_size,
            self.length_size,
        )
        .map_err(|e| self.fail(e))?;
        // Inflated bytes would not pass through the meter. netCDF-C does not filter these heaps.
        if !heap.io_filter_info.is_empty() {
            return Err(refused(
                "NetCDF file",
                "a fractal heap of metadata is filtered",
            ));
        }
        Ok(heap)
    }

    fn records(&self, address: u64) -> Result<Vec<BTreeV2Record>, VortexError> {
        let header = BTreeV2Header::parse_at_storage(
            self.storage,
            address,
            self.offset_size,
            self.length_size,
        )
        .map_err(|e| self.fail(e))?;
        collect_btree_v2_records_storage(
            self.storage,
            &header,
            self.offset_size,
            self.length_size,
            None,
            &[],
            None,
        )
        .map_err(|e| self.fail(e))
    }

    /// `load_dense_attribute_messages_storage`.
    fn dense_attributes(
        &mut self,
        info: &AttributeInfoMessage,
    ) -> Result<Vec<AttributeMessage>, VortexError> {
        if Cursor::is_undefined_offset(info.fractal_heap_address, self.offset_size) {
            return Ok(Vec::new());
        }
        let heap = self.fractal_heap(info.fractal_heap_address)?;
        // `load_dense_attribute_records_storage`: creation order if it has it, else by name.
        let mut records = None;
        let mut failure = None;
        for address in info
            .btree_creation_order_address
            .into_iter()
            .chain([info.btree_name_index_address])
        {
            if Cursor::is_undefined_offset(address, self.offset_size) {
                continue;
            }
            match self.records(address) {
                Ok(found) => {
                    records = Some(found);
                    break;
                }
                Err(error) => failure = Some(error),
            }
        }
        let Some(records) = records else {
            return match failure {
                Some(error) => Err(error),
                None => Ok(Vec::new()),
            };
        };
        let mut out = Vec::new();
        for record in records {
            let (BTreeV2Record::AttributeNameHash { heap_id, .. }
            | BTreeV2Record::AttributeCreationOrder { heap_id, .. }) = record
            else {
                continue;
            };
            let bytes = heap
                .get_object_storage_with_registry(
                    &heap_id,
                    self.storage,
                    self.offset_size,
                    self.length_size,
                    None,
                )
                .map_err(|e| self.fail(e))?;
            let mut cursor = Cursor::new(&bytes);
            out.push(
                attribute::parse(&mut cursor, self.offset_size, self.length_size, bytes.len())
                    .map_err(|e| self.fail(e))?,
            );
        }
        Ok(out)
    }

    fn link(&mut self, link: &LinkMessage, object: &mut Object) -> Result<(), VortexError> {
        match &link.target {
            LinkTarget::Hard { address } => {
                object.children.push(*address);
                Ok(())
            }
            LinkTarget::Soft { .. } => Err(refused(
                "NetCDF file",
                format!("{} is a soft link", link.name),
            )),
            LinkTarget::External { .. } => Err(refused(
                "NetCDF file",
                format!("{} is an external link", link.name),
            )),
        }
    }

    /// `resolve_dense_links_storage`.
    fn dense_links(
        &mut self,
        info: &LinkInfoMessage,
        object: &mut Object,
    ) -> Result<(), VortexError> {
        if Cursor::is_undefined_offset(info.fractal_heap_address, self.offset_size) {
            return Ok(());
        }
        let heap = self.fractal_heap(info.fractal_heap_address)?;
        let records = self.records(info.btree_name_index_address)?;
        let mut cache = FractalHeapDirectBlockCache::default();
        for record in records {
            let (BTreeV2Record::LinkNameHash { heap_id, .. }
            | BTreeV2Record::CreationOrder { heap_id, .. }) = record
            else {
                continue;
            };
            let bytes = heap
                .get_object_storage_cached_with_registry(
                    &heap_id,
                    self.storage,
                    self.offset_size,
                    self.length_size,
                    &mut cache,
                    None,
                )
                .map_err(|e| self.fail(e))?;
            let mut cursor = Cursor::new(&bytes);
            let message = link::parse(&mut cursor, self.offset_size, self.length_size, bytes.len())
                .map_err(|e| self.fail(e))?;
            self.link(&message, object)?;
        }
        Ok(())
    }

    /// `read_old_style_group_storage`. The reader copies each entry's name out of the heap, so
    /// many entries can share one long name.
    fn symbol_table(
        &mut self,
        table: &SymbolTableMessage,
        object: &mut Object,
    ) -> Result<(), VortexError> {
        use hdf5_reader::symbol_table::SymbolTableNode;
        let heap = LocalHeap::parse_at_storage(
            self.storage,
            table.heap_address,
            self.offset_size,
            self.length_size,
        )
        .map_err(|e| self.fail(e))?;
        let leaves = collect_btree_v1_leaves_storage(
            self.storage,
            table.btree_address,
            self.offset_size,
            self.length_size,
            None,
            &[],
            None,
        )
        .map_err(|e| self.fail(e))?;
        let mut segment = None;
        for (_, address) in &leaves {
            let width = 2 * usize::from(self.offset_size) + 4 + 4 + 16;
            let prefix = self
                .storage
                .read_range(*address, 8)
                .map_err(|e| self.fail(e))?;
            let p = prefix.as_ref();
            let entries = usize::from(u16::from_le_bytes([p[6], p[7]]));
            let bytes = self
                .storage
                .read_range(*address, 8 + entries * width)
                .map_err(|e| self.fail(e))?;
            let mut cursor = Cursor::new(bytes.as_ref());
            let node = SymbolTableNode::parse(&mut cursor, self.offset_size, self.length_size)
                .map_err(|e| self.fail(e))?;
            for entry in &node.entries {
                if entry.cache_type == 2 {
                    return Err(refused("NetCDF file", "a group holds a soft link"));
                }
                if segment.is_none() {
                    let len = usize::try_from(heap.data_segment_size).map_err(|_| {
                        decode_error("a local heap is larger than this platform addresses")
                    })?;
                    segment = Some(
                        self.storage
                            .read_range(heap.data_segment_address, len)
                            .map_err(|e| self.fail(e))?,
                    );
                }
                let text = segment.as_ref().map_or(&[][..], |s| s.as_ref());
                let name_at = usize::try_from(entry.link_name_offset).unwrap_or(usize::MAX);
                let name = text
                    .get(name_at..)
                    .and_then(|rest| rest.iter().position(|b| *b == 0))
                    .ok_or_else(|| decode_error("a group entry has no name in its heap"))?;
                object.kept = object
                    .kept
                    .saturating_add(name as u64)
                    .saturating_add(size_of::<String>() as u64);
                object.children.push(entry.object_header_address);
            }
        }
        Ok(())
    }
}

/// `apply_pipeline_with_limit` has no size for what these two produce, so the stage before them runs
/// up to 4 GiB and the stage after copies it. netCDF-C writes neither, and alone they are bounded.
/// `encoded_size_bound` has no answer for any filter but the ones named here either, and a stage is
/// unbounded when such a filter comes earlier in the pipeline, as szip before deflate would.
fn check_filters(pipeline: &FilterPipelineMessage) -> Result<(), VortexError> {
    let unbounded = pipeline
        .filters
        .iter()
        .any(|f| f.id == FILTER_NBIT || f.id == FILTER_SCALEOFFSET);
    if unbounded && pipeline.filters.len() > 1 {
        return Err(refused(
            "NetCDF file",
            "an N-bit or scale-offset filter has other filters beside it",
        ));
    }
    let known = |id: u16| matches!(id, FILTER_DEFLATE | FILTER_SHUFFLE | FILTER_FLETCHER32);
    let last = pipeline.filters.len().saturating_sub(1);
    if let Some(filter) = pipeline.filters[..last].iter().find(|f| !known(f.id)) {
        return Err(refused(
            "NetCDF file",
            format!(
                "filter {} comes before another, so that stage has no limit",
                filter.id
            ),
        ));
    }
    Ok(())
}

/// What `read_attribute_values` builds for one element, field names included.
fn value_cost(datatype: &Datatype) -> u64 {
    let value = size_of::<NcValue>() as u64;
    match datatype {
        Datatype::Compound { fields, .. } => fields.iter().fold(value, |all, field| {
            all.saturating_add(size_of::<NcCompoundValueField>() as u64)
                .saturating_add(field.name.len() as u64)
                .saturating_add(value_cost(&field.datatype))
        }),
        Datatype::Array { base, dims } => {
            let each = value_cost(base);
            let count = dims.iter().fold(1u64, |all, d| all.saturating_mul(*d));
            value
                .saturating_add((dims.len() * size_of::<u64>()) as u64)
                .saturating_add(count.saturating_mul(each))
        }
        Datatype::Enum { members, .. } => {
            let longest = members.iter().map(|m| m.name.len()).max().unwrap_or(0);
            value.saturating_add(longest as u64)
        }
        Datatype::String {
            size: StringSize::Fixed(len),
            ..
        } => value.saturating_add(u64::from(*len)),
        Datatype::Opaque { size, .. } => value.saturating_add(u64::from(*size)),
        _ => value,
    }
}

/// The members of a user-defined type must take up the bytes they read. Overlapping members let one
/// byte make any number of values, and netCDF-C does not write them. An array of elements with no
/// width reaches `Vec::with_capacity(count)` in `decode_value` with nothing to check it against.
fn check_members(datatype: &Datatype) -> Result<(), String> {
    match datatype {
        Datatype::Compound { size, fields } => {
            let mut spans = Vec::new();
            spans
                .try_reserve_exact(fields.len())
                .map_err(|_| "too many members".to_string())?;
            for field in fields {
                check_members(&field.datatype)?;
                let start = u64::from(field.byte_offset);
                let width = exact_width(&field.datatype);
                if width == 0 {
                    return Err(format!("member {} has no width", field.name));
                }
                let end = start.saturating_add(width);
                if end > u64::from(*size) {
                    return Err(format!("member {} is past the type's end", field.name));
                }
                spans.push((start, end));
            }
            spans.sort_unstable();
            if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
                return Err("its type has overlapping members".to_string());
            }
            Ok(())
        }
        Datatype::Array { base, .. } => {
            check_members(base)?;
            if exact_width(base) == 0 || exact_width(datatype) == 0 {
                return Err("an array's elements have no width".to_string());
            }
            Ok(())
        }
        Datatype::Enum { base, .. } => check_members(base),
        _ => Ok(()),
    }
}

/// Decide one read before the reader lists a chunk. `lengths` are what the reader reports for the
/// variable, which `nc4/mod.rs` `dataset` also pads the dataset to, so they are the extents it
/// computes chunks from. The selection's chunks are what `read_chunked_slice` lists and decodes.
pub(super) fn plan_read(
    found: &Chunked,
    lengths: &[u64],
    selection: &NcSliceInfo,
    element: u64,
    limits: &Limits,
    cache_holds: u64,
) -> Result<ReadPlan, String> {
    let nothing = ReadPlan { read_bytes: 0 };
    if found.unwritten {
        return Ok(nothing);
    }
    if lengths.len() != found.dims.len() || selection.selections.len() != found.dims.len() {
        // The reader's error to give: it validates the ranks before listing anything.
        return Ok(nothing);
    }
    let mut chunks = 1u64;
    for ((select, length), chunk) in selection.selections.iter().zip(lengths).zip(&found.dims) {
        let chunk = u64::from(*chunk);
        let (first, last) = match select {
            NcSliceInfoElem::Index(i) => (*i, *i),
            NcSliceInfoElem::Slice { start, end, .. } => {
                let end = (*end).min(*length);
                if *start >= end {
                    return Ok(nothing);
                }
                (*start, end - 1)
            }
        };
        let (first, last) = (first / chunk, last / chunk);
        chunks = chunks.saturating_mul(last - first + 1);
    }
    if chunks > limits.max_chunks_per_read {
        return Err(format!(
            "it would take {chunks} chunks, over the limit of {}",
            limits.max_chunks_per_read
        ));
    }
    let chunk_bytes = found
        .dims
        .iter()
        .fold(element, |all, d| all.saturating_mul(u64::from(*d)));
    let decoded = chunks
        .saturating_mul(chunk_bytes)
        .saturating_mul(found.stages.max(1));
    if decoded > limits.max_decoded_bytes_per_read {
        return Err(format!(
            "it would decode {decoded} bytes in {chunks} chunks, over the limit of {} (Limits::max_decoded_bytes_per_read)",
            limits.max_decoded_bytes_per_read
        ));
    }
    // `load_chunk_data` refuses a chunk over max_chunk_bytes.
    let chunk_bytes = chunk_bytes.min(limits.max_chunk_bytes as u64);
    // What the cache already holds is committed, and a full cache replaces its chunks: it grows by
    // what this read adds, up to the room it has left.
    let cached = chunk_bytes
        .saturating_mul(chunks)
        .min((limits.chunk_cache_bytes as u64).saturating_sub(cache_holds));
    let read_bytes = chunks
        .saturating_mul(CHUNK_TABLE_BYTES)
        .saturating_add(chunk_bytes.saturating_mul(LIVE_CHUNKS))
        .saturating_add(cached);
    Ok(ReadPlan { read_bytes })
}

/// Which message a Shared message becomes.
enum Selected {
    Datatype(u64),
    FillValue,
    Filters(u64),
}

/// `Dataset::from_parsed_header` keeps the last of each type and other paths the first, so a header
/// holding two of one is not a header the walk can speak for.
#[derive(Default)]
struct Once {
    seen: Vec<&'static str>,
}

impl Once {
    fn see(&mut self, what: &'static str) -> Result<(), VortexError> {
        if self.seen.contains(&what) {
            return Err(refused(
                "NetCDF file",
                format!("an object header holds more than one {what} message"),
            ));
        }
        self.seen.push(what);
        Ok(())
    }
}

fn holds_references(datatype: &Datatype) -> bool {
    match datatype {
        Datatype::VarLen { .. }
        | Datatype::String {
            size: StringSize::Variable,
            ..
        } => true,
        Datatype::Compound { fields, .. } => fields.iter().any(|f| holds_references(&f.datatype)),
        Datatype::Array { base, .. } | Datatype::Enum { base, .. } => holds_references(base),
        _ => false,
    }
}

/// Numbers, read with `read_1d`.
fn is_plain(datatype: &Datatype) -> bool {
    match datatype {
        Datatype::FixedPoint { .. } | Datatype::FloatingPoint { .. } => true,
        Datatype::Enum { base, .. } => is_plain(base),
        _ => false,
    }
}

/// `value_size`, for a type holding no references.
fn exact_width(datatype: &Datatype) -> u64 {
    match datatype {
        Datatype::FixedPoint { size, .. }
        | Datatype::FloatingPoint { size, .. }
        | Datatype::Bitfield { size, .. }
        | Datatype::Reference { size, .. } => u64::from(*size),
        Datatype::String {
            size: StringSize::Fixed(len),
            ..
        } => u64::from(*len),
        Datatype::Compound { size, .. } | Datatype::Opaque { size, .. } => u64::from(*size),
        Datatype::Enum { base, .. } => exact_width(base),
        Datatype::Array { base, dims } => dims
            .iter()
            .fold(exact_width(base), |all, d| all.saturating_mul(*d)),
        Datatype::String { .. } | Datatype::VarLen { .. } => 0,
    }
}

/// A zero-width element still makes a value, so it counts as one byte.
fn element_width(datatype: &Datatype) -> u64 {
    exact_width(datatype).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_does_not_reserve_again_what_the_cache_holds() {
        // 1,024 chunks of 1 MiB under a cache of 1 GiB. A reservation of the whole cache on top of a
        // full one showed a 19.9 GB peak for a 10.3 GB cache, and refused a step that fitted.
        let found = Chunked {
            dims: vec![1, 1024],
            stages: 1,
            unwritten: false,
            element: 1024,
            shape: vec![4, 1048576],
        };
        let limits = Limits {
            chunk_cache_bytes: 1 << 30,
            ..Limits::default()
        };
        let selection = NcSliceInfo::all(2);
        let plan = |held| {
            plan_read(&found, &[4, 1048576], &selection, 1024, &limits, held)
                .unwrap()
                .read_bytes
        };
        assert_eq!(plan(0) - plan(1 << 30), 1 << 30);
        assert_eq!(plan(1 << 30), plan(2 << 30));
    }

    #[test]
    fn a_cache_with_room_left_grows_by_all_a_read_adds() {
        // The same read under a cache far larger than it. What the cache already holds leaves the
        // room for this read, so the reservation is the same as for an empty cache.
        let found = Chunked {
            dims: vec![1, 1024],
            stages: 1,
            unwritten: false,
            element: 1024,
            shape: vec![4, 1048576],
        };
        let limits = Limits {
            chunk_cache_bytes: 1 << 40,
            ..Limits::default()
        };
        let selection = NcSliceInfo::all(2);
        let plan = |held| {
            plan_read(&found, &[4, 1048576], &selection, 1024, &limits, held)
                .unwrap()
                .read_bytes
        };
        assert_eq!(plan(1 << 30), plan(0));
        // A cache a read would overfill grows only by the room it has left.
        let full = (1u64 << 40) - (1 << 20);
        assert_eq!(plan(0) - plan(full), plan(0) - plan(1 << 40) - (1 << 20));
    }
}

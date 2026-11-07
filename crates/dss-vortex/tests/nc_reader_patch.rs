//! `vendor/hdf5-reader` and `vendor/netcdf-reader` on their own, with no walk before them: what each
//! patch in `vendor/PATCHES.md` makes the reader do, through its public API.

mod nc_probes;
mod nc_raw;

use hdf5_reader::error::{Error, Result};
use hdf5_reader::object_header::ObjectHeader;
use hdf5_reader::storage::{BytesStorage, Storage, StorageBuffer};
use netcdf_reader::{NcFile, NcSliceInfo};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A storage that charges to a limit and counts the work it is told of.
struct Counted {
    bytes: BytesStorage,
    charged: AtomicU64,
    limit: u64,
    work_calls: AtomicU64,
    worked: AtomicU64,
    max_work_calls: u64,
}

impl Counted {
    fn new(file: Vec<u8>, limit: u64) -> Arc<Self> {
        Arc::new(Counted {
            bytes: BytesStorage::new(file),
            charged: AtomicU64::new(0),
            limit,
            work_calls: AtomicU64::new(0),
            worked: AtomicU64::new(0),
            max_work_calls: u64::MAX,
        })
    }

    fn stopping_after(file: Vec<u8>, calls: u64) -> Arc<Self> {
        Arc::new(Counted {
            bytes: BytesStorage::new(file),
            charged: AtomicU64::new(0),
            limit: u64::MAX,
            work_calls: AtomicU64::new(0),
            worked: AtomicU64::new(0),
            max_work_calls: calls,
        })
    }
}

impl Storage for Counted {
    fn len(&self) -> u64 {
        self.bytes.len()
    }

    fn read_range(&self, offset: u64, len: usize) -> Result<StorageBuffer> {
        self.bytes.read_range(offset, len)
    }

    fn charge(&self, bytes: u64) -> Result<()> {
        let after = self.charged.fetch_add(bytes, Ordering::SeqCst) + bytes;
        if after > self.limit {
            return Err(Error::ResourceLimit("over the test's budget".to_string()));
        }
        Ok(())
    }

    fn check_work(&self, bytes: u64) -> Result<()> {
        self.worked.fetch_add(bytes, Ordering::SeqCst);
        let calls = self.work_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if calls > self.max_work_calls {
            return Err(Error::ResourceLimit("stopped by the test".to_string()));
        }
        Ok(())
    }
}

fn hostile(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/public/netcdf_hostile")
            .join(name),
    )
    .unwrap()
}

/// Open a chunk-index fixture and read its `data`, as far as the file allows.
fn open_and_read(storage: Arc<Counted>) -> netcdf_reader::Result<u64> {
    let file = NcFile::from_storage(storage)?;
    let rank = file.variable("data")?.dimensions.len();
    let array = file.read_variable_slice::<f32>("data", &NcSliceInfo::all(rank))?;
    Ok(array.len() as u64)
}

fn is_over_budget(result: netcdf_reader::Result<u64>) -> bool {
    match result {
        Err(error) => error.to_string().contains("budget"),
        Ok(_) => false,
    }
}

/// N-bit client data: the parameter count, then `words`.
fn nbit(words: &[u32]) -> Vec<u32> {
    let mut all = vec![words.len() as u32 + 1];
    all.extend_from_slice(words);
    all
}

const NBIT_COMPOUND: u32 = 3;
const NBIT_NOOP: u32 = 4;
const NBIT_ARRAY: u32 = 2;

#[test]
fn n_bit_descriptors_that_consume_nothing_are_refused_before_any_loop() {
    use hdf5_reader::filters::nbit::decompress_with_limit;
    let limit = 1 << 28;
    // The re-check's no-op of size 0.
    let noop = nbit(&[0, 1 << 30, NBIT_NOOP, 0]);
    let started = std::time::Instant::now();
    let error = decompress_with_limit(&[], &noop, limit).unwrap_err();
    assert!(error.to_string().contains("no size"), "{error}");
    // Its 5,000 empty members in a compound of size 0.
    let mut empty = vec![0, 1 << 30, NBIT_COMPOUND, 0, 5000];
    for _ in 0..5000 {
        empty.extend_from_slice(&[0, NBIT_NOOP, 0]);
    }
    let error = decompress_with_limit(&[], &nbit(&empty), limit).unwrap_err();
    assert!(error.to_string().contains("no size"), "{error}");
    // Suspected in the re-check: a compound of size 1 keeps the output limit happy.
    let mut padded = vec![0, 1 << 20, NBIT_COMPOUND, 1, 5001, 0, NBIT_NOOP, 1];
    for _ in 0..5000 {
        padded.extend_from_slice(&[0, NBIT_NOOP, 0]);
    }
    let error = decompress_with_limit(&[1; 1 << 20], &nbit(&padded), limit).unwrap_err();
    assert!(error.to_string().contains("no size"), "{error}");
    // Suspected in the re-check: compounds of size 0 inside compounds.
    let mut nested = vec![0, 1 << 30];
    for _ in 0..200 {
        nested.extend_from_slice(&[NBIT_COMPOUND, 0, 1, 0]);
    }
    nested.extend_from_slice(&[NBIT_NOOP, 0]);
    let error = decompress_with_limit(&[], &nbit(&nested), limit).unwrap_err();
    assert!(error.to_string().contains("no size"), "{error}");
    assert!(started.elapsed().as_secs() < 2, "{:?}", started.elapsed());
}

#[test]
fn n_bit_work_per_element_is_bounded_by_the_element() {
    use hdf5_reader::filters::nbit::decompress_with_limit;
    // 5,000 members of a byte at one offset, in a record of one byte: reads 5,000 bytes per byte out.
    let mut overlapping = vec![0, 1, NBIT_COMPOUND, 1, 5000];
    for _ in 0..5000 {
        overlapping.extend_from_slice(&[0, NBIT_NOOP, 1]);
    }
    let error = decompress_with_limit(&[1; 5000], &nbit(&overlapping), 1 << 28).unwrap_err();
    assert!(
        error.to_string().contains("more than the record"),
        "{error}"
    );
    // An array of an array of the same size, thousands deep, costs each element that many calls.
    let mut deep = vec![0, 1];
    for _ in 0..1000 {
        deep.extend_from_slice(&[NBIT_ARRAY, 1]);
    }
    deep.extend_from_slice(&[NBIT_NOOP, 1]);
    let error = decompress_with_limit(&[1; 8], &nbit(&deep), 1 << 28).unwrap_err();
    assert!(error.to_string().contains("nested"), "{error}");
    // What HDF5 writes still reads: records of a byte and an array of two.
    let good = nbit(&[
        0,
        2,
        NBIT_COMPOUND,
        3,
        2,
        0,
        NBIT_NOOP,
        1,
        1,
        NBIT_ARRAY,
        2,
        NBIT_NOOP,
        1,
    ]);
    assert_eq!(
        decompress_with_limit(&[1, 2, 3, 4, 5, 6], &good, 1 << 28).unwrap(),
        [1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn a_scale_offset_size_is_bound_to_a_datatype() {
    use hdf5_reader::filters::scaleoffset::decompress_with_limit;
    // Its size came from the client data and sized the fill value's buffer: 3 GiB here. Reading a
    // 3,079 MiB peak before the error, or an abort under a limit.
    for size in [0u32, 3, 16, 3 << 30] {
        let mut client = vec![2, 0, 0, 0, size, 0, 0, 1];
        client.extend_from_slice(&[0; 12]);
        let mut stream = 0u32.to_le_bytes().to_vec();
        stream.push(8);
        stream.extend_from_slice(&[0; 16]);
        let error = decompress_with_limit(&stream, &client, 1 << 28).unwrap_err();
        assert!(
            error.to_string().contains("datatype size"),
            "{size}: {error}"
        );
    }
}

#[test]
fn a_filter_stops_inside_its_loop() {
    use hdf5_reader::filters::{nbit, scaleoffset};
    use std::cell::Cell;
    let calls = Cell::new(0u32);
    let stop = || -> Result<()> {
        calls.set(calls.get() + 1);
        Err(Error::ResourceLimit("stopped by the test".to_string()))
    };
    let count = 1u32 << 20;
    let packed = vec![1u8; count as usize];
    let client = nbit(&[0, count, NBIT_NOOP, 1]);
    let error = nbit::decompress_checked(&packed, &client, 1 << 28, &stop).unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
    assert_eq!(calls.get(), 1);
    // Scale-offset, one byte per element and no bits stored.
    calls.set(0);
    let mut client = vec![2, 0, count, 0, 1, 0, 0, 1];
    client.extend_from_slice(&[0; 12]);
    let mut stream = 0u32.to_le_bytes().to_vec();
    stream.push(8);
    stream.extend_from_slice(&[0; 16]);
    let error = scaleoffset::decompress_checked(&stream, &client, 1 << 28, &stop).unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
    assert_eq!(calls.get(), 1);
}

#[test]
fn a_header_of_nil_messages_is_charged() {
    // Patch: object headers.
    let file = nc_probes::nil_header(1 << 20);
    let storage = Counted::new(file, 1 << 20);
    let parse = |storage: &Counted| {
        ObjectHeader::parse_at_storage_with_limit(storage, 96, 8, 8, usize::MAX)
    };
    let error = parse(storage.as_ref()).unwrap_err();
    assert!(error.is_resource_limit(), "{error}");
    // 131,072 messages at a few hundred bytes each, not one megabyte.
    assert!(storage.charged.load(Ordering::SeqCst) >= 1 << 20);
    // With room, it parses: the charge is what stopped it.
    let storage = Counted::new(nc_probes::nil_header(1 << 20), 1 << 40);
    let parsed = parse(storage.as_ref()).unwrap();
    assert_eq!(parsed.messages.len(), 1 << 17);
}

#[test]
fn a_chunk_index_of_repeats_is_charged_as_it_is_listed() {
    // Patch: B-trees. It reaches the reader's own duplicate check only with memory to list them.
    let source = hostile("chunk_index_source.nc4");
    let file = nc_raw::duplicate_chunk_entries(&source, 200_000);
    assert!(is_over_budget(open_and_read(Counted::new(
        file.clone(),
        1 << 20
    ))));
    let error = open_and_read(Counted::new(file, 1 << 40)).unwrap_err();
    assert!(error.to_string().contains("duplicate"), "{error}");
    let file = nc_probes::tree_chain(&source, 4);
    assert!(is_over_budget(open_and_read(Counted::new(file, 1 << 20))));
    let file = nc_probes::type0_index(&source, 2);
    assert!(open_and_read(Counted::new(file, 1 << 40)).is_err());
}

#[test]
fn a_version_2_chunk_index_is_charged() {
    let source = hostile("chunk_index_source_v2.nc4");
    for file in [
        nc_probes::v2_records(&source, 200_000, 7, None, false),
        nc_probes::v2_records(&source, 200_000, 0, Some(200), true),
    ] {
        let result = open_and_read(Counted::new(file, 1 << 20));
        let said = format!("{:?}", result.as_ref().map_err(ToString::to_string));
        assert!(is_over_budget(result), "{said}");
    }
}

#[test]
fn a_stored_size_over_the_chunk_is_refused_before_it_is_read() {
    // Patch: chunk lists and filters. 16 bytes x 4 = a 64-byte chunk.
    let source = hostile("chunk_index_source.nc4");
    let file = nc_probes::oversized_chunk(&source, 8 << 20);
    let error = open_and_read(Counted::new(file, u64::MAX)).unwrap_err();
    assert!(
        error.to_string().contains("decoded to 8388608 bytes"),
        "{error}"
    );
}

#[test]
fn work_is_counted_before_each_chunk_and_a_stop_is_a_read_error() {
    // Patch: stop check. Each 4 x 4 x 8 chunk is one call, and a cached one is none.
    // A time axis of 262,145 values in 4,096-value chunks.
    let bytes = hostile("long_record_262145.nc4");
    let storage = Counted::new(bytes.clone(), u64::MAX);
    let file = NcFile::from_storage(storage.clone()).unwrap();
    let rank = file.variable("time").unwrap().dimensions.len();
    file.read_variable_slice::<f64>("time", &NcSliceInfo::all(rank))
        .unwrap();
    let calls = storage.work_calls.load(Ordering::SeqCst);
    assert!(calls > 1, "{calls}");
    assert!(storage.worked.load(Ordering::SeqCst) >= calls);
    file.read_variable_slice::<f64>("time", &NcSliceInfo::all(rank))
        .unwrap();
    assert_eq!(storage.work_calls.load(Ordering::SeqCst), calls);

    let storage = Counted::stopping_after(bytes, 1);
    let file = NcFile::from_storage(storage.clone()).unwrap();
    let error = file
        .read_variable_slice::<f64>("time", &NcSliceInfo::all(rank))
        .unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
    assert_eq!(storage.work_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn copies_of_a_heap_object_are_charged() {
    // Patch: attributes. 256 KB in the file, 4,000 copies of it asked for.
    let file = nc_probes::repeated_heap_object(1 << 16, 4000);
    let storage = Counted::new(file, 1 << 24);
    let result = NcFile::from_storage(storage.clone()).and_then(|f| {
        f.root_group()?;
        Ok(0u64)
    });
    // The default metadata mode drops an attribute it cannot decode. Strict returns the error.
    let _ = result;
    assert!(storage.charged.load(Ordering::SeqCst) > 1 << 24);
}

#[test]
fn a_group_linked_below_itself_is_an_error_on_a_small_stack() {
    // Patch: group recursion. A stack overflow ends the process, so this cannot fail softly.
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(|| {
            let error = NcFile::from_bytes(&nc_raw::group_cycle())
                .and_then(|f| f.root_group().map(|_| ()))
                .unwrap_err();
            assert!(error.to_string().contains("group"), "{error}");
            let started = std::time::Instant::now();
            // 2^40 visits as written. The limit on visits ends it.
            assert!(NcFile::from_bytes(&nc_raw::group_doubled(40))
                .and_then(|f| f.root_group().map(|_| ()))
                .is_err());
            assert!(started.elapsed().as_secs() < 120, "{:?}", started.elapsed());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_group_linked_twice_without_a_cycle_is_still_built() {
    // The visited set is the traversal's ancestors, so a legal shared group is not an error.
    let file = NcFile::from_bytes(&nc_raw::group_doubled(2)).unwrap();
    let root = file.root_group().unwrap();
    assert_eq!(root.groups.len(), 2);
    assert_eq!(root.groups[0].groups.len(), 2);
}

# Changelog

## 0.1.3

Entries marked **Behaviour change** can change what existing code does or writes. Read them
before upgrading.

### Added

- **macOS wheels.** Separate abi3 wheels for macOS arm64 (deployment target 11.0) and x86_64
  (10.12), linking only system libraries. They are built in the release workflow on macOS 15,
  checked to link only system libraries, smoke-tested and tested with the Python suite there.
  The engine's killed-process matrix, including writes through a held handle, was qualified
  on macOS 15 arm64 and x86_64 before release. Routine CI stays on Linux and Windows. numpy
  and pandas set their own macOS requirements (see the README).
- **`write_batch(dss_file, records, *, version=7, sync="end", allow_undetectable_readers=True)`**
  and **`DssFile.write_batch(records, ...)`**: store many time-series and paired-data records in
  one call. Each record is a mapping with `"kind"` (`"timeseries"` or `"paired_data"`) and the
  arguments of `write_timeseries` or `write_paired_data` by name.
  - The file is opened once, the HEC lock is tested once and held until the last record, and
    the default `sync="end"` flushes once, after the last record.
  - Each record is checked by the same code and written by the same transaction, with the
    same in-progress marker, as its own `write_timeseries` / `write_paired_data` call. So the
    file holds the same records, and a process killed during a batch is recovered as for
    separate calls.
  - Every record is checked before anything is written. A record refused while the batch is
    written stops it: the records before it stay written, and its exception carries
    `batch_index` and `batch_reports`.
  - `records` may be a generator, and it may read through the same `DssFile` while it is
    consumed. Location reads after the batch see the records it wrote, also after a batch
    that fails part way.
  - Patterns, notes and multi-word quality are written one record per call, with
    `write_timeseries`.
  - `FEATURES` gains `write_batch`. Rust: `dss7::write::write_batch[_to]` and
    `dss6::write::write_batch6[_to]`.
- **`align="snap_out"`** for `gridded.import_gridded`, `import_arrays` and `import_zarr`. When
  no envelope is stated and GDAL's suggested extent is not on whole cells, its corners are
  rounded outward to whole cells and the warp samples on that lattice, so each record is
  stored where it was sampled. This does not give every cell source support: corners of a
  reprojected footprint and the added perimeter can still be missing. The default
  (`"refuse"`) is unchanged.
- **Alignment diagnostics.** Under `align="vortex"` or `"snap_out"` each import record has an
  `alignment` dict. It holds the transform the cells were sampled on, the corner and cell size
  the record stores, the shift between them in map units and in cells, and the number of
  missing cells. For a warped record it also holds the number of cells whose centre maps
  outside the source. It is `None` under the default.
- `ras_commander_fast_dss.__version__`.
- `DssPublishError` (a `DssError` and an `OSError`), raised by `create` and `repair` when they
  cannot publish a new file safely (below).
- `benchmarks/bench_public.py`: write modes `engine_batch` and `engine_batch_none`.

### Changed

- **Behaviour change: `create` and `repair` refuse when they cannot publish safely.**
  - A new file is built in full under a unique temporary name beside the destination. It is
    then given its name by a hard link, or, on glibc Linux, an atomic no-replace rename. So a
    process killed at any moment leaves the destination absent or complete.
  - When neither works, the call raises `DssPublishError` and the destination is not created.
    This happens on a filesystem without hard links such as FAT or exFAT, on an SMB share that
    does not support hard links (such as a continuously available or scale-out share), on a
    permission problem, or with too many links.
  - `create(version=7)` used to write the destination directly. On such filesystems, create
    the file on one with hard links and copy it.
  - `repair` keeps its verified copy and names it in `kept_path`.
- **Behaviour change (fix): `times=` for weekly, monthly, semi-monthly, tri-monthly and yearly
  series accepts only times the values read back at.** Up to 0.1.2, `write_timeseries(...,
  times=...)` checked the times against a calendar progression from the first time. That does
  not always match where HEC-DSS stores and reads a regular series, so it accepted and wrote
  some series whose values then read back at other times, in this library and in HEC's:
  - DSS7: a start on a day of the month that a later month lacks (for example `1MON` from
    30 January reads back with two values on 30 March), and a weekly series starting in the
    first week of a DSS decade block (read back one week later, or with a value lost when the
    first values are missing).
  - DSS6: yearly series whose times cross 29 February (HEC reads them a day off), and monthly,
    semi-monthly and tri-monthly series whose offset does not fit February.

  Such series are now refused with `ValueError` before anything is written. Leading missing
  values are taken into account, because HEC trims them when reading, which can move
  calendar times. Only HEC missing markers count as missing here, as when reading: a NaN
  written with `raw=True` is stored and read back as a value, so its time is checked too.
  Data already written is unchanged; writes with `start=` store what HEC's library stores.
- **Behaviour change (fix): DSS6 calendar series read at HEC's times.** Reading a DSS6 weekly,
  monthly, semi-monthly, tri-monthly or yearly series stored with a time offset now gives, for
  every value, the time HEC's current library gives, however many values are missing. Up to
  0.1.2 the times followed HEC's older Fortran routines, which read some values a day early:
  for example, a yearly series from 2019-03-03 read 2020-03-02 (HEC: 2020-03-03), and a series
  HEC Monolith wrote from 2020-06-15 read 2020-06-14. Fixed intervals and series without an
  offset are unchanged.
  - Qualified difference: where a value's offset does not fit February (a monthly offset of
    29–31 days, semi-monthly 14–15, tri-monthly 9–10), HEC reads that value at the next
    period's time, next to the following value (two values at one time), or drops it. This
    library keeps it at its own period's time (2 or 3 March), as 0.1.2 did.
- **Behaviour change: historical SNODAS header times are imported, not refused.** With the
  default `snodas_times="header"`, two historical NOHRSC header conventions now import with
  the header's own period as the D and E parts, typed PER-CUM:
  - melt (`1044`) and sublimation (`1039`, `1050`) files dated **before 2019-02-05**: 06:00 UTC
    of the previous day to a **05:00 UTC** stop. For 2019-02-04 the record is
    `03FEB2019:0600` to `04FEB2019:0500`;
  - liquid and solid precipitation (`1025`) files dated **before 2008-10-23**: a **07:00 UTC**
    start on the previous day to 06:00 UTC. For 2008-10-22 the record is `21OCT2008:0700` to
    `22OCT2008:0600`.

  **These historical records are 23-hour DSS periods, so consecutive daily records leave one
  hour uncovered:** 05:00 to 06:00 UTC for melt and sublimation, and 06:00 to 07:00 UTC for
  precipitation. Nothing is filled, prorated, rescaled or shifted, so summing the records does
  not give continuous coverage in DSS time. The times are the archive's header metadata. The
  NSIDC SNODAS user guide describes these products as 24-hour integrations, so the 23-hour
  periods do not show that a stored total leaves out an hour of water.

  From 2019-02-05 and 2008-10-23 the headers state 06:00 to 06:00, as before. Earlier releases
  refused files using these historical header conventions under the default timing. A file
  dated on or after a convention's end that still states it is refused, as is any other header
  that disagrees with its file name. `snodas_times="vortex"` (Vortex's timing, for
  compatibility) and `"header_as_stated"` are unchanged. Melt with time code `T0001` (files
  dated 2009-09-29 to 2010-02-16) and the snowpack temperature (`1038`) still need an explicit
  `snodas_times`. Source: the headers of NSIDC's public SNODAS archive (collection G02158),
  surveyed from 2003-10-01 to 2026-07-15; the boundary-day headers are test fixtures.
- **Writes cost less per call and per record.** The HEC lock test now checks each run of
  adjacent lock words with one call, and checks word by word only when a lock is found, so it
  reports exactly what it reported before. `gridded.grid_to_point` opens its output file once
  per call instead of once per series; each series is still its own write, with its own lock.
  No new cross-tool figures are published for 0.1.3; measure with `benchmarks/bench_public.py`
  (see the README's Performance section).
- **`DssUnsupportedRecordError` is also a `ValueError`**, as the Java path through
  hms-commander raised for a record of another kind. It is still a `DssError` and a
  `RuntimeError`.
- `read_paired_data_compat` logs a record of another kind at WARNING, not ERROR, and returns
  `None` as before.

### Fixed

- The `times=` acceptance of calendar-interval series and the DSS6 calendar read times (above).
- `create(version=6)` could remove a file it had not created when its fixed temporary name
  was already taken. Temporary names are now unique and created exclusively; only files the
  call created are ever removed.
- `create(version=7)` could leave an empty or partial file at the destination if the process
  died during the call.

### Intentional behaviour (unchanged, confirmed)

These were already true in 0.1.2. The maintainer confirmed them as intended:

- **DSS6 compressed blocks:** a native merge into a block HEC stored with delta compression
  leaves the stored values outside the written window exactly as stored, and stores the block
  uncompressed; HEC re-quantizes the whole block
  ([`docs/format/dss6.md` §9.4](docs/format/dss6.md#94-regular-merge-and-extend-into-existing-blocks)).
- **Pathnames outside printable ASCII** are refused for writing.
- **DSS6 irregular-pattern writes** (record type 111) need
  `allow_unconfirmed_dss6_irregular_pattern=True`.
- **`repair` is DSS7-only.** A DSS6 file is not repaired by a copy; the next native write to it
  reconciles an interrupted write in place.

### Documentation

- `align="vortex"` reproduces observed Vortex output and is meant for matching existing Vortex
  output. The cells are sampled on GDAL's unsnapped suggested lattice, where cells outside the
  source's transformed support come out missing. The write then rounds the declared origin to
  whole cells, which moves the whole array, missing mask included, without resampling it. For
  new forcing, state an envelope and a cell size inside the source's coverage and check the
  result against the source.
- On macOS the flushes use `F_FULLFSYNC` (Rust's `sync_all` and `sync_data`), which is
  stronger, and typically slower, than `fsync` on Linux. No power-loss claim is made on any
  platform (`docs/format/dss7.md`).

### Correction to 0.1.2

- The 0.1.2 entry said the file gridded import writes is "byte-identical whatever the count" of
  `threads`. The records written for any thread count have equal values, metadata and
  pathnames; their write-time stamps differ, as they do between any two runs.

### Unchanged

- Every default: `sync="end"`, the lock policy, `align="refuse"`, the flush and crash behaviour
  of each write call, the held `DssFile` handle and its identity checks, and every file format.

## 0.1.2

Entries marked **Behaviour change** can change what existing code does or writes. Read them
before upgrading.

### Changed

- **Behaviour change: `sync=True` now flushes once per write call.** A default write calls
  `fsync` once, at the end of each public write call, instead of at every flush point of the
  transaction. `sync=` now names three policies: `"end"` (the new default; `True` means the
  same), `"step"` (a flush at every flush point, the default up to 0.1.1) and `"none"` (no
  flush; `False` means the same). Code that passed `sync=True` to get per-step flushing must
  now pass `sync="step"`. `SYNC_MODES` lists the three names.
  - The three policies make the same writes in the same order, with the same in-progress
    marker. A write interrupted by process death is therefore reconciled the same way in
    each, and the process-death guarantee of `docs/format/dss7.md` is unchanged in every
    mode. The killed-process test (`crates/dss-core/tests/p64_kill.rs`) runs every
    instrumented kill point of its scenarios in all three modes.
  - No power-loss claim is made for any mode.
  - A call that stores several records (`gridded.import_gridded`, `import_arrays`,
    `import_zarr`, the grid tools, the time tools and `gridded.grid_to_point`) flushes once,
    after its last record, in the default mode.
- **Behaviour change: gridded import uses every core by default.** `threads` of
  `gridded.import_gridded`, `import_arrays` and `import_zarr` now defaults to `None`, meaning
  every core available to the process; it was `1`. `threads=1` and any explicit count still
  work. The file written is byte-identical whatever the count.
- **A `DssFile` session keeps one native handle.** The first `write_*` call through a
  `DssFile` opens one read+write handle, which the object keeps until `close()`, the end of
  its `with` block or garbage collection. Later writes and all reads use it, so consecutive
  writes no longer open the file each time. A `DssFile` that only reads opens nothing for
  writing.
  - Every write still tests, takes and releases the HEC lock, and the file is unlocked
    between writes.
  - Before each write, the handle's identity is checked against the file the path names
    (Windows: volume serial number and file index; Unix: device and inode). A replaced file
    is reopened; a renamed or deleted one fails the write as a module-level write would.
  - The check is repeated after the write. Another process can rename, delete or replace
    the file while a session write runs, and nothing can prevent that. If the write then
    completes, the record is stored in the previous file, which the path no longer names,
    and the call raises `OSError` (`FileNotFoundError` if nothing is at the path any more).
    If the write itself fails (for example `DssLockedError`), that error is raised and
    nothing is acknowledged.
  - A file replaced before a write by one of the other DSS major version is refused with
    `ValueError`, and nothing is written.
  - **Coordination rule:** close the `DssFile` before renaming, replacing or deleting its
    file.
  - **Behaviour change (Windows):** while a session holds its handle, `os.replace` onto the
    file fails. Close the `DssFile` first.
- **Behaviour change: DSS6 irregular-pattern writes need an explicit opt-in.** A DSS6
  irregular pattern (an `IR-` E part with a `TS-` D part, record type 111) is now refused with
  `DssUnsupportedRecordError`, and the file is left unchanged, unless the write passes
  `allow_unconfirmed_dss6_irregular_pattern=True`. This applies to `write_timeseries(...,
  version=6)` on a path and to writes through a `DssFile` session (its held handle) alike, in
  every `sync=` mode. Up to 0.1.1 such records were written without the flag. They are
  written as HEC's `zsitsi6.f` stores them, but HEC Monolith 3.3.27 misreads the tested
  records from both writers and no HEC reader has confirmed them. DSS6 regular patterns and
  all DSS7 patterns are unaffected. In Rust, `write_pattern6` and `write_pattern6_to` refuse
  them; `write_pattern6_with` and `write_pattern6_to_with` take
  `Dss6IrregularPattern::AllowUnconfirmed`.

### Added

- `sync=` on `create()`, `repair()`, `gridded.grid_to_point` and the gridded importer, so
  every public DSS writer takes it. `repair()` builds its new file under a temporary name
  that no reader sees until it is published, so `"end"` (the default) and `"step"` both
  flush it once, before it is verified and published, and `"none"` does not flush it. Up to
  0.1.1 it always flushed twice.
- **NCEP Stage IV GRIB1 (2002 to July 2020)** through `gridded.import_gridded` and
  `list_variables`: total precipitation accumulated over 1, 6 or 24 hours on the polar
  stereographic HRAP grid, simple packing, with or without a bitmap. Names, units and windows
  follow HEC-Vortex 1.1.0. Every other GRIB1 variant is a typed refusal. Up to 0.1.1, GRIB1
  was refused.
- **PRISM:** ESRI BIL beside its `.hdr`; `*_bil.zip` and `*_asc.zip` packages; and PRISM's
  2025 packages (GeoTIFF or BIL, including the Cloud-Optimized GeoTIFF normals).
- **SNODAS:** daily tars and `.dat` / `.dat.gz` members with their NOHRSC header, with the new
  `snodas_times=` and `snodas_georeference=` arguments of `gridded.import_gridded`.
  - **Behaviour difference from Vortex: header timing is the default.** With
    `snodas_times="header"`, times come from the checked NOHRSC header. A daily total (liquid
    and solid precipitation, melt, the two sublimation products) is PER-CUM over the 24 hours
    ending at 06:00 UTC of the file's date. SWE and snow depth are INST-VAL at 06:00 UTC of
    that date. Every timing field of the file name is checked against the NOHRSC naming
    rules, and a grid with no header, or a header that disagrees with the name, is refused.
  - `snodas_times="vortex"` gives HEC-Vortex 1.1.0's timing and types, for compatibility.
    Vortex puts each daily total one day later than its header and types every daily total
    PER-AVER. `snodas_times="header_as_stated"` takes the header's Start and Stop as written.
  - Products with no qualified interpretation are refused unless the caller names one of the
    two options above: the snowpack average temperature (`1038`), melt with time code
    `T0001` (historical files), and sublimation whose header stops at 05:00 (as the 2010
    headers do). The refusal names both options.
  - A SNODAS cell of `-9999` is masked before scaling and resampling, stored as a DSS missing
    value and left out of the record's statistics. Real negative values, such as sublimation,
    are kept. (Vortex keeps `-9999` divided by the product's scale as a number.)
  - A grid record holds at most 16,777,216 cells. The whole SNODAS grid (23,239,185 cells) is
    refused unwarped while the run is planned; clip to the model envelope, as the
    `import_gridded` notes show.
- **A loose GeoTIFF with a `<name>.aux.xml` beside it is read.** The sidecar supplies nodata
  and statistics only. A sidecar that states georeferencing, a CRS, a transform, a mask, a
  scale or an offset is refused, as are `<name>.aux`, `<stem>.aux` and `<name>.msk`.
- **DSS6 regular time-series merge and extend, qualified against HEC's libraries.**
  `write_timeseries(..., version=6)` into an existing record merges into and extends its
  blocks as HEC's default store (`zsrtsi6`, "always replace") does: the written values
  replace the stored ones at their positions, every other stored value is kept bit for bit,
  and new blocks are created with missing values in their unwritten slots. This behaviour is
  not new; 0.1.2 adds its qualification. A matrix of 60 stores (overlaps, extensions across
  block boundaries from 1MIN to 1YEAR, offsets, missing values both ways, every float/double
  combination, quality, compressed stored blocks) was written by HEC Monolith 3.3.27
  (`fixtures/public/p66_hec_ts6_merge.dss`) and replayed natively: 42 of 44 records are
  word-identical apart from addresses and write stamps. HEC keeps the other two compressed
  and the native writer stores them uncompressed, so their representation differs; their
  values decode equal or differ only as described below. HEC's DSS6 C library (hec-dss
  `7-IV-1`) made the same merges for the records without quality or compression with equal results, and both HEC libraries read
  every native result as they read HEC's. Killed-process tests now include DSS6 merges and
  extensions. Details: `docs/format/dss6.md` §9.4.
- **Documented difference from HEC: a merge into a delta-compressed DSS6 block.** When a
  write merges into a block that HEC stored with delta compression (scheme 3), the native
  writer stores the written values as given, keeps every stored value outside the written
  window exactly as stored, and stores the block uncompressed. HEC instead re-quantizes the
  whole block, inside and outside the window, to the caller's precision, so some stored
  values outside the window change in HEC's result (for example 15.45 becomes 15.5 at
  precision -1). This difference is deliberate and accepted; see `docs/format/dss6.md`
  §9.4, "Merges into the same HEC-compressed block". Repeat-only compression (scheme 1)
  gives the same values in both.
- **Documented: pathnames outside printable ASCII are refused for writing.** The DSS6 and
  DSS7 writers refuse a pathname that contains a byte below 0x20 or above 0x7E, as they
  already did in 0.1.1. HEC software accepts such pathnames and stores them changed, so a
  native write cannot add to an existing HEC record whose pathname has such bytes; reading
  it is unaffected. The rule is now in `docs/format/dss6.md` §10.
- `benchmarks/bench_public.py`, a reproducible cross-tool benchmark on generated data.
- `FEATURES` flags `write_sync_modes`, `gridded_threads_all_cores`, `dssfile_held_handle`,
  `gridded_grib1_stage4`, `gridded_prism_bil`, `gridded_snodas` and
  `write_ts_dss6_irregular_pattern_opt_in`.

### Fixed

- **Kilometre-to-metre conversion now matches Vortex exactly** for GRIB1, GRIB2 and NetCDF
  grids with kilometre axes. Vortex converts through its units library, by the decimal of
  `Double.toString` times 1,000, which can give a different double from `x * 1000.0`. For
  example, 1.001 km is 1001 m there and 1000.9999999999999 m through a multiplication. The
  engine now does the same, checked bit for bit against Vortex's own conversion
  (`crates/dss-vortex/tests/data/README.md`). A cell size or origin that is not finite after
  the conversion is refused, as Vortex fails to read such a grid.
- A DSS6 `DssFile.read_associated_location()` of a whole dataset could answer from the
  dataset index the object had built before a write through the same object, and miss
  records that write added. Every `DssFile` write now drops that index first.

### Robustness

- Input reads are bounded by the bytes actually read, not by the size a file system or
  archive states, so a stream that never ends, or a member that states a huge size, is
  refused at the input limit.
- One ZIP archive's rasters share a memory budget, checked before each raster is decoded
  (by default 2^30 bytes of decoded cells and 256 rasters per archive).
- BIL `.hdr`, SNODAS NOHRSC headers and GRIB1 sections are validated field by field;
  malformed values give typed errors, never a panic.

### Other

- New public fixtures: a PRISM daily BIL package and a SNODAS member with its header
  (`fixtures/public/p71/`, unmodified public files), and a synthetic NetCDF file with
  kilometre axes (`netcdf/lambert_km_decimal.nc`). Their provenance is in
  `fixtures/public/README.md`. The kilometre-to-metre reference answers are described in
  `crates/dss-vortex/tests/data/README.md`.
- New public fixtures for the DSS6 merge qualification: synthetic DSS6 files written by HEC
  Monolith 3.3.27 and by HEC's DSS6 C library, with their store scripts
  (`fixtures/public/p66_*`). Their provenance is in `fixtures/public/README.md`.
- The kilometre-to-metre reference answers (2.3 MB) are in the source repository but not in
  the source distribution, which stays at about 2.6 MB; the Rust test that reads them skips
  when the file is absent.
- New dependency, Windows only: `windows-sys` 0.61 (Microsoft's official bindings), for the
  identity check of a session's held handle. It was already in the lock file through other
  crates.
- Release wheels: unchanged from 0.1.1 (Windows x86_64, Linux x86_64 and aarch64 for
  manylinux and musllinux, plus the source distribution). There are no macOS wheels; macOS
  users can build from the source distribution.

## 0.1.1

### Fixed (crash recovery, DSS7)

- **F1.** A write killed after a new bin's entry body was written but before its
  publication word left the record unlisted: reconciliation treated the half-written bin as
  in use and put the record in the next bin, and catalog walks (the engine's and heclib's)
  stop at the first bin whose first word is zero. Reconciliation and the bin allocator now
  use heclib's empty-bin rule, and a new bin is written whole.
- **R17.** A replacement killed between its info-pointer write and its metadata write left
  the bin entry pointing at the new info block under the old record type and dates. The
  pointer, type, write time and dates are now written in a single write.
- In both cases, reconciliation now restores the file exactly. A file that 0.1.0 left in
  either state, with its interrupted-write marker still set, is completed by the next 0.1.1
  write. If a later 0.1.0 write already reconciled an F1 interruption, the file keeps a
  *hole* (a bin the catalog walk stops at, with lookup-reachable bins after it); native
  writes leave it as it is, and `repair` is the way to copy its records into a new file
  (`docs/format/dss7.md`, files damaged by engines up to 0.1.0).
- **Scope of the guarantee** (see `docs/format/dss7.md`, threat model): a file that was
  consistent before an interrupted write is reconciled exactly after process death between
  any two separate writes. It is not a power-loss guarantee, and independent damage that
  process death cannot produce, such as corrupted pointers, is refused only when cheap,
  bounded checks detect it; use `repair` for such files.
- Tested by a killed-process test (`crates/dss-core/tests/p64_kill.rs`) kills the writer
  at every flush point, step boundary and separate write, in both sync modes, then checks
  the reconciled file against an uninterrupted write.

### Added

- `repair(src, dest)`: copy-out recovery for a damaged DSS7 file. `src` is opened
  read-only; every readable record that the documented discovery procedure reaches is
  copied into a new file and verified against the source, and the result reports what was
  and was not copied (`complete`, `not_copied`). A bin block whose declared span does not
  fit the file is reported as unclassified, not salvaged (the catalog walk does salvage
  whole bins from such a block), so callers must check `complete`. An existing `dest` is
  never replaced. A partial recovery is published with `complete=False`.
- `sync=` on `write_timeseries`, `write_paired_data`, `write_grid`, `write_profile` and
  `write_location`. The default, `sync=True`, is unchanged. `sync=False` (opt-in) skips the
  between-step flushes: it keeps the ordering that process-death recovery relies on and
  gives up any power-loss claim.
- `FEATURES` flags `repair_copy_out` and `write_sync_policy`.

### Reads and catalogs

- DSS7 whole-dataset reads (time series or profiles with no window) use a per-catalog range
  index instead of scanning the whole catalog on every read.
- Catalog rows are filtered natively before export to Python, so deleted entries are no
  longer decoded or framed. `DssFile.pathnames()` now lists pathnames natively without
  building a DataFrame.
- Catalog hash-chain validation reuses per-slot work (validation stays on), and grid and
  profile reads adopt the native arrays instead of copying them.
- Outputs are unchanged: catalogs, values and metadata were compared before and after each
  change.

### Other

- New synthetic fixtures `synthetic_range_recovery7.dss` and `synthetic_chains7.dss`.
- Release wheels: Windows x86_64, Linux x86_64 and aarch64 (manylinux), Linux x86_64 and
  aarch64 (musllinux), plus the source distribution. macOS wheels are no longer built and CI
  no longer runs on macOS; macOS users can build from the source distribution.

## 0.1.0 — first public release

The first public release of the engine, under the MIT licence, as the `ras-commander-fast-dss`
Python package (abi3 wheels for CPython 3.10+ and a source distribution).

### Reading

- DSS6 and DSS7 header classification, catalogs (with recovery diagnostics for
  interrupted-write files), regular and irregular time series, quality and notes, pattern
  records, paired data, grids (HRAP, SHG/Albers, specified and undefined), profiles and
  location records.
- pandas DataFrames compatible with ras-commander's `RasDss` and hms-commander's `DssCore`
  results, with HEC missing markers mapped to NaN.
- Typed exceptions, and detection of files that HEC software holds open.

### Writing

- DSS7 and DSS6 time-series writers (regular and irregular, quality, notes, patterns,
  pseudo-regular intervals), paired data, grids, profiles and location records. Writers keep
  a file's DSS version, refuse files locked by HEC software, and write data before pointers
  so an interrupted write leaves the previous record intact.

### Gridded import and tools

- Import of GRIB2, NetCDF (classic and NetCDF-4/HDF5), GeoTIFF and ASCII rasters, and local
  Zarr v2 stores (AORC) into DSS grids, with reprojection, resampling and unit handling
  checked against HEC-Vortex 1.1.0.
- Grid tools: calculator, clipper, grid to point (zonal statistics), time shifter and
  resampler.

### Other

- Output cross-checked against pydsstools 3.1.0 as an independent reader and writer (see
  the README). The three minor edge-case differences it found in the engine were fixed to
  follow HEC's behaviour: DSS6 time-series units and type text are trimmed as heclib and
  HEC Monolith trim them, DSS6 specified-grid headers with an oversized byte count are read,
  and time-invariant grids are written with the record types HEC software reads.
- A read-only WebAssembly binding (`crates/dss-wasm`).
- Licence notices for every resolved dependency (`licenses/dependencies/`), a licence bundle
  inside the wheel, and a source manifest (`SOURCE_MANIFEST.json`).

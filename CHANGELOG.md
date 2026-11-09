# Changelog

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

### Performance

The numbers below are indicative: they were measured on a shared, loaded workstation during
development, on two large DSS7 test files. Final benchmarks will be published separately.

- DSS7 whole-dataset reads (time series or profiles with no window) use a per-catalog range
  index instead of scanning the whole catalog on every read. Mean whole-dataset read: 43.7 ms
  to 1.0 ms on a file with 147,634 datasets, and 31.5 ms to 0.6 ms on one with 33,543.
- Catalog rows are filtered natively before export to Python, so deleted entries are no
  longer decoded or framed. `catalog()` on a file with 1.54 million physical and 122,000
  live entries: 2.7 s to 1.2 s. `DssFile.pathnames()` now lists pathnames natively without
  building a DataFrame.
- Catalog hash-chain validation reuses per-slot work (validation stays on), and grid and
  profile reads adopt the native arrays instead of copying them. Checked catalog: 0.42 s to
  0.26 s and 0.85 s to 0.62 s on the two files. A 4096 x 4096 grid read allocates 32 MiB on
  the NumPy side instead of 96 MiB.
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

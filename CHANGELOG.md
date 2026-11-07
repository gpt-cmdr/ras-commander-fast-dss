# Changelog

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

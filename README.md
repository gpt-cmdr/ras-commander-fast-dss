# ras-commander-fast-dss

A native HEC-DSS engine for Python, written in Rust. It reads and writes DSS version 6 and
version 7 files (catalogs, regular and irregular time series, paired data, grids, profiles and
location records) without Java, HEC Monolith or any HEC native library. It also imports gridded
data (GRIB2, NCEP Stage IV GRIB1, NetCDF, GeoTIFF/ASCII rasters, PRISM BIL and ZIP packages,
SNODAS and Zarr v2) into DSS grids, the job HEC-Vortex does for HEC-HMS and HEC-RAS.

`ras-commander-fast-dss` is the DSS backend of
[RAS Commander](https://github.com/gpt-cmdr/ras-commander) and hms-commander. It is called
FAST-DSS on the [RAS Commander website](https://rascommander.info/ras/native-engine/).

```bash
pip install ras-commander-fast-dss
```

Wheels are abi3 (one wheel covers CPython 3.10 and later) for Windows x86_64 and for Linux
x86_64 and aarch64 (manylinux and musllinux). There are no macOS wheels, and CI does not build
or test on macOS. The source distribution is published with every release: macOS users, and
other platforms, can build it with a Rust toolchain; the build has no C dependencies.

## Quick example

```python
import numpy as np
import ras_commander_fast_dss as dss

with dss.open("model.dss") as f:
    print(f.version, f.version_string)        # 7 '7-IU'
    catalog = f.catalog()                     # DataFrame, one row per record
    flow = f.read_timeseries("/BASIN/LOC/FLOW//1HOUR/OBS/")

rating = dss.read_paired_data("model.dss", "/BASIN/LOC/STAGE-FLOW///RATING/")
grid = dss.read_grid("model.dss", "/SHG/BASIN/PRECIP/01JAN2020:0000/01JAN2020:0100/RADAR/")

# Write a regular time series into a new DSS7 file.
dss.create("out.dss", version=7)
dss.write_timeseries(
    "out.dss",
    "/BASIN/LOC/FLOW/01JAN2020/1HOUR/SIM/",
    np.array([10.0, 12.5, 15.0]),
    start="2020-01-01 01:00",
    units="CFS",
    data_type="INST-VAL",
)
```

Time series come back as pandas DataFrames with HEC missing markers mapped to NaN; paired data
and grids come back as NumPy arrays with their metadata. Errors are typed
(`ras_commander_fast_dss.exceptions`), and a file that HEC software holds open for writing is
refused rather than written (`DssLockedError`). The gridded importer and tools are in
`ras_commander_fast_dss.gridded`.

## Interrupted writes, `sync=`, `DssFile` sessions and `repair`

Writers order their steps so that a DSS7 or DSS6 file that was consistent before a write
can be reconciled after the writing process dies part-way through; the next native write
does this from the file itself. The crash model is process death, not power loss. The
format notes ([`dss7.md`](docs/format/dss7.md), [`dss6.md`](docs/format/dss6.md)) give the exact steps, what is
and is not guaranteed, and the known limitations.

- `sync=` on every writer (`write_timeseries`, `write_paired_data`, `write_grid`,
  `write_profile`, `write_location`, `create`, `repair`, the gridded importer, the grid
  tools and `gridded.grid_to_point`) chooses when the write calls `fsync`:
  - `"end"` (the default since 0.1.2; `True` means the same): one flush at the end of each
    write call. A call that stores many records flushes once, after the last.
  - `"step"`: a flush at every flush point between the steps of the write, the default up
    to 0.1.1.
  - `"none"` (`False` means the same): no flush.

  The three make the same writes in the same order, so the process-death recovery above is
  the same in every mode. No mode claims durability on power loss.
- A `DssFile` keeps one read+write handle from its first write until it is closed, so
  consecutive writes through it do not open the file again. Every write still takes and
  releases the HEC lock, and the handle is checked against the file the path names before
  and after each write. Close the `DssFile` before renaming, replacing or deleting its file:
  a write that loses a race with such a change raises `OSError`, and on Windows `os.replace`
  onto a file held by a session fails.
- `repair(src, dest)` copies the readable records of a damaged DSS7 file into a new file:
  every readable record that its documented discovery procedure reaches
  ([`dss7.md`](docs/format/dss7.md), copy-out repair). It does not find everything a
  catalog might: a bin block whose declared span does not fit the file is reported as
  unclassified rather than salvaged, while the catalog walk salvages the whole bins of such
  a partial block. `src` is opened read-only, each copied record is verified against the
  source, and an existing `dest` is never replaced. Always check the result: when the
  recovery is partial, `complete` is `False` and `not_copied` lists what was left out.

```python
report = dss.repair("damaged.dss", "recovered.dss")
if not report["complete"]:
    print(report["not_copied"])
```

`FEATURES` lists optional capabilities by name, so callers can test for them. Flags added in
0.1.2: `write_sync_modes`, `gridded_threads_all_cores`, `dssfile_held_handle`,
`gridded_grib1_stage4`, `gridded_prism_bil`, `gridded_snodas` and
`write_ts_dss6_irregular_pattern_opt_in`.

DSS6 irregular patterns (record type 111) are written only with
`allow_unconfirmed_dss6_irregular_pattern=True`, on a path or through a `DssFile` session, because no
HEC reader has confirmed such records; without the flag the write is refused and the file is
unchanged. Pathnames with bytes outside printable ASCII are refused for writing.

## With ras-commander and hms-commander

RAS Commander reads DSS boundary data through `RasDss`. With this package installed, select the
native backend:

```python
from ras_commander import RasDss

RasDss.backend = "native"     # or set RAS_COMMANDER_DSS_BACKEND=native
catalog = RasDss.get_catalog("model.dss")
```

hms-commander uses the same package for its DSS reads. See the
[native engine documentation](https://rascommander.info/ras/native-engine/) for the backend
options and the differences from the Java path.

## Qualification

The engine is a clean-room implementation of the DSS formats, written from HEC's
MIT-licensed [hec-dss](https://github.com/HydrologicEngineeringCenter/hec-dss) source and
the format notes in [`docs/format/`](docs/format/), which cite the `heclib` file and function
behind every rule. HEC software is used only as a test oracle; none of its code is copied,
linked or shipped.

- **Reads** were compared with HEC Monolith through ras-commander's Java path on the 53 public
  RasExamples DSS files (11,045 comparisons): every non-missing value is bit-identical and
  every missing position agrees; the remaining differences are documented corrections.
- **Writers** are checked in this repository's tests against files written by HEC's own C
  library and by HEC Monolith (`fixtures/public/`): for the uncompressed records tested, the
  native writer, given the same input, produces word-identical records apart from write
  times and the program name.
  DSS6 regular time-series merges and extensions into existing records are checked against
  HEC Monolith and HEC's DSS6 C library. Uncompressed merged records are word-identical
  apart from addresses and write stamps. Where HEC stored a block compressed, HEC keeps it
  compressed and the native writer stores it uncompressed: their decoded values are
  compared, and the representation differs (addresses, stamps, `KINDAT`, `KICOMP` and
  `KINCHE`). Repeat-only compression (scheme 1) decodes to the same values. One deliberate
  value difference is documented: a merge into a block HEC stored with delta compression
  keeps the stored values outside the written window, where HEC re-quantizes the whole block
  ([`docs/format/dss6.md` §9.4](docs/format/dss6.md#94-regular-merge-and-extend-into-existing-blocks)).
- **Gridded import** is checked against HEC-Vortex 1.1.0 output on public NOAA, PRISM and
  NSIDC products and Vortex's own test resources, bit for bit where the fixtures say so.
  Where this package differs from Vortex on purpose, the difference is documented: SNODAS
  times (Vortex's timing is the explicit option `snodas_times="vortex"`) and SNODAS missing
  cells (masked here, kept as numbers by Vortex).
- **Hostile input:** parsers bounds-check every offset and length and return errors; `dss-core`
  forbids `unsafe` code; fuzz and allocation-limit tests run in CI.

This is qualification for the files and versions named, not a claim that every possible DSS
record is covered. Comparison with HEC software does not imply HEC or USACE endorsement.

## Performance

Performance is qualified internally against HEC's DSS C library, pydsstools, hecdss, HEC Monolith and HEC-Vortex on a private corpus of production model files; those files and the detailed results are not published. The statements below summarize those evaluations. To measure on your own machine, run `benchmarks/bench_public.py`, which generates its own data and checks every result. Absolute times depend on storage, antivirus scanning and file layout.

- **Reads and catalogs** are on par with HEC's C library and pydsstools. On very large DSS7 catalogs pydsstools is somewhat faster, because FAST-DSS validates and normalizes every pathname it returns.
- **Compared with the Java path (HEC Monolith) and hecdss,** whole-dataset reads are many times faster, because FAST-DSS decodes records directly into NumPy arrays without a per-record Python or Java crossing.
- **Gridded import** is on par with HEC-Vortex for continental grids and faster for smaller imports, with no JVM start-up. The grid tools and grid-to-point are slower than Vortex, because each output record is written separately.
- **Writes are durable by default and therefore slower.** By default, FAST-DSS flushes to disk at the end of every write call, so that a killed process leaves a file HEC can read. The other tools leave flushing to the operating system. For bulk loads, write inside a `DssFile` session with `sync="none"`. Bulk series writes are then on par with the C-based tools. Writing many small records or paired-data records one call at a time remains noticeably slower than the C libraries, because of per-call overhead; reducing it is planned.

## Repository layout

| Path | Contents |
|---|---|
| `crates/dss-core` | The format engine: DSS6 and DSS7 readers and writers (`#![forbid(unsafe_code)]`) |
| `crates/dss-vortex` | Gridded import and grid tools |
| `crates/dss-py` | PyO3 binding, built with maturin into `ras_commander_fast_dss._native` |
| `crates/dss-wasm` | Read-only WebAssembly binding |
| `python/ras_commander_fast_dss` | The Python API |
| `vendor/` | `hdf5-reader` and `netcdf-reader` 0.12.1 with local patches ([PATCHES.md](vendor/PATCHES.md)) |
| `fixtures/public/` | Public and synthetic test files, each with its provenance in [README.md](fixtures/public/README.md) |
| `docs/format/` | DSS6 and DSS7 format specifications |
| `benchmarks/` | A reproducible cross-tool benchmark on generated data ([README.md](benchmarks/README.md)) |

Some comments, test names and synthetic fixture pathnames carry development labels (`P20`,
`P46`, ...) that date when a rule or test was added. They are labels only: the rules
themselves are stated in the code, the committed tests and [`docs/format/`](docs/format/).

## Build from source

```bash
cargo test --locked                                   # dss-core
cargo test --locked --workspace --exclude dss-py
python -m pip install maturin
python -m maturin build --release --locked --out dist
python -m pip install dist/*.whl pytest
python -I -m pytest                                   # tests/python against the installed wheel
```

`dss-vortex` needs Rust 1.92 or later; the other crates build with 1.80.

## Attributions and acknowledgments

FAST-DSS (`ras-commander-fast-dss`) is an independent, pure-Rust implementation of the HEC-DSS file format, versions 6 and 7. It could not have been built without the open-source software and data published by the U.S. Army Corps of Engineers Hydrologic Engineering Center (HEC), and by the wider water-resources community.

### How it was developed

The engine was written from HEC's published `hec-dss` C and Fortran source, which is MIT licensed. Every format rule in the code cites the HEC source file and routine it was derived from. No HEC code is copied into the engine, and no HEC library is linked or bundled: the engine has no C dependencies at build or run time.

HEC's own software served as the reference ("oracle") throughout development. Each reader and writer was compared against it on public example files, synthetic files, and a private corpus of real project files. Files written by the engine were also read back with HEC software:

- **HEC-DSS C library** (`hec-dss`, through HEC's `hecdss` Python package): the reference for DSS7 reads and writes, and for file checks such as `zcheckFile`.
- **HEC Monolith**, the Java DSS library used by HEC-RAS, HEC-HMS and HEC-DSSVue: the reference for DSS6, paired data, grids, and Java-path parity in ras-commander and hms-commander.
- **HEC-Vortex** (MIT), versions 1.1.0 and 0.13.3: the reference for the gridded importer and grid tools. Its bundled PROJ 7.2.0 and GDAL 3.2.1 were the reference for projections and resampling.
- **HEC-DSSVue, HEC-RAS and HEC-HMS example data** (public USACE data): the source of the real-data test fixtures.
- **HEC-RAS and HEC-HMS:** model runs through ras-commander and hms-commander confirmed that the software reads DSS files written by the engine.

### Independent validation with pydsstools

As a final check, outputs were validated with **pydsstools** (MIT, https://github.com/gyanz/pydsstools), a widely used third-party Python library for HEC-DSS. The comparison covered 4,727 records (21.6 million values) from 122 DSS6 and DSS7 files, covering regular and irregular time series, patterns, paired data, and HRAP, Albers and specified grids. 4,080 records were identical bit for bit and 182 more under a documented rule (for example, pydsstools returns paired data as float32). Of the 196 records that differed, 183 differed only in metadata. pydsstools could not read 209 records that the engine read, the engine could not read 1 record type that is out of its scope (irregular profiles), and 32 corrupt or zeroed records were unreadable by both. Every difference was traced to pydsstools, to behaviour HEC's own readers share, or to a documented engine convention, with HEC Monolith used to break ties. In the write tests, each library read back exactly every time-series and paired-data value the other wrote. The check found three minor engine issues, all in edge cases of files not written by HEC software and none changing a value read from a HEC-written file; all three were fixed before this release to follow HEC's behaviour (DSS6 units and type text, DSS6 specified-grid header byte counts, and the record types used for time-invariant grids).

### Thanks

Thanks to the HEC developers for publishing `hec-dss`, HEC-Vortex and their example data as open source, and to the pydsstools maintainers for a library the industry relies on.

FAST-DSS is an independent open-source project. It is not affiliated with, endorsed by or supported by HEC or USACE. HEC-DSS, HEC-RAS, HEC-HMS, HEC-DSSVue and HEC-Vortex are products of the U.S. Army Corps of Engineers.

## License

MIT; see [LICENSE](LICENSE). Some modules port code under other permissive licences
(BSD-3-Clause, MIT): [NOTICE](NOTICE) names each one and carries its notice, and
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) covers the Rust dependencies.

HEC-DSS, HEC-RAS, HEC-HMS, HEC-DSSVue and HEC-Vortex are products of the US Army Corps of
Engineers Hydrologic Engineering Center. This project is independent of HEC and USACE.

If you use this package in published work, please consider citing RAS Commander:
<https://rascommander.info/ras/cite/>.

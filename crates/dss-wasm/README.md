# dss-wasm

Read-only WebAssembly bindings for `dss-core`. JavaScript passes the
bytes of a whole DSS6 or DSS7 file as a `Uint8Array`; the module reads the signature, the
catalog, time-series records and datasets, paired data, grids, profiles and locations from
those bytes, with an explicit recovery mode for interrupted-write files.
It has no file access, no locking, no threads and no writers.

## Build

```bash
rustup target add wasm32-unknown-unknown
wasm-pack build crates/dss-wasm --release --target web       # browsers (ES module)
wasm-pack build crates/dss-wasm --release --target nodejs    # Node (CommonJS)
```

The package lands in `crates/dss-wasm/pkg/` (or `--out-dir`). The optimized module is
about 526 KB (234 KB gzip); nearly all of it is the `dss-core` readers.

## Usage

```js
import init, { DssFile, fileSignature, isHecMissing } from "./pkg/dss_wasm.js";

await init();                                   // --target web only
const bytes = new Uint8Array(await (await fetch("sample7.dss")).arrayBuffer());

console.log(fileSignature(bytes));              // { version: 7, versionString: "7-IU", ... }

const file = new DssFile(bytes);                // copies the bytes into WASM memory
const cat = file.catalog();                     // parallel arrays, every status
for (let i = 0; i < cat.pathname.length; i++) {
  if (cat.status[i] !== "live") continue;
  console.log(cat.pathname[i], cat.recordType[i]);
}

// Time series: one record, or a dataset stitched from its blocks.
const ts = file.readDataset("/BASIN/LOC/FLOW//1HOUR/OBS/");
const epochMs = Date.UTC(1899, 11, 31);         // times are seconds since 1899-12-31 00:00
ts.times.forEach((t, k) => {
  const v = ts.values[k];
  if (!isHecMissing(v)) console.log(new Date(epochMs + t * 1000).toISOString(), v, ts.units);
});
const window = file.readDataset("/BASIN/LOC/FLOW//1HOUR/OBS/", ts.times[0], ts.times[23], false);

// Paired data: values are curve-major (numberCurves runs of numberOrdinates).
const pd = file.readPairedData("/BASIN/LOC/STAGE-FLOW///RATING/");
const curve0 = pd.values.subarray(0, pd.numberOrdinates);

// Grids: Float32Array of nx * ny cells, x fastest, lowest row first.
const g = file.readGrid("/SHG/BASIN/PRECIP/01JAN2020:0000/01JAN2020:0100/QPE/");
const cell = (x, y) => g.data[y * g.nx + x];

// Profiles: a time x depth matrix, row-major in one typed array.
const pr = file.readProfile("/LAKE/SITE/DEPTH-TEMP//1HOUR/OBS/");
const at = (t, d) => pr.values[t * pr.numberDepths + d];
const firstRow = pr.values.subarray(0, pr.numberDepths);

// Locations: DSS7 type-20 records, or the coordinates of a DSS6 time series.
const loc = file.readAssociatedLocation("/BASIN/LOC/FLOW//1HOUR/OBS/");
if (loc !== null) console.log(loc.x, loc.y, loc.z, loc.supplementalText);

try {
  file.readRecord("/NO/SUCH/RECORD/01JAN2020/1HOUR/X/");
} catch (e) {
  console.log(e.name, e.kind);                  // DssPathnameNotFoundError not_found
}
file.free();                                    // release the WASM copy early

// Recovery is explicit: only this constructor reads records HEC's catalog omits.
const damaged = DssFile.openRecovery(bytes);
const rows = damaged.catalog();
for (let i = 0; i < rows.pathname.length; i++) {
  const p = damaged.entryProvenance(i);         // no values decoded
  if (p.outcome === "recovered" || p.outcome === "unreachable_by_pathname") {
    const { provenance, record } = damaged.readRecordAt(i);
    console.log(provenance.hashChain, record.pathname, record.values.length);
  }
}
```

## API

| Export | Returns |
|---|---|
| `dssVersion(head)` | 6 or 7 from a buffer holding at least the header |
| `fileSignature(head)` | `version`, `versionString`, `byteOrder`, `numberRecords`, `fileSizeBytes`, `deadWords`, `numberDeletes` (DSS7, else `null`) |
| `isHecMissing(v)` | HEC's missing test: -901, -902, -FLT_MAX (a stored NaN is a value) |
| `new DssFile(bytes)` | in-memory file; getters `version`, `versionString`, `recoveryMode`; `signature()` |
| `DssFile.openRecovery(bytes)` | the same, in explicit recovery mode (below) |
| `DssFile.catalog()` | `pathname`, `storedPathname`, `status` (arrays of strings), `recordType` (`Int32Array`), `rawStatus`, `lastWriteMs` (`Float64Array`, -1 when not stored), `duplicate`, `recovered`, `anomalies` |
| `DssFile.readRecord(pathname)` | one time-series record (one block of regular data) |
| `DssFile.readDataset(pathname, start?, end?, trimMissing = true, maxBlocks = 100000, maxRows = 16777216)` | a dataset (any D part); `start`/`end` in seconds, both or neither |
| `DssFile.readPairedData(pathname)` | types 200/205 |
| `DssFile.readGrid(pathname, maxCells = 16777216, preserveLegacyMissing = false)` | types 400-431 |
| `DssFile.readProfile(pathname, start?, end?, trimMissing = true, maxBlocks = 100000, maxCells = 16777216)` | a regular profile dataset, types 102/107 (any D part) |
| `DssFile.readProfileRecord(pathname, maxCells = 16777216)` | one stored profile record (one block) |
| `DssFile.readLocation(pathname)` | a DSS7 location record, type 20 (`/A/B/Location Info////`) |
| `DssFile.readAssociatedLocation(pathname)` | the location of a data pathname, or `null` |
| `DssFile.entryProvenance(index)` | where a `catalog()` row sits and its `outcome`; decodes no values |
| `DssFile.readRecordAt(index)` | `{provenance, record}`: the time-series record behind a `catalog()` row |

Time-series objects carry `pathname`, `recordType`, `regular`, `intervalSeconds`,
`irregularBlock`, `times` (`Float64Array`), `values` (`Float32Array` or `Float64Array`
as stored, `valueBits` 32 or 64), `quality` (`Int32Array` or `null`), `units`,
`dataType`, `timeZone`, `timeOffsetSeconds`, `pattern`, `blocksFound`, `blocksMissing`,
`warnings` and `precision`. Paired-data and grid objects carry the fields of the Python
binding's dicts in camelCase (`ordinates`, `values`, `labels`, `unitsIndependent`, ...;
`data`, `nx`, `ny`, `cellSize`, `srsDefinition`, `albers`, `nullValue`, `startTime`, ...).

Profile objects carry `pathname`, `recordType`, `regular`, `intervalSeconds`,
`intervalName`, `times`, `depths`, `values` (`valueBits` 32 or 64), `numberTimes`,
`numberDepths`, `quality` (first word per time) and `qualityWords` (every word; both
`Int32Array` or `null`), `qualityElementSize`, `depthUnits`, `valueUnits`, `dataType`,
`timeZone`, `precision`, `timeOffsetSeconds`, `timeGranularitySeconds`, `julianBaseDate`,
`blocksFound`, `blocksMissing`, `blockPathnames`, `internalHeader`, `unitsPadded` and
`userHeader`. A profile is a time x depth matrix; `values` holds it row-major
(`values[t * numberDepths + d]`) because JavaScript has no two-dimensional typed array.
Irregular profiles (112/117) throw `unsupported`, as in Python. DSS6 files cannot store
profiles (`Public/ztsStore.c`): an existing DSS6 record throws `unsupported`.

Location objects carry `pathname`, `recordType`, `source` (`dss7_location_record` or
`dss6_ts_header`), `x`, `y`, `z`, `coordinates` (the same three in a `Float64Array`, which
keeps stored bits exactly), `descriptorsStored`, the stored codes `coordinateSystem`,
`coordinateId`, `horizontalUnits`, `horizontalDatum`, `verticalUnits`, `verticalDatum`,
`timeZone`, `supplemental` (`Uint8Array` or `null`), `supplementalText`,
`supplementalEncoding`, `supplementalSource`, and for DSS7 `values1`, `program`,
`lastWriteMs`; for DSS6 `blockJulian`. A datum named in the supplemental text stays
informational and never changes the codes. DSS7: the associated location
is the record `/A/B/Location Info////`, and the data record need not exist. DSS6 has no
location records: `readLocation` throws `unsupported`, and `readAssociatedLocation` reads
the time-series header. A DSS6 pathname naming no stored block reads every block of the
dataset; blocks storing different coordinates throw `DssLocationConflictError` (`kind`
`dss`), and otherwise the last block by D-part date supplies the result.

Recovery mode is opt-in; `new DssFile` never uses it. `DssFile.openRecovery` makes DSS7
catalogs also list entries in unlinked bins (interrupted writes, which HEC's catalog omits;
`recovered` is true), and makes pathname reads take the best-preserved copy of a duplicated
pathname. Every integrity check still applies. A DSS6 file reads as usual.

`readRecordAt` and `entryProvenance` work in both modes, by row of `catalog()`. They reach
what a pathname read cannot: later copies of duplicated pathnames and DSS7 orphan-bin
entries. `outcome` is one of `ordinary`, `recovered`, `unreachable_by_pathname`,
`unavailable`, `unsupported_record_type` or `not_live`. A record whose info block fails
verification throws `DssRecordUnavailableError` (`kind` `format`, with `problem`, `index`
and `infoAddress`) and no data is substituted. Row out of range or not live: `DssIndexError`
(`kind` `index`). `binEntryAddress` and `infoAddress` are byte offsets, `null` beyond
`Number.MAX_SAFE_INTEGER`.

Conventions, the same as the Python binding:

- Values keep their stored width and bits. HEC missing markers are not converted.
- `times` are whole seconds since 1899-12-31 00:00, no time zone applied. Every value an
  engine read produces fits a JavaScript number exactly; a value that would not raises a
  `DssError`.
- Pathnames are matched as given (UTF-8, then Latin-1); DSS6 Latin-1 pathnames from the
  catalog read back unchanged.
- Errors are thrown as `Error` objects. `name` is the Python exception class
  (`DssFormatError`, `DssPathnameNotFoundError`, `DssUnsupportedRecordError`, `DssError`,
  `DssValueError`, `DssIndexError`, `DssRecordUnavailableError`,
  `DssLocationConflictError`) and `kind` is `format`, `not_found`, `unsupported`, `dss`,
  `index` or `invalid_argument`. A pathname naming a record that is not a time series raises
  `unsupported` from the time-series reads.

Limits (refusals are `invalid_argument`, thrown before any data is assembled; they read
only the catalog, pathname lookups and record headers. `readGrid`'s `maxCells`, checked by
`dss-core`, throws `DssError` instead):

- A windowed `readDataset` probes every block period in the window on the calling
  thread. A window may span at most `maxBlocks` block periods of the dataset (default
  100,000: 273 years of daily blocks, the smallest block size; much longer for monthly
  and larger blocks). The count comes from the E part (`1Hour`: monthly blocks,
  `1Day`: yearly, `IR-Day`: daily, ...). A whole-dataset `readDataset` obeys it too,
  because both DSS versions walk every block period between the first and last stored
  blocks. On every public fixture a whole dataset needs at most 151 block periods.
- `readDataset` also applies `maxRows` (default 16,777,216: 32 years of 1-minute data with
  no quality; a whole number from 1 to 2^30, so it cannot be switched off). It counts rows
  x elements per row: one value, plus the quality and integer-note element sizes in the
  block headers (a series with one quality word per value counts 2 per row), plus 3 when
  any block has character notes. A regular dataset gets a row
  for every period of its range, stored or not (`ztsRetrieveReg7`, `zrrtsi.f`), so a sparse
  or damaged file could otherwise allocate far beyond its own size. Upper estimate, checked
  before anything is assembled and counted before the trim. Irregular rows are the stored
  values, counted the same way. Reading a profile through `readDataset` throws `unsupported`
  without assembling it.
- `readRecord` and `readRecordAt` need no row limit: DSS7 regular rows are limited to one
  block period and everything else to the values stored in the file.
- `readProfile` applies `maxBlocks` with or without a window, and also `maxCells`, the
  times x (depths + quality and note elements) it would assemble (default 16,777,216, at most 2^30). `readProfileRecord`
  applies `maxCells` to its block. They use sizes read before anything is decoded (the
  header's depth count, the catalog's dates) and are upper estimates counted before the
  trim; on every public fixture the defaults refuse nothing. A plain time series read
  through `readProfile` counts one cell per time.
- `maxCells`, `maxRows` and `maxBlocks` must be whole numbers from 1 up (`maxCells` and
  `maxRows` at most 2^30). Negative, fractional, `NaN` and out-of-range values throw; they never wrap.
  The defaults are safe in the 4 GiB wasm32 memory: about 270 MB at worst. Raising a limit past roughly 2^27 can make a read trap on allocation before the 2^30 ceiling; recover by creating a new instance.
- A catalog `index` must be a whole number: otherwise `invalid_argument`; negative or
  beyond the catalog, `DssIndexError`.
- Location areas and recovery catalogs need no count limit: `dss-core` reads them from the
  file bytes, so they cannot grow beyond the file.
- A time, offset or size beyond `Number.MAX_SAFE_INTEGER` fails the read that returns
  it with a `DssError`. In `catalog()`, such an entry of `rawStatus` or `lastWriteMs`
  becomes `NaN` (and an anomaly `address` becomes `null`) so the rest of the catalog
  still returns. Engine-produced times never come near the limit.

After a WebAssembly trap (`WebAssembly.RuntimeError`, for example out of memory on a
huge grid) or any error that is not one of the `Dss*` errors above, discard the
`DssFile` and create a new one from the bytes; the old object may stay locked ("recursive
use of an object") and its memory may not be released. Typed `Dss*` errors leave the
object usable.

Reading a profile record through the time-series reads throws `unsupported` naming it a
profile record. Not exported yet: the DSS6 "no data" refusal, text records, and the
catalog-index route for paired data and grids (in recovery mode they read by pathname).

## Tests

```bash
cargo test -p dss-wasm                         # host: every route vs dss-core, all public fixtures
wasm-pack test --node crates/dss-wasm          # Node: the exports inside wasm32
cargo check -p dss-wasm --target wasm32-unknown-unknown
```

The Node test needs Node.js and the `wasm-bindgen-test-runner` that matches the locked
`wasm-bindgen` version (wasm-pack downloads it). Both tests check the same recorded digest
tables (`tests/common/expected.rs`, `tests/common/expected_routes.rs`), so a host/WASM
difference fails one of them.

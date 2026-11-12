# Public benchmark

`bench_public.py` times `ras-commander-fast-dss` against other HEC-DSS tools on data it
generates itself, so anyone can reproduce the performance statements about this library. It
needs no private files. It writes deterministic synthetic DSS7 and DSS6 files to a temporary
folder. When it runs from a checkout, it also catalogs the public DSS fixtures in
`fixtures/public/`.

## Run it

```bash
python -m pip install ras-commander-fast-dss        # or a wheel built from this repository
python benchmarks/bench_public.py                  # every installed tool, full sizes, 5 repetitions
python benchmarks/bench_public.py --quick --tools engine --reps 1   # what CI runs (smaller)
```

Options:

| Option | Meaning |
|---|---|
| `--quick` | Small sizes, for a smoke test. Default repetitions become 1. |
| `--tools engine,pydsstools,hecdss,monolith` | Which tools to try. A tool is used only if it imports in its interpreter; otherwise the report states why it was skipped. |
| `--reps N` | Timed repetitions after one warm-up (default 5). |
| `--out DIR` | Where `bench_public.json` and `bench_public.md` go (default `bench_public_results`). |
| `--versions 7,6` | DSS versions to generate and test. |
| `--kinds ...` | A subset of `ts_regular,ts_irregular,pd,grid,catalog,small`. |
| `--ops catalog,read,write` | Which operation groups to run. |
| `--widths float32,float64` | Value widths of the time-series and paired-data write cases. |
| `--write-modes ...` | Engine write modes to run (default all four, below). |
| `--python TOOL=PATH` | Run one tool in another interpreter, for example a venv with `numpy<2` for pydsstools. Repeatable. |
| `--max-cpu PCT`, `--max-wait S` | Load guard: before each timed case, wait until system CPU is at most PCT. Stop after S seconds rather than time a busy machine. |
| `--allow-unmeasured-load` | With `--max-cpu`: run even if CPU load cannot be measured. The reports then say the guard was **not applied**. Without this option the script refuses to time. |
| `--fixtures DIR` | Folder of the public DSS fixtures, when the script runs outside a checkout. |
| `--no-fixtures` | Skip the public-fixture catalog cases. |
| `--timeout S` | Per-case worker timeout (default 3600 s). A timeout is recorded as a failed case, and the run continues. |
| `--diagnostics FILE` | Also write raw error text, tracebacks and the OS-reported CPU model to FILE, for local debugging only. **Not for publishing:** it can hold paths, user and host names. It is never part of the report. |
| `--keep` | Keep the temporary folder. |

`psutil` is optional. With it the report also records physical cores and memory, and the load
guard uses it; without it the guard reads the OS counters on Windows and Linux.

## Tools

- **engine:** `ras_commander_fast_dss`. Reads use one `DssFile` per case. Writes run in four
  modes, each with its own rows:

  | Mode | API | `sync` |
  |---|---|---|
  | `engine` | module-level writers (`write_timeseries(path, ...)` and so on; each call opens the file) | `"end"` (default): one flush at the end of each write call |
  | `engine_none` | module-level writers | `"none"`: no flush |
  | `engine_session` | one `DssFile` session holding its native handle | `"end"` |
  | `engine_session_none` | one `DssFile` session | `"none"` |

- **pydsstools** (`pydsstools`).
- **hecdss**, HEC's Python wrapper of the DSS C library. It handles DSS7 only, so it is not
  run on DSS6 files.
- **monolith:** HEC Monolith (Java) through `pyjnius`, only when `DSS_BENCH_MONOLITH` names a
  folder holding `jar/*.jar` (the Monolith jars and their dependencies), `lib/` (the
  `javaHeclib` native library) and optionally `jre17/`. Otherwise `JAVA_HOME` is used.

Each tool writes its own files for the write cases. The engine generates the read inputs, and
every tool listed here reads them; none needs inputs it wrote itself.

**Sync policy.** Every row records its flush policy:

- reads: not applicable;
- engine writes: `sync="end"` or `"none"`;
- the other tools: "tool default". They request no flush, and the OS writes the data back after
  each file is closed.

The closest engine equivalent of the other tools is therefore `sync="none"`. `sync="end"` does
more work: it forces the data to storage at the end of each write call.

**Value widths.** Grids are float32 in every tool. For time series and paired data the engine
writes both widths, each in its own rows; the other tools write what they can:

| Tool | Time-series and paired-data width |
|---|---|
| hecdss | float64 only |
| pydsstools | float32 only |
| Monolith (as driven here) | float32 only |

The cells a tool cannot write are listed as skipped, with the reason. Compare tools within one
width only. Every written file's record types are checked against the width of its row. Read
inputs are float64 time series and paired data, and float32 grids.

## Data

| Kind | Full size | Quick size |
|---|---|---|
| `ts_regular` | 100 hourly series × 8,760 values | 10 × 720 |
| `ts_irregular` | 100 irregular series × 1,000 values (IR-MONTH) | 10 × 200 |
| `pd` | 100 paired-data records, 500 ordinates × 3 curves | 10 × 50 × 3 |
| `grid` | 50 grids of 400 × 400 cells (SHG, zlib) | 4 of 50 × 50 |
| `catalog` | 100 daily irregular series × 1,000 values: 100,000 records | 2,000 records |
| `small` | 1,300 hourly series × 48 values (one record each) | 100 |

Values are multiples of 0.25 that float32 represents exactly, so every width reads back the
exact generated sum. Times are whole minutes. Irregular-time arrays are passed to every tool in
numeric form, so no tool formats or parses them as strings. Regular series are still written with a
start-time string (`01JAN2021 0100`) where the tool's API takes one.

## Method

- Each case runs in a fresh subprocess that imports only the tool it measures: one warm-up
  run, then `--reps` timed runs, timed with `time.perf_counter` around the whole operation
  (open, work, close; for writes the file creation is included).
- **Input preparation.** Each tool's write inputs are converted to the form its API takes
  before any timer starts:
  - engine: datetime64 times;
  - pydsstools: int32 minutes;
  - hecdss: lists of `datetime`;
  - Monolith: Python lists.

  The timed region holds the tool's own conversion and storage work, Java marshalling
  included.
- The report gives the warm median and the warm-up time as the "first run": the first call in
  a fresh process. The OS file cache is not flushed, so it is not a cold-disk figure.

## Correctness

Every attempted case ends as `pass`, `mismatch`, `error` (including a timeout or a worker that
could not start) or `skip` (with its reason). Only `pass` counts as verified. Errors and skips
never count as passes.

- **Catalogs:** every pass, timed or warm-up, must return the generated record count. A
  public fixture's count must equal the engine's.
- **Reads:**
  - Every timed pass must return as many values as an untimed check pass that runs the same
    code. The count includes the padding a DSS6 block read returns. For example, each quick-size
    hourly series of 720 values starting 01JAN2021 01:00 comes back from DSS6 as its full January
    block of 744 slots, 720 of them written and 24 missing. Across the ten series that is 7,440
    values returned and 7,200 valid.
  - The check pass digests every array as the tool returns it: the count of valid values and
    their exact `math.fsum`. That digest must equal the generated values.
  - The timed passes only count values, so the timing holds the tool's read and nothing else.
- **Writes:** every file a case writes, the warm-up and each repetition, is read back with the
  engine before it is deleted. Each must match the generated data:
  - the values;
  - the times, for time series;
  - the DSS version;
  - the record types of its width.

The top of the Markdown report lists every failed case. The exit status is:

| Status | Meaning |
|---|---|
| 0 | every attempted case passed |
| 3 | a mismatch |
| 4 | an error: a tool exception, a timeout, a worker that exited with a nonzero status (even after writing its result; its measurements are kept but the case does not pass), or a run that could not complete |
| 5 | the load guard refused to time |
| 2 | a usage error |

The report files are written in every case.

## Output and privacy

This is exactly what `bench_public.json`, `bench_public.md` and the console can contain. Anything
outside it becomes `unspecified` or is not published.

- **Text written by the script itself:**
  - case, tool and mode names;
  - fixed messages;
  - dataset and file names from its own generator (for example `pd_v7.dss`) and the public fixture
    names it lists;
  - mismatch details built from counts, sums and record types;
  - the start time.
- **Numbers:**
  - timings;
  - counts;
  - CPU percentages;
  - core counts (integers);
  - memory in GB (with `psutil`).
- **Machine metadata, as validated fields only:**
  - OS: one of `Windows`, `Linux`, `Darwin` or `other`;
  - OS release and OS version: digits and dots only, otherwise `unspecified`;
  - architecture: one of `x86_64`, `arm64`, `x86`, `arm` or `other`;
  - Python version: `major.minor.micro` from `sys.version_info`; implementation `CPython`, `PyPy`
    or `other`;
  - CPU: only the architecture above and the integer logical and physical core counts. The CPU
    model is **not published**: `cpu_model` is always the fixed text `not recorded`, and the
    Markdown report and the console carry no CPU model. The model the OS reports (Windows
    registry, `/proc/cpuinfo`, `sysctl`, or the `platform.processor()` fallback, which may read
    WMI or `PROCESSOR_IDENTIFIER`) is free text, so it goes only to the local `--diagnostics`
    file.
- **Tool versions,** under this script's known keys only (`ras-commander-fast-dss`, `numpy`,
  `pydsstools`, `hecdss`, `java`, `heclib`):
  - each must be a PEP 440 public version, which always contains a digit;
  - a `+local` label is removed;
  - for `heclib`, HEC's `N-XX` form (for example `7-IU`) is accepted;
  - anything else is `unspecified`.
- **Failures,** as structured fields only:
  - the tool and the case;
  - the outcome (`mismatch` or `error`);
  - a category with its fixed message, for example "the worker exited with a nonzero status";
  - the exception class name, if it is a plain identifier of at most 64 characters (otherwise
    `UnrecognizedError`);
  - the generated dataset or file name.

  Exception text and tracebacks are never published.
- **Usage errors** print a fixed message that names at most one of this script's own options, for
  example `invalid value for --kinds; see --help`. They never repeat the rejected value. On
  completion the script prints a fixed message, not the `--out` path or its name.

As defence in depth, the user name, the hostname and the home and temp folders are also
scrubbed, by exact match, from everything written.

What is **not** covered:

- **The `--diagnostics` file.** It receives raw error text, tracebacks and the CPU model as the
  OS reports it, for local debugging only. It is **not for publishing**: it can hold paths, user
  and host names, and any text the OS returns as the CPU model.
- **Valid identifiers chosen by a tool.** An exception class name that is a valid identifier is
  published as the tool chose it.
- **An adversarial tool.** The result protocol between the script and its own worker processes is
  not a security boundary against a tool that tries to smuggle text through numeric fields.

Numbers from this script describe the machine they ran on. Compare tools within one run, not
across machines.

#!/usr/bin/env python
"""Reproducible HEC-DSS benchmark on synthetic data, for ras-commander-fast-dss and other tools.

Everything this script reads it first generates, deterministically, in a temporary folder.
Optionally it also catalogs the public DSS fixtures committed under ``fixtures/public/``.
It needs no other data.

Run with an interpreter that has ``ras-commander-fast-dss`` installed::

    python benchmarks/bench_public.py                    # every installed tool, full sizes
    python benchmarks/bench_public.py --quick --tools engine --reps 1   # CI smoke
    python benchmarks/bench_public.py --python pydsstools=/path/to/venv/python

Tools, each used only if it imports in its interpreter (otherwise skipped with the reason):

* ``engine``: ``ras_commander_fast_dss``. Writes run in four modes: module-level writers
  with the default ``sync`` (``"end"``) and with ``sync="none"``, and a held ``DssFile``
  session with the default and with ``"none"``.
* ``pydsstools``.
* ``hecdss`` (HEC's Python wrapper of the DSS C library; DSS7 only).
* ``monolith``: HEC Monolith through ``pyjnius``, only if ``DSS_BENCH_MONOLITH`` names a
  folder with ``jar/*.jar`` and ``lib/`` (the ``javaHeclib`` native library), and optionally
  ``jre17/`` (otherwise ``JAVA_HOME`` is used).

Method: every case runs in its own subprocess, which imports only the tool it measures: one
warm-up run, then ``--reps`` timed runs. The report gives the warm median and the warm-up run as
the "first run" (the first call in a fresh process; the OS file cache is not flushed).

Correctness, case by case (see benchmarks/README.md):

* Reads: every timed pass is checked for the number of values it returned, and one more untimed
  pass through the same code digests every value (count and exact ``math.fsum``) and compares
  it with the generated data.
* Writes: every file written, the warm-up and every timed repetition, is read back with the
  engine and compared with the generated values (and times), and its stored value width and DSS
  version are checked.

Every case ends as ``pass``, ``mismatch``, ``error`` (including timeouts) or ``skip`` (with a
reason). Only ``pass`` counts as verified. Exit status: 0 if every attempted case passed, 3 if
any mismatched, 4 if any errored or the run could not complete, 5 if the load guard refused to
time, 2 for usage errors. The report is written in every case.

Output: ``bench_public.json`` and ``bench_public.md`` in ``--out``. They, and the console, never
contain exception text. A failure is published as structured, allowlisted fields only: the tool,
the case, the outcome, the exception class name, a fixed category message chosen by this script,
and the name of the generated dataset or file, which comes from this script's own generator.
Raw error text and tracebacks go only to the optional ``--diagnostics FILE``, a local file that
is not for publishing. Machine metadata and tool versions are published only as validated fields
(fixed vocabularies, numbers, strict patterns); anything else becomes ``unspecified``. The CPU
model is never published (``cpu_model`` is always ``not recorded``); the CPU is described only by
the architecture and the core counts, and the OS-reported model goes only to the ``--diagnostics``
file. Usage errors print a fixed message and never echo the rejected input. The user name and
hostname are also scrubbed from everything written.

Load guard: with ``--max-cpu PCT`` the script measures system CPU for 2 s before each timed case
and waits (up to ``--max-wait`` s) while it is above PCT; if the machine stays busy it stops. If
CPU load cannot be measured it refuses to run, unless ``--allow-unmeasured-load`` is given, in
which case both reports state that the guard was not applied.
"""
from __future__ import annotations

import argparse
import getpass
import json
import math
import os
import platform
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SCRIPT = Path(__file__).resolve()
REPO = SCRIPT.parent.parent
PUBLIC_FIXTURES = ["Muncie.dss", "sample6.dss", "castro.dss", "BaldEagleDamBrk.dss"]

TOOLS = ["engine", "pydsstools", "hecdss", "monolith"]
# The import that must succeed in a tool's interpreter for the tool to be used.
TOOL_IMPORT = {"engine": "import ras_commander_fast_dss",
               "pydsstools": "from pydsstools.heclib.dss.HecDss import Open",
               "hecdss": "import hecdss",
               "monolith": "import jnius_config"}
# Write modes of the engine: (api, sync).
ENGINE_WRITE_MODES = {
    "engine": ("module writers", "end"),
    "engine_none": ("module writers", "none"),
    "engine_session": ("DssFile session", "end"),
    "engine_session_none": ("DssFile session", "none"),
}
MODE_LABEL = {
    "engine": "engine, module writers",
    "engine_none": "engine, module writers",
    "engine_session": "engine, DssFile session",
    "engine_session_none": "engine, DssFile session",
    "pydsstools": "pydsstools",
    "hecdss": "hecdss",
    "monolith": "HEC Monolith (pyjnius)",
}
SYNC_TEXT = {
    "end": 'sync="end": one flush to storage at the end of each write call (default)',
    "none": 'sync="none": no flush requested; the OS writes the data back',
    "os": "tool default: no flush requested; the OS writes the data back; file closed at case end",
    "read": "not applicable (read)",
}
# Value widths each tool stores for time-series and paired-data writes. Grids are float32 in
# every tool. A (tool, width) pair outside this table is reported as a skip with the reason.
WRITE_WIDTHS = {"engine": ("float32", "float64"), "hecdss": ("float64",), "pydsstools": ("float32",),
                "monolith": ("float32",)}
WIDTH_REASON = {"hecdss": "hecdss stores time series and paired data as float64 only",
                "pydsstools": "pydsstools stores time series and paired data as float32 only",
                "monolith": "Monolith stores new time-series and paired-data records as float32 here"}
# DSS record types that hold each kind at each width (HEC record type codes).
RECORD_TYPES = {("ts_regular", "float32"): {100}, ("ts_regular", "float64"): {105},
                ("small", "float32"): {100}, ("small", "float64"): {105},
                ("ts_irregular", "float32"): {110}, ("ts_irregular", "float64"): {115},
                ("pd", "float32"): {200}, ("pd", "float64"): {205}}

FULL = {
    "ts_regular": {"n": 100, "values": 8760},
    "ts_irregular": {"n": 100, "values": 1000},
    "pd": {"n": 100, "ordinates": 500, "curves": 3},
    "grid": {"n": 50, "ny": 400, "nx": 400},
    "catalog": {"n": 100, "values": 1000},  # one value per day, IR-DAY: one record each
    "small": {"n": 1300, "values": 48},
}
QUICK = {
    "ts_regular": {"n": 10, "values": 720},
    "ts_irregular": {"n": 10, "values": 200},
    "pd": {"n": 10, "ordinates": 50, "curves": 3},
    "grid": {"n": 4, "ny": 50, "nx": 50},
    "catalog": {"n": 10, "values": 200},
    "small": {"n": 100, "values": 48},
}
KIND_TEXT = {
    "ts_regular": "{n} hourly series x {values:,} values",
    "ts_irregular": "{n} irregular series x {values:,} values",
    "pd": "{n} paired-data records of {ordinates} x {curves}",
    "grid": "{n} grids of {ny} x {nx}",
    "catalog": "{n} daily irregular series x {values:,} values ({records:,} records)",
    "small": "{n:,} small hourly series x {values} values",
}
TS_KINDS = ("ts_regular", "small", "ts_irregular", "catalog")
WRITE_KINDS = ("ts_regular", "ts_irregular", "small", "pd", "grid")
READ_KINDS = ("catalog", "small", "ts_regular", "ts_irregular", "pd", "grid")

# ============================================================================ published text
# Nothing this script publishes (JSON, Markdown, console) is exception text. Failures become
# structured fields from fixed vocabularies; raw text goes only to the local --diagnostics file.
# As defence in depth, the user name, the hostname and the home and temp folders are scrubbed
# from everything written, by exact match.

FAILURE_TEXT = {
    "tool_exception": "the tool raised an exception during the case",
    "timeout": "the worker timed out and was stopped",
    "launch_error": "the worker process could not start",
    "no_result": "the worker exited without writing a result",
    "nonzero_exit": "the worker exited with a nonzero status",
    "readback_exception": "the engine raised an exception reading back a written file",
    "readback_failed": "the engine read-back worker failed",
    "generation_failed": "generating the inputs with the engine failed",
    "internal_error": "the benchmark script raised an unexpected exception",
}
_CLASS_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,63}")
# PEP 440 public version (no local label), case-insensitive; it always contains a digit.
_PEP440_PUBLIC = re.compile(
    r"(?:[0-9]+!)?[0-9]+(?:\.[0-9]+)*"
    r"(?:[-_.]?(?:a|b|c|rc|alpha|beta|pre|preview)[-_.]?[0-9]*)?"
    r"(?:-[0-9]+|[-_.]?(?:post|rev|r)[-_.]?[0-9]*)?"
    r"(?:[-_.]?dev[-_.]?[0-9]*)?", re.IGNORECASE)
# HEC's DSS library reports its version as e.g. "7-IU" (major, dash, two or three capitals).
_HECLIB_VERSION = re.compile(r"[0-9]{1,2}-[A-Z]{1,3}")
# The only version keys published: this script's own package and runtime identifiers.
VERSION_KEYS = ("ras-commander-fast-dss", "numpy", "pydsstools", "hecdss", "java", "heclib")
# Machine metadata vocabularies.
OS_NAMES = ("Windows", "Linux", "Darwin")
ARCH_NAMES = {"amd64": "x86_64", "x86_64": "x86_64", "x64": "x86_64", "arm64": "arm64", "aarch64": "arm64",
              "x86": "x86", "i386": "x86", "i686": "x86", "armv7l": "arm"}
PY_IMPLEMENTATIONS = ("CPython", "PyPy")
_RELEASE = re.compile(r"[0-9]{1,6}(?:\.[0-9]{1,6}){0,5}")
# The published CPU model. The OS-reported model is free text that no pattern can tell apart from
# other prose, so it is never published; it goes only to the local --diagnostics file.
CPU_MODEL_NOT_RECORDED = "not recorded"


def class_name(name):
    """An exception class name if it is a plain identifier, otherwise a fixed placeholder."""
    return name if isinstance(name, str) and _CLASS_NAME.fullmatch(name) else "UnrecognizedError"


def version_text(value, key=None):
    """A tool-reported version: a PEP 440 public version (any ``+local`` label removed), or for
    ``heclib`` HEC's ``N-XX`` form; anything else is ``unspecified``."""
    if not isinstance(value, str):
        return "unspecified"
    if key == "heclib" and _HECLIB_VERSION.fullmatch(value):
        return value
    public = value.split("+", 1)[0]
    return public if _PEP440_PUBLIC.fullmatch(public) else "unspecified"


def public_versions(versions):
    """Only this script's known version keys, each with a validated version."""
    if not isinstance(versions, dict):
        return {}
    return {k: version_text(versions[k], k) for k in VERSION_KEYS if k in versions}


def release_text(value):
    """An OS release or version: digits and dots only, otherwise ``unspecified``."""
    return value if isinstance(value, str) and _RELEASE.fullmatch(value) else "unspecified"


def failure(category, error_class=None, detail=None):
    """A structured failure: a category from FAILURE_TEXT, an allowlisted class name and an optional
    detail that this script builds itself (numbers and generated names only)."""
    assert category in FAILURE_TEXT, category
    return {"category": category, "message": FAILURE_TEXT[category],
            "error_class": class_name(error_class) if error_class else None, "detail": detail}


def failure_text(f):
    return (f"{f['message']}" + (f" ({f['detail']})" if f.get("detail") else "")
            + (f" [{f['error_class']}]" if f.get("error_class") else ""))


def _user():
    try:
        return getpass.getuser()
    except Exception:  # noqa: BLE001
        return ""


def _host():
    try:
        return socket.gethostname()
    except OSError:
        return ""


def scrub(text):
    """Defence in depth: remove the user name, hostname and home and temp folders (exact matches)."""
    if not isinstance(text, str) or not text:
        return text
    for d in (tempfile.gettempdir(), str(Path.home())):
        for form in {d, d.replace("\\", "/")}:
            if len(form) > 3:
                text = text.replace(form, "<redacted>")
    for word, repl in ((_user(), "<user>"), (_host(), "<host>")):
        if word and len(word) >= 3:
            text = re.sub(r"(?i)(?<![A-Za-z0-9])" + re.escape(word) + r"(?![A-Za-z0-9])", repl, text)
    return text


def scrub_tree(obj):
    if isinstance(obj, str):
        return scrub(obj)
    if isinstance(obj, list):
        return [scrub_tree(x) for x in obj]
    if isinstance(obj, dict):
        return {k: scrub_tree(v) for k, v in obj.items()}
    return obj


def say(*parts, err=False):
    print(scrub(" ".join(str(p) for p in parts)), file=sys.stderr if err else sys.stdout, flush=True)


# ============================================================================ shared generators
# Deterministic inputs. Values are multiples of 0.25 and exact in float32, so every width reads
# back exactly the generated sum.

HOUR = 3600
DAY = 86400
MONTHS = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"]
T0 = 44196 * DAY + HOUR  # 01JAN2021 01:00 in seconds since 31DEC1899 00:00
GRID_LL = (-500, 600)  # lower-left cell on the SHG 2000 m grid
SHG_WKT = ('PROJCS["USA_Contiguous_Albers_Equal_Area_Conic_USGS_version",GEOGCS["GCS_North_American_1983",'
           'DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],'
           'UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],PARAMETER["False_Easting",0.0],'
           'PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],'
           'PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],UNIT["Meter",1.0]]')


def _np():
    import numpy as np
    return np


def civil(sec):
    """(year, month, day, hour, minute) of seconds since 31DEC1899 00:00."""
    import datetime as dt
    d = dt.datetime(1899, 12, 31) + dt.timedelta(seconds=int(sec))
    return d.year, d.month, d.day, d.hour, d.minute


def hec_time(sec, sep=" "):
    """``DDMMMYYYY HHMM`` with 2400 for midnight (the end of the previous day), as HEC writes."""
    y, mo, d, h, mi = civil(sec)
    if h == 0 and mi == 0:
        y, mo, d, _, _ = civil(sec - DAY)
        h = 24
    return f"{d:02d}{MONTHS[mo - 1]}{y}{sep}{h:02d}{mi:02d}"


def rts_values(i, n):
    np = _np()
    k = np.arange(n, dtype=np.float64)
    return np.round((np.sin(k / 37.0 + i) * 400.0 + 500.0 + i) * 4.0) / 4.0


def its_series(i, n, step_s):
    """Times (seconds since 1899-12-31, whole minutes) and values of irregular series ``i``."""
    np = _np()
    t = T0 + np.arange(n, dtype=np.int64) * step_s + i * 60
    v = np.round((np.cos(np.arange(n) / 11.0 + i) * 50.0 + 100.0) * 4.0) / 4.0
    return t, v


def pd_values(i, n, curves):
    np = _np()
    x = np.arange(n, dtype=np.float64) * 0.5 + i
    y = np.stack([np.round((x - i) ** 1.5 * (c + 1) * 4.0) / 4.0 for c in range(curves)], axis=1)
    return x, y


def grid_values(i, ny, nx):
    np = _np()
    r = np.arange(ny, dtype=np.float32)[:, None]
    c = np.arange(nx, dtype=np.float32)[None, :]
    return ((r * 7 + c * 3 + i * 11) % 97).astype(np.float32) * np.float32(0.25)  # row 0 = south


def kind_records(kind, s):
    """Records a kind's file holds, from the generator's layout and HEC's block rules."""
    if kind in ("ts_regular", "small"):
        # Regular hourly blocks are calendar months; a value stamped at midnight ends the
        # previous day, so it belongs to the block of (time - 1 s).
        months = {civil(T0 + k * HOUR - 1)[:2] for k in range(s["values"])}
        return s["n"] * len(months)
    if kind == "ts_irregular":  # IR-MONTH blocks, keyed by each value's own time
        return sum(len({civil(int(t))[:2] for t in its_series(i, s["values"], 523 * 60)[0]}) for i in range(s["n"]))
    if kind == "catalog":
        return s["n"] * s["values"]  # one value per day, IR-DAY blocks: one record each
    if kind in ("pd", "grid"):
        return s["n"]
    raise ValueError(kind)


def items(kind, s):
    """The records of a kind: (pathname or dataset, payload...) per item."""
    n = s["n"]
    if kind == "ts_regular":
        return [(f"/BENCH/LOC{i:03d}/FLOW//1HOUR/SYNTH/", rts_values(i, s["values"])) for i in range(n)]
    if kind == "small":
        return [(f"/BENCH/S{i:04d}/FLOW//1HOUR/SMALL/", rts_values(i, s["values"])) for i in range(n)]
    if kind == "ts_irregular":
        return [(f"/BENCH/LOC{i:03d}/STAGE//IR-MONTH/SYNTH/",) + its_series(i, s["values"], 523 * 60) for i in range(n)]
    if kind == "catalog":
        return [(f"/BENCH/C{i:04d}/STAGE//IR-DAY/CATALOG/",) + its_series(i, s["values"], DAY) for i in range(n)]
    if kind == "pd":
        return [(f"/BENCH/LOC{i:03d}/STAGE-FLOW///SYNTH/",) + pd_values(i, s["ordinates"], s["curves"]) for i in range(n)]
    if kind == "grid":
        out = []
        for i in range(n):
            a = T0 - HOUR + i * HOUR
            out.append((f"/SHG/BENCH/PRECIP/{hec_time(a, ':')}/{hec_time(a + HOUR, ':')}/SYNTH/",
                        grid_values(i, s["ny"], s["nx"])))
        return out
    raise ValueError(kind)


def item_times(kind, it):
    """Stored times (seconds since 1899-12-31) of a time-series item."""
    np = _np()
    if kind in ("ts_regular", "small"):
        return T0 + np.arange(len(it[1]), dtype=np.int64) * HOUR
    return it[1]


def windows(kind, its):
    """Per dataset: first and last stored time (``DDMMMYYYY HHMM``), for tools that need a window."""
    out = []
    for it in its:
        t = item_times(kind, it)
        out.append([hec_time(int(t[0])), hec_time(int(t[-1]))])
    return out


def digest_arrays(arrays):
    """[count, exact sum] of float64 values, HEC missing markers and NaN excluded."""
    np = _np()
    n, parts = 0, []
    for a in arrays:
        v = np.asarray(a, dtype=np.float64).ravel()
        v = v[~(np.isnan(v) | (v == -901.0) | (v == -902.0) | (v <= -3.0e38))]
        n += int(v.size)
        parts.append(math.fsum(v.tolist()))
    return [n, math.fsum(parts)]


def _valid(a):
    np = _np()
    v = np.asarray(a, dtype=np.float64).ravel()
    return ~(np.isnan(v) | (v == -901.0) | (v == -902.0) | (v <= -3.0e38))


def digest_times(arrays):
    """[count, exact integer sum] of time arrays."""
    np = _np()
    n, total = 0, 0
    for a in arrays:
        a = np.asarray(a, dtype=np.int64).ravel()
        n += int(a.size)
        total += sum(int(x) for x in a.tolist())
    return [n, total]


def expected_digest(kind, s):
    its = items(kind, s)
    if kind == "pd":
        return digest_arrays([a for it in its for a in (it[1], it[2])])
    if kind in ("ts_irregular", "catalog"):
        return digest_arrays([it[2] for it in its])
    return digest_arrays([it[1] for it in its])


def expected_times(kind, s):
    if kind not in TS_KINDS:
        return None
    return digest_times([item_times(kind, it) for it in items(kind, s)])


# ============================================================================ worker (subprocess)


class Sink:
    """The arrays a read returns. Each is digested, or in timed passes only counted, as soon as
    the tool returns it: some tools reuse an array's buffer on their next call."""

    digest = False

    def __init__(self):
        self.n, self.parts, self.size = 0, [], 0

    def append(self, a):
        if Sink.digest:
            c, total = digest_arrays([a])
            self.n += c
            self.parts.append(total)
        self.size += int(_np().asarray(a).size)

    def __iadd__(self, arrays):
        for a in arrays:
            self.append(a)
        return self

    def result(self):
        return [self.n, math.fsum(self.parts)], self.size


class Engine:
    def __init__(self, mode="engine"):
        import ras_commander_fast_dss as fd
        self.fd = fd
        self.mode = mode

    def versions(self):
        import importlib.metadata as md
        return {"ras-commander-fast-dss": md.version("ras-commander-fast-dss"), "numpy": _np().__version__}

    # -- reads (one open per case, as a user would)
    def catalog(self, path, job):
        with self.fd.open(path) as f:
            return len(f.catalog())

    def read_ts(self, path, job):
        out = Sink()
        with self.fd.open(path) as f:
            for p in job["datasets"]:
                out.append(f.read_arrays(p, trim_missing=False)["values"])
        return out

    def read_pd(self, path, job):
        out = Sink()
        with self.fd.open(path) as f:
            for p in job["records"]:
                r = f.read_paired_data(p)
                out += [r.ordinates, r.values]
        return out

    def read_grid(self, path, job):
        out = Sink()
        with self.fd.open(path) as f:
            for p in job["records"]:
                out.append(f.read_grid(p, missing="raw").data)
        return out

    # -- writes: module-level writers, or one DssFile session
    def prepare(self, kind, its):
        """Inputs in the form the API takes, built before the timer (irregular times as datetime64)."""
        if kind in ("ts_irregular", "catalog"):
            np = _np()
            epoch = np.datetime64("1899-12-31T00:00:00", "s")
            return [(p, epoch + t.astype("timedelta64[s]"), v) for p, t, v in its]
        return its

    def write(self, path, kind, version, its, width="float64"):
        fd = self.fd
        api, sync = ENGINE_WRITE_MODES.get(self.mode, ("DssFile session", "none"))
        session = api == "DssFile session"
        value_type = "float" if width == "float32" else "double"
        fd.create(path, version=version, sync=sync)
        f = fd.open(path) if session else None
        try:
            for it in its:
                p = it[0]
                if kind in ("ts_regular", "small"):
                    args = {"start": hec_time(T0), "units": "CFS", "data_type": "INST-VAL",
                            "value_type": value_type, "sync": sync}
                    if session:
                        f.write_timeseries(p, it[1], **args)
                    else:
                        fd.write_timeseries(path, p, it[1], version=version, **args)
                elif kind in ("ts_irregular", "catalog"):
                    args = {"times": it[1], "units": "FEET", "data_type": "INST-VAL", "value_type": value_type,
                            "sync": sync}
                    if session:
                        f.write_timeseries(p, it[2], **args)
                    else:
                        fd.write_timeseries(path, p, it[2], version=version, **args)
                elif kind == "pd":
                    args = {"x_units": "FEET", "x_type": "LINEAR", "y_units": "CFS", "y_type": "LINEAR",
                            "value_type": value_type, "sync": sync}
                    if session:
                        f.write_paired_data(p, it[1], it[2], **args)
                    else:
                        fd.write_paired_data(path, p, it[1], it[2], version=version, **args)
                elif kind == "grid":
                    args = {"grid_type": "specified", "cell_size": 2000.0, "units": "MM", "data_type": "PER-CUM",
                            "lower_left_cell": GRID_LL, "srs_definition": SHG_WKT, "srs_name": "SHG",
                            "compression": "zlib", "sync": sync}
                    if session:
                        f.write_grid(p, it[1], **args)
                    else:
                        fd.write_grid(path, p, it[1], version=version, **args)
        finally:
            if f is not None:
                f.close()
        return len(its)

    def verify(self, path, kind, version, its):
        """Read every written item back with the engine (untimed): values, times and record types."""
        with self.fd.open(path) as f:
            cat = f.catalog()
            types = sorted({int(t) for t in cat["record_type"]})
            times = None
            if kind == "pd":
                arrays = []
                for it in its:
                    r = f.read_paired_data(it[0])
                    arrays += [r.ordinates, r.values]
            elif kind == "grid":
                arrays = [f.read_grid(it[0], missing="raw").data for it in its]
            else:
                ds = [f.read_arrays(it[0], trim_missing=False) for it in its]
                arrays = [d["values"] for d in ds]
                # Times of the stored values only: DSS6 regular reads return whole blocks, padded
                # with missing values, whose times are not part of the written series.
                times = digest_times([_np().asarray(d["times"])[_valid(d["values"])] for d in ds])
            return {"digest": digest_arrays(arrays), "times": times, "records": len(cat),
                    "record_types": types, "version": int(f.version)}


class Pydss:
    def __init__(self, mode="pydsstools"):
        import warnings
        warnings.simplefilter("ignore")
        from pydsstools.heclib.dss.HecDss import Open
        self.Open = Open

    def versions(self):
        import importlib.metadata as md
        return {"pydsstools": md.version("pydsstools"), "numpy": _np().__version__}

    def catalog(self, path, job):
        f = self.Open(str(path))
        try:
            return len(f.search_path(""))
        finally:
            f.close()

    def read_ts(self, path, job):
        out = Sink()
        f = self.Open(str(path))
        try:
            for p, w in zip(job["datasets"], job["windows"]):
                r = f.read_ts(p, window=tuple(w), value_precision="native", trim_missing=False)
                out.append(r.values)  # r keeps the values' buffer alive until they are digested
        finally:
            f.close()
        return out

    def read_pd(self, path, job):
        out = Sink()
        f = self.Open(str(path))
        try:
            for p in job["records"]:
                r = f.read_pd(p, dataframe=False)
                x, y = (r.x_data, r.y_data) if hasattr(r, "x_data") else r.get_data()[:2]
                out += [x, y]
        finally:
            f.close()
        return out

    def read_grid(self, path, job):
        out = Sink()
        f = self.Open(str(path))
        try:
            for p in job["records"]:
                g = f.read_grid(p)
                out.append(_np().asarray(g.read() if hasattr(g, "read") else g.data))
        finally:
            f.close()
        return out

    def prepare(self, kind, its):
        """Irregular times as int32 minutes since the julian base 31DEC1899, built before the timer."""
        if kind in ("ts_irregular", "catalog"):
            return [(p, (t // 60).astype(_np().int32), v) for p, t, v in its]
        return its

    def write(self, path, kind, version, its, width="float32"):
        np = _np()
        from pydsstools.core import TimeSeriesContainer
        f = self.Open(str(path), version=version)
        try:
            for it in its:
                p = it[0]
                if kind in ("ts_regular", "small"):
                    f.put_ts(TimeSeriesContainer(p, len(it[1]), 3600, values=it[1], start_time=hec_time(T0),
                                                 data_units="CFS", data_type="INST-VAL"))
                elif kind in ("ts_irregular", "catalog"):
                    # Numeric times (prepare): whole minutes since the julian base 31DEC1899, the same
                    # epoch as the engine's input, so no tool formats or parses time strings.
                    f.put_ts(TimeSeriesContainer(p, len(it[2]), -1, values=it[2], times=it[1],
                                                 julian_base="31DEC1899", data_units="FEET",
                                                 data_type="INST-VAL"))
                elif kind == "pd":
                    from pydsstools.core import PairedDataContainer
                    c = PairedDataContainer(p, (len(it[1]), it[2].shape[1]))
                    c.x_data, c.y_data = it[1], it[2].T.copy()
                    c.x_units, c.x_type, c.y_units, c.y_type = "FEET", "LINEAR", "CFS", "LINEAR"
                    f.put_pd(c)
                elif kind == "grid":
                    from pydsstools.core.gridinfo import DataType, GridInfoCreate, GridType
                    ny, nx = it[1].shape
                    gi = GridInfoCreate(grid_type=GridType.specified, data_type=DataType.per_cum, shape=(ny, nx),
                                        cell_size=2000.0, crs=SHG_WKT, crs_name="SHG",
                                        nodata=-3.4028234663852886e38,
                                        min_xy=(GRID_LL[0] * 2000.0, GRID_LL[1] * 2000.0), data_units="MM")
                    (f.put_grid if version == 7 else f.put_grid0)(np.flipud(it[1]), p, gi)  # takes north-up rows
        finally:
            f.close()
        return len(its)


class HecDss:
    def __init__(self, mode="hecdss"):
        import hecdss
        self.h = hecdss

    def versions(self):
        import importlib.metadata as md
        return {"hecdss": md.version("hecdss"), "numpy": _np().__version__}

    def _open(self, path):
        return self.h.HecDss(str(path))

    def catalog(self, path, job):
        d = self._open(path)
        try:
            return len(d.get_catalog().uncondensed_paths)
        finally:
            d.close()

    def read_ts(self, path, job):
        import datetime as dt
        out = Sink()
        d = self._open(path)
        try:
            for p, w in zip(job["datasets"], job["windows"]):
                s, e = (dt.datetime.strptime(x.replace("2400", "0000"), "%d%b%Y %H%M") for x in w)
                if w[1].endswith("2400"):
                    e += dt.timedelta(days=1)
                r = d.get(p, s, e)
                out.append(r.values)
        finally:
            d.close()
        return out

    def read_pd(self, path, job):
        out = Sink()
        d = self._open(path)
        try:
            for p in job["records"]:
                r = d.get(p)
                out += [r.ordinates, r.values]
        finally:
            d.close()
        return out

    def read_grid(self, path, job):
        out = Sink()
        d = self._open(path)
        try:
            for p in job["records"]:
                r = d.get(p)
                if r is None:
                    raise RuntimeError("hecdss returned no grid")
                out.append(r.data)
        finally:
            d.close()
        return out

    def prepare(self, kind, its):
        """Times as the datetime lists hecdss takes, built before the timer."""
        import datetime as dt
        base = dt.datetime(1899, 12, 31)
        if kind in ("ts_regular", "small"):
            return [(p, v, [base + dt.timedelta(seconds=T0 + k * HOUR) for k in range(len(v))]) for p, v in its]
        if kind in ("ts_irregular", "catalog"):
            return [(p, [base + dt.timedelta(seconds=int(s)) for s in t], v) for p, t, v in its]
        return its

    def write(self, path, kind, version, its, width="float64"):
        h = self.h
        d = self._open(path)
        try:
            for it in its:
                p = it[0]
                if kind in ("ts_regular", "small"):
                    ts = h.RegularTimeSeries()
                    ts.id, ts.values, ts.units, ts.data_type, ts.interval = p, it[1], "CFS", "INST-VAL", 3600
                    ts.times = it[2]
                elif kind in ("ts_irregular", "catalog"):
                    ts = h.IrregularTimeSeries()
                    ts.id, ts.values, ts.units, ts.data_type = p, it[2], "FEET", "INST-VAL"
                    ts.times = it[1]
                    ts.time_granularity_seconds, ts.julian_base_date = 60, 0
                elif kind == "pd":
                    ts = h.PairedData.create(it[1], it[2].copy(), labels=[], x_units="FEET", x_type="LINEAR",
                                             y_units="CFS", y_type="LINEAR", path=p)
                elif kind == "grid":
                    from hecdss.gridded_data import GriddedData
                    ny, nx = it[1].shape
                    ts = GriddedData.create(path=p, type=430, dataType=1, lowerLeftCellX=GRID_LL[0],
                                            lowerLeftCellY=GRID_LL[1], numberOfCellsX=nx, numberOfCellsY=ny,
                                            dataUnits="MM", srsName="SHG", srsDefinitionType=1,
                                            srsDefinition=SHG_WKT, cellSize=2000.0, isInterval=1,
                                            isTimeStamped=1, timeZoneID="UTC",
                                            nullValue=-3.4028234663852886e38, data=it[1])
                st = d.put(ts)
                if st != 0:
                    raise RuntimeError(f"put returned status {st}")
        finally:
            d.close()
        return len(its)


class Monolith:
    """HEC Monolith through pyjnius. ``DSS_BENCH_MONOLITH`` names the folder (see the module notes)."""

    def __init__(self, mode="monolith"):
        root = Path(os.environ["DSS_BENCH_MONOLITH"])
        if (root / "jre17").is_dir():
            os.environ["JAVA_HOME"] = str(root / "jre17")
            os.environ["PATH"] = str(root / "jre17" / "bin" / "server") + os.pathsep + os.environ["PATH"]
        os.environ["PATH"] = str(root / "lib") + os.pathsep + os.environ["PATH"]
        import jnius_config
        jnius_config.add_options("-Xmx4g", "-Djava.library.path=" + str(root / "lib"))
        jnius_config.add_classpath(*[str(j) for j in sorted((root / "jar").glob("*.jar"))])
        from jnius import autoclass, cast  # starts the JVM
        self.A, self.cast = autoclass, cast
        self.HecDssJ = autoclass("hec.heclib.dss.HecDss")
        autoclass("hec.heclib.util.Heclib").Hec_zset("MLEVEL", "", 0)

    def versions(self):
        sysc = self.A("java.lang.System")
        return {"java": str(sysc.getProperty("java.version")),
                "heclib": str(self.A("hec.heclib.util.Heclib").getLibraryVersionString())}

    def catalog(self, path, job):
        d = self.HecDssJ.open(str(path))
        try:
            return int(d.getCatalogedPathnames().size())
        finally:
            d.done()

    def read_ts(self, path, job):
        np = _np()
        out = Sink()
        d = self.HecDssJ.open(str(path))
        try:
            for p, w in zip(job["datasets"], job["windows"]):
                c = self.cast("hec.io.TimeSeriesContainer", d.get(p, w[0], w[1]))
                out.append(np.array(c.values))
        finally:
            d.done()
        return out

    def read_pd(self, path, job):
        np = _np()
        out = Sink()
        d = self.HecDssJ.open(str(path))
        try:
            for p in job["records"]:
                c = self.cast("hec.io.PairedDataContainer", d.get(p))
                out += [np.array(c.xOrdinates), np.array(c.yOrdinates)]
        finally:
            d.done()
        return out

    def read_grid(self, path, job):
        np = _np()
        out = Sink()
        d = self.HecDssJ.open(str(path))
        try:
            for p in job["records"]:
                c = self.cast("hec.io.GridContainer", d.get(p))
                out.append(np.array(c.getGridData().getData(), dtype=np.float32))
        finally:
            d.done()
        return out

    def prepare(self, kind, its):
        """Python lists of minutes, values and cells, built before the timer (Java conversion stays timed)."""
        if kind in ("ts_regular", "small"):
            return [(p, v.tolist(), [(T0 + k * HOUR) // 60 for k in range(len(v))], 60, "CFS") for p, v in its]
        if kind in ("ts_irregular", "catalog"):
            return [(p, v.tolist(), (t // 60).tolist(), 0, "FEET") for p, t, v in its]
        if kind == "pd":
            return [(p, x.tolist(), y.T.tolist(), len(x), y.shape[1]) for p, x, y in its]
        if kind == "grid":
            return [(p, g.shape, g.ravel().tolist()) for p, g in its]
        return its

    def write(self, path, kind, version, its, width="float32"):
        A = self.A
        if version == 6:
            A("hec.heclib.util.Heclib").Hec_zset("DSSV", "", 6)
        if kind in TS_KINDS:
            w = A("hec.heclib.dss.HecTimeSeries")()
            w.setDSSFileName(str(path))
            w.setStoreDoubles(False)
            TSC, HTA = A("hec.io.TimeSeriesContainer"), A("hec.heclib.util.HecTimeArray")
            try:
                for p, vals, tm, interval, units in its:
                    c = TSC()
                    c.fullName = p
                    c.setTimes(HTA(tm))
                    c.setValues(vals)
                    c.numberValues, c.units, c.type, c.interval = len(vals), units, "INST-VAL", interval
                    c.startTime, c.endTime = tm[0], tm[-1]
                    st = w.write(c)
                    if st != 0:
                        raise RuntimeError(f"write returned status {st}")
            finally:
                w.done()
        elif kind == "pd":
            w = A("hec.heclib.dss.HecPairedData")()
            w.setDSSFileName(str(path))
            w.setStoreDoubles(False)
            PDC = A("hec.io.PairedDataContainer")
            try:
                for p, x, y, n_ord, n_curves in its:
                    c = PDC()
                    c.fullName = p
                    c.setXOrdinates(x)
                    c.setYOrdinates(y)
                    c.numberOrdinates, c.numberCurves = n_ord, n_curves
                    c.xunits, c.xtype, c.yunits, c.ytype = "FEET", "LINEAR", "CFS", "LINEAR"
                    st = w.write(c)
                    if st != 0:
                        raise RuntimeError(f"write returned status {st}")
            finally:
                w.done()
        elif kind == "grid":
            GriddedData, GridData = A("hec.heclib.grid.GriddedData"), A("hec.heclib.grid.GridData")
            SGI, HecTime = A("hec.heclib.grid.SpecifiedGridInfo"), A("hec.heclib.util.HecTime")
            out = GriddedData()
            out.setDSSFileName(str(path))
            try:
                for p, (ny, nx), cells in its:
                    gi = SGI()
                    gi.setSpatialReference("SHG", SHG_WKT, 0.0, 0.0)
                    gi.setCellInfo(GRID_LL[0], GRID_LL[1], nx, ny, 2000.0)
                    gi.setParameterInfo("MM", 1)  # 1 = PER-CUM
                    parts = p.split("/")
                    gi.setGridTimes(HecTime(parts[4].replace(":", " ")), HecTime(parts[5].replace(":", " ")))
                    gi.setCompressionMethod(26)  # zlib deflate
                    out.setPathname(p)
                    st = out.storeGriddedData(gi, GridData(cells, gi))
                    if st != 0:
                        raise RuntimeError(f"store returned status {st}")
            finally:
                out.done()
        return len(its)


WORKER_TOOLS = {"engine": Engine, "pydsstools": Pydss, "hecdss": HecDss, "monolith": Monolith}


def worker_main(job_path, out_path):
    """Run one case in this process: a warm-up, then ``reps`` timed repetitions.

    Reads return each pass's value count and, from one more untimed pass, the full digest.
    Writes return every file they wrote (warm-up included), for the parent to verify.
    Errors are returned raw; the parent keeps them only in the local diagnostics."""
    job = json.loads(Path(job_path).read_text(encoding="utf-8"))
    tool_name, mode, op = job["tool"], job["mode"], job["op"]
    res = {"versions": None, "times_s": [], "first_s": None, "pass_counts": [], "digest": None,
           "written": [], "error": None, "error_type": None, "verify": None}
    try:
        tool = WORKER_TOOLS[tool_name](mode)
        res["versions"] = tool.versions()
        if op == "gen":  # generate the read inputs with the engine; untimed
            for spec in job["files"]:
                tool.write(spec["path"], spec["kind"], spec["version"],
                           tool.prepare(spec["kind"], items(spec["kind"], spec["sizes"])), spec.get("width", "float64"))
        elif op == "verify":
            res["verify"] = []
            for v in job["targets"]:
                try:
                    res["verify"].append(tool.verify(v["path"], v["kind"], v["version"], items(v["kind"], v["sizes"])))
                except Exception as e:  # noqa: BLE001 - one unreadable file is one failure
                    res["verify"].append({"error": f"{type(e).__name__}: {e}"[:600], "error_type": type(e).__name__})
        else:
            reps = int(job["reps"])
            # Inputs in each tool's own form, built before any timer starts.
            its = tool.prepare(job["kind"], items(job["kind"], job["sizes"])) if op == "write" else None
            for k in range(1 + reps):
                if op == "write":
                    target = f"{job['path'][:-4]}_{k}.dss"
                    res["written"].append(target)  # recorded before the write, so a failed one is verified too
                    t = time.perf_counter()
                    tool.write(target, job["kind"], job["version"], its, job.get("width"))
                    dt_ = time.perf_counter() - t
                else:
                    Sink.digest = False
                    t = time.perf_counter()
                    r = getattr(tool, op)(job["path"], job)
                    dt_ = time.perf_counter() - t
                    res["pass_counts"].append(r if op == "catalog" else r.size)
                if k == 0:
                    res["first_s"] = dt_
                else:
                    res["times_s"].append(dt_)
            if op not in ("write", "catalog"):  # one more pass, untimed, digesting each array
                Sink.digest = True
                res["digest"], res["digest_pass_size"] = getattr(tool, op)(job["path"], job).result()
    except Exception as e:  # noqa: BLE001 - reported, never hidden
        res["error"] = f"{type(e).__name__}: {e}"[:600]
        res["error_type"] = type(e).__name__
    Path(out_path).write_text(json.dumps(res), encoding="utf-8")


# ============================================================================ machine facts


# Where the Linux CPU model is read from (a module constant so tests can point it elsewhere).
CPUINFO = Path("/proc/cpuinfo")


def _cpu_model_raw(system=None):
    """The CPU model as the OS reports it (registry, /proc/cpuinfo, sysctl; then platform.processor(),
    which may read WMI or PROCESSOR_IDENTIFIER). ``system`` defaults to ``sys.platform``.

    Outside free text, for the local --diagnostics file only: it is never published."""
    system = sys.platform if system is None else system
    try:
        if system == "win32":
            import winreg
            k = winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
            return winreg.QueryValueEx(k, "ProcessorNameString")[0]
        if system.startswith("linux"):
            for line in CPUINFO.read_text().splitlines():
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1]
        if system == "darwin":
            return subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True,
                                  text=True, check=False).stdout
    except Exception:  # noqa: BLE001 - fall back below
        pass
    try:
        return platform.processor()
    except Exception:  # noqa: BLE001
        return None


def _int_or_none(v):
    return v if isinstance(v, int) and not isinstance(v, bool) and v >= 0 else None


def machine():
    """Machine metadata, as validated fields only: fixed vocabularies, numbers and strict patterns.

    No OS-reported CPU text is read here: ``cpu_model`` is the fixed literal ``not recorded``, and
    the CPU is described only by the architecture and the integer core counts."""
    system = platform.system()
    arch = ARCH_NAMES.get(str(platform.machine()).lower(), "other")
    impl = platform.python_implementation()
    v = sys.version_info
    m = {"os": system if system in OS_NAMES else "other",
         "os_release": release_text(platform.release()),
         "os_version": release_text(platform.version()),
         "architecture": arch,
         "cpu_model": CPU_MODEL_NOT_RECORDED,
         "logical_cores": _int_or_none(os.cpu_count()),
         "physical_cores": None,
         "python": f"{int(v.major)}.{int(v.minor)}.{int(v.micro)}",
         "python_implementation": impl if impl in PY_IMPLEMENTATIONS else "other"}
    try:
        import psutil
        m["physical_cores"] = _int_or_none(psutil.cpu_count(logical=False))
        mem = psutil.virtual_memory().total
        m["memory_gb"] = round(float(mem) / 2**30, 1) if isinstance(mem, (int, float)) else None
    except ImportError:
        pass
    return m


# ============================================================================ machine-load guard


def _cpu_counters():
    """(idle, total) CPU time counters, or None if this OS gives none without psutil."""
    try:
        if sys.platform == "win32":
            import ctypes
            from ctypes import wintypes
            idle, kern, user = wintypes.FILETIME(), wintypes.FILETIME(), wintypes.FILETIME()
            if not ctypes.windll.kernel32.GetSystemTimes(ctypes.byref(idle), ctypes.byref(kern), ctypes.byref(user)):
                return None
            v = [(f.dwHighDateTime << 32) | f.dwLowDateTime for f in (idle, kern, user)]
            return v[0], v[1] + v[2]  # kernel time includes idle time
        if sys.platform.startswith("linux"):
            f = Path("/proc/stat").read_text().splitlines()[0].split()[1:]
            v = [int(x) for x in f]
            return v[3] + (v[4] if len(v) > 4 else 0), sum(v)
    except (OSError, ValueError, AttributeError):
        return None
    return None


def system_cpu_percent(seconds=2.0):
    """System-wide CPU use over ``seconds``, or None if it cannot be measured."""
    try:
        import psutil
        return float(psutil.cpu_percent(interval=seconds))
    except ImportError:
        pass
    a = _cpu_counters()
    if a is None:
        return None
    time.sleep(seconds)
    b = _cpu_counters()
    if b is None:
        return None
    total = b[1] - a[1]
    return round(100.0 * (1.0 - (b[0] - a[0]) / total), 1) if total > 0 else None


class EngineMissing(Exception):
    """The engine is not usable; its message is built by this script."""


class GuardRefused(Exception):
    """The load guard refused to time (machine busy, or load not measurable)."""


class LoadGuard:
    """Waits for a quiet machine before each timed case when ``max_cpu`` is set.

    If CPU load cannot be measured the guard refuses (``GuardRefused``) unless
    ``allow_unmeasured`` is set; then ``applied`` is False and the reports say so."""

    def __init__(self, max_cpu, max_wait, allow_unmeasured=False):
        self.max_cpu, self.max_wait, self.allow_unmeasured = max_cpu, max_wait, allow_unmeasured
        self.samples, self.applied, self.note = [], max_cpu is not None, None
        if max_cpu is not None and system_cpu_percent(0.2) is None:
            self._unmeasurable()

    def _unmeasurable(self):
        if not self.allow_unmeasured:
            raise GuardRefused(f"--max-cpu {self.max_cpu:g} was requested but system CPU load cannot be measured "
                               "here; refusing to time (--allow-unmeasured-load runs without the guard)")
        self.applied = False
        self.note = "CPU load could not be measured; the load guard was NOT applied (--allow-unmeasured-load)"

    def wait_quiet(self):
        if self.max_cpu is None or not self.applied:
            return None
        t0 = time.monotonic()
        while True:
            pct = system_cpu_percent()
            if pct is None:
                self._unmeasurable()
                return None
            self.samples.append(pct)
            if pct <= self.max_cpu:
                return pct
            if time.monotonic() - t0 > self.max_wait:
                raise GuardRefused(f"system CPU stayed above {self.max_cpu:g} % for {self.max_wait:g} s; "
                                   "stopped rather than time a busy machine")
            say(f"  machine busy ({pct} % > {self.max_cpu:g} %); waiting")
            time.sleep(8.0)


# ============================================================================ orchestrator


def tool_available(tool, py):
    """(available, reason). Imports the tool in the interpreter that will run it."""
    if tool == "monolith":
        root = os.environ.get("DSS_BENCH_MONOLITH")
        if not root:
            return False, "DSS_BENCH_MONOLITH is not set"
        if not list(Path(root).glob("jar/*.jar")):
            return False, "DSS_BENCH_MONOLITH has no jar/*.jar"
    # The probe prints only the exception class name on failure; nothing else is read from it.
    code = (f"try:\n    {TOOL_IMPORT[tool]}\nexcept BaseException as e:\n"
            "    print('BENCH-IMPORT-FAILED', type(e).__name__)\n    raise SystemExit(1)")
    try:
        cp = subprocess.run([py, "-I", "-c", code], capture_output=True, text=True, timeout=300, check=False)
    except (OSError, subprocess.TimeoutExpired) as e:
        return False, f"its interpreter could not run [{class_name(type(e).__name__)}]"
    if cp.returncode != 0:
        marks = [ln.split()[-1] for ln in (cp.stdout or "").splitlines() if ln.startswith("BENCH-IMPORT-FAILED ")]
        return False, f"import failed in its interpreter [{class_name(marks[-1] if marks else None)}]"
    return True, ""


class Runner:
    def __init__(self, args, work, pythons, guard, diagnostics):
        self.args, self.work, self.pythons, self.guard, self.diag = args, work, pythons, guard, diagnostics
        self.n = 0
        self.gen_s = None

    def _launch(self, cmd, timeout):
        """Run a worker. Kept separate so tests can replace it."""
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=False)

    def run(self, tool, mode, op, **job):
        """Run one worker; never raises for a worker failure, which comes back as ``failure``.

        Any nonzero worker exit is a failure, even when the worker wrote a complete result: its
        measurements are kept, and the case cannot pass. Raw error text goes only to the local
        diagnostics."""
        self.n += 1
        jp = self.work / "jobs" / f"{self.n:04d}.json"
        out_path = self.work / "jobs" / f"{self.n:04d}.out.json"
        jp.parent.mkdir(exist_ok=True)
        job.update(tool=tool, mode=mode, op=op)
        jp.write_text(json.dumps(job), encoding="utf-8")
        cpu = self.guard.wait_quiet() if op not in ("gen", "verify") else None
        cmd = [self.pythons[tool], "-I", str(SCRIPT), "--worker", str(jp), str(out_path)]
        where = f"{tool}/{mode}/{op} (job {self.n:04d})"
        try:
            cp = self._launch(cmd, self.args.timeout)
        except subprocess.TimeoutExpired:
            self.diag.append(f"{where}: timed out after {self.args.timeout:g} s")
            res = {"failure": failure("timeout", "TimeoutExpired", f"after {self.args.timeout:g} s")}
        except OSError as e:
            self.diag.append(f"{where}: could not start: {type(e).__name__}: {e}")
            res = {"failure": failure("launch_error", type(e).__name__)}
        else:
            rc = cp.returncode
            try:
                res = json.loads(out_path.read_text(encoding="utf-8"))
                if not isinstance(res, dict):
                    raise ValueError("result is not an object")
            except (OSError, ValueError):
                self.diag.append(f"{where}: exit status {rc}, no result; stderr:\n{cp.stderr or ''}")
                res = {"failure": failure("no_result", None, f"exit status {rc}")}
            else:
                raw = res.pop("error", None)
                cls = res.pop("error_type", None)
                if raw:
                    self.diag.append(f"{where}: {raw}")
                    res["failure"] = failure("tool_exception", cls or "UnrecognizedError")
                if rc != 0:
                    self.diag.append(f"{where}: exit status {rc} after writing its result; stderr:\n{cp.stderr or ''}")
                    if not res.get("failure"):
                        res["failure"] = failure("nonzero_exit", None, f"exit status {rc}")
                for v in res.get("verify") or []:  # per-file read-back errors: class name only
                    if isinstance(v, dict) and v.get("error"):
                        self.diag.append(f"{where}: read-back: {v.pop('error')}")
            res["exit_status"] = rc
        res["cpu_before_pct"] = cpu
        return res


def fmt_s(v):
    if v is None:
        return "–"
    if v >= 10:
        return f"{v:.1f} s"
    if v >= 1:
        return f"{v:.2f} s"
    if v >= 0.01:
        return f"{v:.3f} s"
    return f"{v * 1000:.2f} ms"


class Report:
    """Rows and failures of one run. Every attempted case ends pass, mismatch, error or skip."""

    def __init__(self):
        self.rows, self.fatal, self.tool_versions = [], [], {}

    def add(self, **row):
        row.setdefault("status", "pass")
        row.setdefault("problems", [])
        self.rows.append(row)
        return row

    def fail(self, row, status, problem):
        """Mark a row failed with a problem this script built. ``error`` outranks ``mismatch``."""
        if status == "error" or row["status"] != "error":
            row["status"] = status
        row["problems"].append(problem)

    def error(self, row, f):
        """Mark a row as an error from a structured failure (see ``failure``)."""
        row.setdefault("failures", []).append(f)
        self.fail(row, "error", failure_text(f))

    def counts(self):
        c = {"pass": 0, "mismatch": 0, "error": 0, "skip": 0}
        for r in self.rows:
            c[r["status"]] += 1
        return c


class _Parser(argparse.ArgumentParser):
    """An argument parser whose errors never echo the rejected input."""

    def error(self, message):
        opts = [o for a in self._actions for o in a.option_strings if o.startswith("--")]
        m = re.match(r"argument (--[a-z-]+)", message or "")
        name = m.group(1) if m and m.group(1) in opts else None
        usage_exit(f"invalid value for {name}; see --help" if name else "invalid command line; see --help")


def usage_exit(text):
    """Print a fixed usage message (no user input) and exit with status 2."""
    print(f"bench_public.py: error: {text}", file=sys.stderr, flush=True)
    raise SystemExit(2)


def main(argv=None):
    ap = _Parser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quick", action="store_true", help="small sizes (CI smoke test)")
    ap.add_argument("--tools", default=",".join(TOOLS),
                    help="comma-separated subset of engine,pydsstools,hecdss,monolith (default: all)")
    ap.add_argument("--reps", type=int, default=None, help="timed repetitions after one warm-up (default 5; --quick 1)")
    ap.add_argument("--out", default="bench_public_results", help="folder for bench_public.json and .md")
    ap.add_argument("--versions", default="7,6", help="DSS versions to test (default 7,6)")
    ap.add_argument("--kinds", default=",".join(FULL),
                    help="comma-separated data kinds to run (default: all of " + ",".join(FULL) + ")")
    ap.add_argument("--ops", default="catalog,read,write", help="which operation groups to run")
    ap.add_argument("--widths", default="float32,float64", help="value widths of the TS/PD write cases")
    ap.add_argument("--write-modes", default=",".join(ENGINE_WRITE_MODES), help="engine write modes to run")
    ap.add_argument("--python", action="append", default=[], metavar="TOOL=PATH",
                    help="run TOOL's worker with another interpreter (repeatable)")
    ap.add_argument("--max-cpu", type=float, default=None, metavar="PCT",
                    help="wait before each timed case until system CPU is at most PCT (off by default)")
    ap.add_argument("--max-wait", type=float, default=1800.0, help="seconds to wait for a quiet machine")
    ap.add_argument("--allow-unmeasured-load", action="store_true",
                    help="with --max-cpu, run anyway if CPU load cannot be measured (reported as not applied)")
    ap.add_argument("--fixtures", default=None, metavar="DIR",
                    help="folder of the public DSS fixtures (default: fixtures/public of this checkout)")
    ap.add_argument("--no-fixtures", action="store_true", help="skip the public-fixture catalog cases")
    ap.add_argument("--timeout", type=float, default=3600.0, help="per-case worker timeout, seconds")
    ap.add_argument("--diagnostics", default=None, metavar="FILE",
                    help="also write raw error text and tracebacks to FILE: local use only, not for publishing")
    ap.add_argument("--keep", action="store_true", help="keep the temporary folder")
    ap.add_argument("--worker", nargs=2, help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    if a.worker:
        worker_main(*a.worker)
        return 0

    def csv(s, allowed, what):
        vals = [x.strip() for x in s.split(",") if x.strip()]
        for x in vals:
            if x not in allowed:
                usage_exit(f"invalid value for {what}; see --help")
        return vals

    kinds = csv(a.kinds, FULL, "--kinds")
    ops = csv(a.ops, ("catalog", "read", "write"), "--ops")
    widths = csv(a.widths, ("float32", "float64"), "--widths")
    write_modes = csv(a.write_modes, ENGINE_WRITE_MODES, "--write-modes")
    wanted = csv(a.tools, TOOLS, "--tools")
    versions = [int(v) for v in csv(a.versions, ("6", "7"), "--versions")]
    sizes = {k: v for k, v in (QUICK if a.quick else FULL).items() if k in kinds}
    reps = a.reps if a.reps is not None else (1 if a.quick else 5)
    pythons = {t: sys.executable for t in TOOLS}
    custom = set()
    for spec in a.python:
        t, _, p = spec.partition("=")
        if t not in TOOLS or not p:
            usage_exit("invalid value for --python (expected TOOL=PATH); see --help")
        pythons[t] = p
        custom.add(t)

    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    rep = Report()
    diagnostics = []
    skipped, notes, tools = {}, [], []
    started = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    guard = None
    run = None
    work = None
    exit_code = 0
    try:
        guard = LoadGuard(a.max_cpu, a.max_wait, a.allow_unmeasured_load)
        for t in wanted:
            ok, why = tool_available(t, pythons[t])
            (tools.append(t) if ok else skipped.__setitem__(t, why))
        for t in TOOLS:
            if t not in wanted:
                skipped[t] = "not requested (--tools)"
        if "engine" not in tools:
            raise EngineMissing("the engine (ras_commander_fast_dss) is needed to generate inputs and check "
                                "writes: " + skipped.get("engine", ""))
        work = Path(tempfile.mkdtemp(prefix="dss_bench_"))
        run = Runner(a, work, pythons, guard, diagnostics)
        run_cases(a, rep, run, work, tools, sizes, ops, widths, write_modes, versions, reps, notes)
    except GuardRefused as e:  # the message is built by LoadGuard from numbers only
        rep.fatal.append({"status": "refused", "category": "load_guard", "problem": str(e)})
        exit_code = 5
    except EngineMissing as e:
        rep.fatal.append({"status": "error", "category": "engine_missing", "problem": str(e)})
    except Exception as e:  # noqa: BLE001 - recorded, and the report is still written
        import traceback
        diagnostics.append(f"fatal: {traceback.format_exc()}")
        f = failure("internal_error", type(e).__name__)
        rep.fatal.append({"status": "error", "category": f["category"], "error_class": f["error_class"],
                          "problem": failure_text(f)})
    finally:
        if work is not None:
            if a.keep:
                say(f"kept the temporary folder {work.name} (in the system temporary folder)")
            else:
                shutil.rmtree(work, ignore_errors=True)
        if guard is not None and guard.note:
            notes.append(guard.note)
        counts = rep.counts()
        if exit_code == 0:
            if counts["error"] or rep.fatal:
                exit_code = 4
            elif counts["mismatch"]:
                exit_code = 3
        gen_s = run.gen_s if run is not None else None
        report = {
            "schema": 2,
            "started": started,
            "sizes": "quick" if a.quick else "full",
            "reps": reps,
            "outcome": {"exit_status": exit_code, **counts, "fatal": rep.fatal,
                        "verified_all": exit_code == 0 and counts["pass"] > 0},
            "machine": machine(),
            "tools": {t: rep.tool_versions.get(t) for t in tools},
            "separate_interpreter": sorted(custom),
            "skipped_tools": skipped,
            "load_guard": {"max_cpu_pct": a.max_cpu, "applied": bool(guard and guard.applied),
                           "samples_pct": guard.samples if guard else [],
                           "note": guard.note if guard else None},
            "input_generation_s": round(gen_s, 3) if gen_s is not None else None,
            "notes": notes,
            "rows": rep.rows,
        }
        report = scrub_tree(report)
        (out / "bench_public.json").write_text(json.dumps(report, indent=1), encoding="utf-8")
        (out / "bench_public.md").write_text(markdown(report), encoding="utf-8")
        if a.diagnostics:  # raw text, local only; never part of the report
            try:
                diagnostics.append(f"cpu model as reported by the OS: {_cpu_model_raw()!r}")
            except Exception as e:  # noqa: BLE001 - diagnostics are best effort
                diagnostics.append(f"cpu model could not be read: {type(e).__name__}")
            Path(a.diagnostics).write_text(
                "Raw diagnostics of a bench_public.py run. NOT FOR PUBLISHING: may contain paths, user and host "
                "names.\n\n" + "\n".join(diagnostics) + "\n", encoding="utf-8")
        say("report written to the --out directory (bench_public.json, bench_public.md)")
        if exit_code:
            say(f"FAILED (exit status {exit_code}): {counts['mismatch']} mismatch(es), {counts['error']} error(s), "
                f"{len(rep.fatal)} run-level failure(s); see the top of the report", err=True)
    return exit_code


def run_cases(a, rep, run, work, tools, sizes, ops, widths, write_modes, versions, reps, notes):
    def dss_tools(v):
        return [t for t in tools if not (t == "hecdss" and v == 6)]

    if "hecdss" in tools and 6 in versions:
        notes.append("hecdss reads and writes DSS7 only; it is not run on DSS6 files.")

    def timing(row, res):
        times = res.get("times_s") or []
        row.update(warm_median_s=statistics.median(times) if times else None,
                   warm_min_s=min(times) if times else None, first_run_s=res.get("first_s"), reps=len(times),
                   cpu_before_pct=res.get("cpu_before_pct"))
        if res.get("versions"):
            rep.tool_versions.setdefault(row["tool"], public_versions(res["versions"]))
        if res.get("failure"):
            rep.error(row, res["failure"])

    def show(row):
        say(f"  {row['case']:<28} DSS{row['dss']} {row['mode']:<20} {row.get('width') or '':<8} "
            f"{fmt_s(row.get('warm_median_s')):>10}  {row['status'].upper()}"
            + (f": {row['problems'][0][:160]}" if row["problems"] else ""))

    # ---------------------------------------------------------------- generate (engine)
    reads_wanted = bool({"catalog", "read"} & set(ops))
    gen_ok = True
    if reads_wanted:
        say(f"generating inputs ({'quick' if a.quick else 'full'} sizes) ...")
        (work / "in").mkdir()
        gen = [{"path": str(work / "in" / f"{k}_v{v}.dss"), "kind": k, "version": v, "sizes": sizes[k],
                "width": "float64"} for v in versions for k in sizes if k in READ_KINDS]
        t = time.perf_counter()
        r = run.run("engine", "engine_session_none", "gen", files=gen)
        run.gen_s = time.perf_counter() - t
        chk = run.run("engine", "engine", "verify", targets=gen) if not r.get("failure") else {}
        problems = [r["failure"]] if r.get("failure") else []
        if chk.get("failure"):
            problems.append(chk["failure"])
        for g, vres in zip(gen, chk.get("verify") or [None] * len(gen)):
            exp = [expected_digest(g["kind"], g["sizes"]), kind_records(g["kind"], g["sizes"])]
            if not vres or vres.get("error_type") or [vres.get("digest"), vres.get("records")] != exp:
                problems.append(failure("generation_failed", vres.get("error_type") if vres else None,
                                        f"{Path(g['path']).name} does not read back as generated"))
        if problems:
            gen_ok = False
            f = problems[0]
            rep.fatal.append({"status": "error", "category": "generation_failed", "error_class": f.get("error_class"),
                              "problem": FAILURE_TEXT["generation_failed"] + ": " + failure_text(f)})
            say("input generation FAILED; read cases are not run", err=True)

    # ---------------------------------------------------------------- reads
    if reads_wanted and gen_ok:
        say("reads ...")
        for v in versions:
            for kind in [k for k in READ_KINDS if k in sizes]:
                s = sizes[kind]
                path = str(work / "in" / f"{kind}_v{v}.dss")
                its = items(kind, s)
                names = [it[0] for it in its]
                desc = KIND_TEXT[kind].format(records=kind_records(kind, s), **s)
                width = "float32" if kind == "grid" else "float64"
                cases = []
                if "catalog" in ops and kind in ("catalog", "small"):
                    cases.append(("catalog", f"catalog: {kind}", {}, kind_records(kind, s), None))
                if "read" in ops and kind in ("small", "ts_regular", "ts_irregular"):
                    cases.append(("read_ts", f"read every TS: {kind}", {"datasets": names, "windows": windows(kind, its)},
                                  None, expected_digest(kind, s)))
                if "read" in ops and kind == "pd":
                    cases.append(("read_pd", "read every PD record", {"records": names}, None, expected_digest(kind, s)))
                if "read" in ops and kind == "grid":
                    cases.append(("read_grid", "read every grid", {"records": names}, None, expected_digest(kind, s)))
                for op, case, extra, exp_count, exp_digest in cases:
                    for tool in dss_tools(v):
                        res = run.run(tool, tool, op, path=path, reps=reps, **extra)
                        row = rep.add(case=case, op="catalog" if op == "catalog" else "read", kind=desc, dss=v,
                                      tool=tool, mode=tool, api=tool, width=None if op == "catalog" else width,
                                      sync=SYNC_TEXT["read"], expected=exp_count if op == "catalog" else exp_digest,
                                      dataset=f"{kind}_v{v}.dss")
                        timing(row, res)
                        if not res.get("failure"):
                            # Catalogs: every pass must return the generated record count. Reads:
                            # every timed pass must return as many values (stored and padding) as the
                            # untimed check pass, whose valid values must equal the generated ones.
                            want = exp_count if op == "catalog" else res.get("digest_pass_size")
                            counts = res.get("pass_counts") or []
                            bad = [i for i, c in enumerate(counts) if c != want]
                            if len(counts) != reps + 1:
                                rep.fail(row, "error", "not every pass returned a result")
                            if bad:
                                rep.fail(row, "mismatch", f"pass(es) {bad} returned {sorted(set(counts))} values, "
                                                          f"the check pass {want}")
                            if op != "catalog":
                                row["digest"] = res.get("digest")
                                if res.get("digest") != exp_digest:
                                    rep.fail(row, "mismatch", f"values digest {res.get('digest')} != generated {exp_digest}")
                        show(row)

    # ---------------------------------------------------------------- public fixtures (catalog)
    if "catalog" in ops and not a.no_fixtures:
        fix_dir = Path(a.fixtures) if a.fixtures else REPO / "fixtures" / "public"
        if fix_dir.is_dir():
            say("public fixtures (catalog) ...")
            (work / "fixtures").mkdir()
            for name in PUBLIC_FIXTURES:
                src = fix_dir / name
                if not src.is_file():
                    continue
                dst = work / "fixtures" / name  # tools read a copy: some HEC tools write on open
                shutil.copyfile(src, dst)
                with open(dst, "rb") as fh:
                    head = fh.read(18)
                ver = 7 if head[16:18] == b"7-" else 6  # the header's version string, "6-.." or "7-.."
                if ver not in versions:
                    continue
                ref = None
                for tool in dss_tools(ver):
                    res = run.run(tool, tool, "catalog", path=str(dst), reps=reps)
                    row = rep.add(case=f"catalog: fixture {name}", op="catalog", kind="public fixture", dss=ver,
                                  tool=tool, mode=tool, api=tool, width=None, sync=SYNC_TEXT["read"], dataset=name)
                    timing(row, res)
                    counts = res.get("pass_counts") or []
                    if tool == "engine":
                        ref = counts[0] if counts and not res.get("failure") else None
                        row["expected"] = "reference (engine)"
                    else:
                        row["expected"] = ref
                    if not res.get("failure"):
                        if ref is None:
                            rep.fail(row, "error", "no engine reference count for this fixture")
                        elif any(c != ref for c in counts) or len(counts) != reps + 1:
                            rep.fail(row, "mismatch", f"catalog counts {sorted(set(counts))} != engine {ref}")
                    show(row)
        else:
            notes.append("public fixtures folder not found; fixture catalog cases skipped (see --fixtures).")

    # ---------------------------------------------------------------- writes
    if "write" not in ops:
        return
    say("writes ...")
    (work / "out").mkdir()
    for v in versions:
        for kind in [k for k in WRITE_KINDS if k in sizes]:
            s = sizes[kind]
            desc = KIND_TEXT[kind].format(records=kind_records(kind, s), **s)
            exp, exp_t = expected_digest(kind, s), expected_times(kind, s)
            pending = []
            for width in (["float32"] if kind == "grid" else widths):
                for tool in dss_tools(v):
                    modes = write_modes if tool == "engine" else [tool]
                    for mode in modes:
                        api, sync = ENGINE_WRITE_MODES[mode] if tool == "engine" else ("tool API", "os")
                        base = {"case": f"write: {kind}", "op": "write", "kind": desc, "dss": v, "tool": tool,
                                "mode": mode, "api": api, "width": width, "sync": SYNC_TEXT[sync], "expected": exp}
                        if kind != "grid" and width not in WRITE_WIDTHS[tool]:
                            rep.add(**base, status="skip", problems=[WIDTH_REASON[tool]])
                            continue
                        path = str(work / "out" / f"{mode}_{kind}_{width}_v{v}.dss")
                        res = run.run(tool, mode, "write", path=path, kind=kind, version=v, sizes=s, reps=reps,
                                      width=width)
                        row = rep.add(**base, dataset=Path(path).name[:-4] + "_<rep>.dss")
                        timing(row, res)
                        # The files this case should have written, named by this script; the
                        # worker's own list is only compared with them, never published or opened.
                        expected_files = [f"{path[:-4]}_{k}.dss" for k in range(reps + 1)]
                        reported = res.get("written") or []
                        row["files_written"] = sum(Path(f).exists() for f in expected_files)
                        if not res.get("failure") and reported != expected_files:
                            rep.fail(row, "error", f"the worker reported {len(reported)} files written, "
                                                   f"{reps + 1} expected")
                        pending.append((row, expected_files))
            # Verify every repetition's file (warm-up included), then delete them all.
            targets = [{"path": p, "kind": kind, "version": v, "sizes": s} for _, ws in pending for p in ws
                       if Path(p).exists()]
            results = {}
            if targets:
                chk = run.run("engine", "engine", "verify", targets=targets)
                if chk.get("failure"):
                    for row, _ in pending:
                        rep.error(row, failure("readback_failed", chk["failure"].get("error_class"),
                                               chk["failure"]["message"]))
                for t, vr in zip(targets, chk.get("verify") or []):
                    results[t["path"]] = vr
            for row, written in pending:
                ok_files = 0
                for i, p in enumerate(written):
                    vr = results.get(p)
                    label = ("warm-up" if i == 0 else f"repetition {i}") + f" ({Path(p).name})"
                    if vr is None:
                        if row["status"] != "error":
                            rep.fail(row, "mismatch", f"{label}: file missing, not verified")
                        continue
                    if vr.get("error_type"):
                        rep.fail(row, "mismatch", f"{label}: {FAILURE_TEXT['readback_exception']} "
                                                  f"[{class_name(vr['error_type'])}]")
                        continue
                    probs = []
                    if vr["digest"] != exp:
                        probs.append(f"values {vr['digest']} != generated {exp}")
                    if exp_t is not None and vr["times"] != exp_t:
                        probs.append(f"times {vr['times']} != generated {exp_t}")
                    if vr["version"] != v:
                        probs.append(f"stored as DSS{vr['version']}")
                    want_types = RECORD_TYPES.get((kind, row["width"]))
                    if want_types is not None and not set(vr["record_types"]) <= want_types:
                        probs.append(f"record types {vr['record_types']} are not {row['width']}")
                    if kind == "grid" and not all(400 <= t < 500 for t in vr["record_types"]):
                        probs.append(f"record types {vr['record_types']} are not grids")
                    row.setdefault("stored_record_types", vr["record_types"])
                    if probs:
                        rep.fail(row, "mismatch", f"{label}: " + "; ".join(probs))
                    else:
                        ok_files += 1
                row["files_verified_equal"] = ok_files
                show(row)
            for _, ws in pending:
                for p in ws:
                    try:
                        Path(p).unlink()
                    except OSError:
                        pass


def markdown(rep):
    m, o = rep["machine"], rep["outcome"]
    md = ["# HEC-DSS benchmark on synthetic data", ""]
    failed = [r for r in rep["rows"] if r["status"] in ("mismatch", "error")]
    if o["exit_status"] != 0:
        md += [f"## RUN FAILED (exit status {o['exit_status']}): {o['mismatch']} mismatch(es), {o['error']} error(s), "
               f"{len(o['fatal'])} run-level failure(s)", ""]
        for f in o["fatal"]:
            md.append(f"- **{f['status']}** ({f.get('category')}): {f['problem']}")
        if failed:
            md += ["", "| Case | DSS | Tool | Width | Outcome | Category | Exception class | Dataset | Detail |",
                   "|---|---|---|---|---|---|---|---|---|"]
            for r in failed:
                fs = r.get("failures") or []
                md.append(f"| {r['case']} | {r['dss']} | {r['mode']} | {r.get('width') or ''} | "
                          f"{r['status'].upper()} | {', '.join(f['category'] for f in fs) or 'mismatch'} | "
                          f"{', '.join(f['error_class'] for f in fs if f.get('error_class')) or '–'} | "
                          f"{r.get('dataset') or ''} | {'; '.join(r['problems'])[:300]} |")
        md += ["", "Correctness is **not** established for this run.", ""]
    elif o["verified_all"]:
        md += [f"**Correctness:** all {o['pass']} attempted cases passed. Catalogs returned the generated record "
               "count on every pass (public fixtures: the engine's count). Every timed read pass returned as many "
               "values as an untimed check pass, whose values matched the generated values exactly (count and exact "
               "sum). Every written file (warm-up and each repetition) read back with the engine equal to the "
               "generated values and times, at the stated width and DSS version." + (f" {o['skip']} cell(s) were skipped (listed below) and are not counted as verified."
                                  if o["skip"] else ""), ""]
    md += ["## Machine and tools", "",
           f"- OS: {m['os']} {m['os_release']} (version {m['os_version']}, {m['architecture']}); "
           f"CPU: {m['logical_cores']} logical cores"
           + (f", {m['physical_cores']} physical" if m.get("physical_cores") else "") + f"; Python {m['python']}.",
           f"- Sizes: {rep['sizes']}; {rep['reps']} timed repetitions after one warm-up; started {rep['started']}."]
    for t, v in rep["tools"].items():
        md.append(f"- {t}: " + (", ".join(f"{k} {val}" for k, val in (v or {}).items()) or "version unknown"))
    for t, why in rep["skipped_tools"].items():
        md.append(f"- {t}: skipped ({why})")
    lg = rep["load_guard"]
    if lg["max_cpu_pct"] is None:
        md.append("- Load guard: off (no --max-cpu).")
    elif not lg["applied"]:
        md.append(f"- Load guard: **NOT applied** ({lg['note'] or 'the run stopped before it was applied'}).")
    else:
        s = [x for x in lg["samples_pct"] if x is not None]
        md.append(f"- Load guard: at most {lg['max_cpu_pct']:g} % system CPU before each timed case"
                  + (f"; measured median {statistics.median(s):.1f} %, max {max(s):.1f} %" if s else ""))
    for n in rep["notes"]:
        md.append(f"- {n}")
    md += ["", "Times are the warm median; \"first\" is the warm-up run, the first call in a fresh process "
           "(OS file cache not flushed). Width is the stored value width. Sync is the flush policy of the "
           "write; the other tools request no flush, so their closest engine equivalent is sync=\"none\".", ""]
    for what, title in (("catalog", "Catalog"), ("read", "Reads"), ("write", "Writes")):
        sel = [r for r in rep["rows"] if r["op"] == what and r["status"] != "skip"]
        if not sel:
            continue
        md += [f"## {title}", "", "| Case | Data | DSS | Tool | Width | Sync | Warm median | First run | Status |",
               "|---|---|---|---|---|---|---:|---:|---|"]
        for r in sel:
            st = r["status"].upper() if r["status"] != "pass" else "pass"
            if r["problems"] and r["status"] != "pass":
                st += f": {r['problems'][0][:100]}"
            label = MODE_LABEL.get(r["mode"], r["mode"]) if what == "write" else (r["tool"] if r["tool"] == "engine" else MODE_LABEL.get(r["tool"], r["tool"]))
            md.append(f"| {r['case']} | {r['kind']} | {r['dss']} | {label} | "
                      f"{r.get('width') or '–'} | {r['sync'].split(':')[0]} | {fmt_s(r.get('warm_median_s'))} | "
                      f"{fmt_s(r.get('first_run_s'))} | {st} |")
        md.append("")
    skips = [r for r in rep["rows"] if r["status"] == "skip"]
    if skips:
        md += ["## Skipped cells (not timed, not verified)", "", "| Case | DSS | Tool | Width | Reason |",
               "|---|---|---|---|---|"]
        for r in skips:
            md.append(f"| {r['case']} | {r['dss']} | {r['mode']} | {r.get('width') or ''} | {'; '.join(r['problems'])} |")
        md.append("")
    md += ["## Sync policies", ""] + [f"- {v}" for v in SYNC_TEXT.values()] + [""]
    return scrub("\n".join(md)) + "\n"


if __name__ == "__main__":
    sys.exit(main())

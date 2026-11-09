"""P64: the public opt-in flush policy (``sync=``) of the TS, paired-data, grid, profile and
location writers.

The default stays ``sync=True`` (``fsync`` at the transaction's existing flush points and
one terminal ``sync_all``). ``sync=False`` writes the same records in the same order
without flushing; the killed-process qualification is the Rust matrix
``crates/dss-core/tests/p64_kill.rs`` (``docs/format/dss7.md`` §6). These tests check
the public surface: the default, the pass-through to the native writers, equal records in
both modes, strict ``bool`` input, and OS kills of a process writing through the public
API in both modes, between writes and during them.

Synthetic data in ``tmp_path`` only.
"""

from __future__ import annotations

import inspect
import os
import queue
import signal
import subprocess
import sys
import textwrap
import threading
import time

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import _native

WRITERS = [
    dss.write_timeseries,
    dss.write_paired_data,
    dss.write_grid,
    dss.write_profile,
    dss.write_location,
]

SHG = dict(grid_type="shg", cell_size=2000.0, units="MM", data_type="PER-CUM",
           lower_left_cell=(-1000, 700))
PD_KW = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")


def new(tmp_path, name, v):
    p = tmp_path / f"{name}{v}.dss"
    dss.create(p, version=v)
    return p


@pytest.mark.parametrize("fn", WRITERS, ids=lambda f: f.__name__)
def test_default_is_durable(fn):
    p = inspect.signature(fn).parameters["sync"]
    assert p.default is True
    assert p.kind is inspect.Parameter.KEYWORD_ONLY


def write_all(p, v, sync):
    """One write of every kind the version supports; returns what to read back."""
    kw = {} if sync is None else {"sync": sync}
    vals = np.arange(60, dtype=np.float64) * 0.25
    dss.write_timeseries(p, "/A/RTS/FLOW//1HOUR/S/", vals, start="01JAN2021 01:00",
                         units="CFS", data_type="INST-VAL", version=v, **kw)
    # A second write merges into the stored block.
    dss.write_timeseries(p, "/A/RTS/FLOW//1HOUR/S/", vals[:10] + 100, start="01JAN2021 05:00",
                         units="CFS", data_type="INST-VAL", version=v, **kw)
    t = pd.to_datetime(["2021-02-01 00:30", "2021-02-03 07:00", "2021-02-09 00:00"])
    dss.write_timeseries(p, "/A/ITS/STAGE//IR-MONTH/S/", [4.5, 5.25, 6.0], times=t,
                         units="FT", data_type="INST-VAL", version=v, **kw)
    dss.write_paired_data(p, "/A/PD/STAGE-FLOW///S/", [0.0, 1.0, 2.0],
                          [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]], version=v, **PD_KW, **kw)
    g = (np.arange(12, dtype=np.float32) * 0.5).reshape(3, 4)
    dss.write_grid(p, "/SHG/A/PRECIP/01SEP2018:0100/01SEP2018:0200/S/", g, version=v, **SHG, **kw)
    if v == 7:
        dss.write_profile(p, "/A/LAKE/DEPTH-TEMP//1Hour/S/", [0.5, 1.5, 3.0],
                          np.arange(30, dtype=np.float64).reshape(10, 3) + 1.5,
                          start="10JAN2020 01:00", depth_units="m", value_units="deg C",
                          data_type="INST-VAL", **kw)
        dss.write_location(p, "/A/LAKE/Location Info////", x=-97.25, y=30.5, z=10.0, **kw)
        dss.write_timeseries(p, "/A/NOTES/FLOW//1DAY/S/", [1.0, 2.0], start="01JAN2021",
                             units="CFS", data_type="INST-VAL", notes=["a", "b"], **kw)
    dss.write_timeseries(p, "/A/PAT/FLOW/TS-PATTERN/1DAY/S/", [1.0, 2.0, 3.0],
                         units="CFS", data_type="INST-VAL", version=v, **kw)


def contents(p, v):
    with dss.open(p) as f:
        cat = f.catalog()
        out = {"catalog": cat.drop(columns=[c for c in cat.columns if "write" in c])}
        out["rts"] = f.read_timeseries("/A/RTS/FLOW//1HOUR/S/", trim_missing=False)
        out["its"] = f.read_timeseries("/A/ITS/STAGE//IR-MONTH/S/")
        out["pattern"] = f.read_timeseries("/A/PAT/FLOW/TS-PATTERN/1DAY/S/")
        if v == 7:
            out["notes"] = f.read_timeseries("/A/NOTES/FLOW//1DAY/S/")
        pdr = f.read_paired_data("/A/PD/STAGE-FLOW///S/")
        out["pd"] = (pdr.ordinates.tobytes(), pdr.values.tobytes())
        g = f.read_grid("/SHG/A/PRECIP/01SEP2018:0100/01SEP2018:0200/S/", missing="raw")
        out["grid"] = g.data.tobytes()
        if v == 7:
            pr = f.read_profile("/A/LAKE/DEPTH-TEMP//1Hour/S/")
            out["profile"] = (pr.depths.tobytes(), pr.values.tobytes())
            loc = f.read_location("/A/LAKE/Location Info////")
            out["location"] = (loc.x, loc.y, loc.z)
    return out


def assert_same(a, b):
    assert a.keys() == b.keys()
    for k in a:
        if isinstance(a[k], pd.DataFrame):
            pd.testing.assert_frame_equal(a[k], b[k], check_exact=True)
            if a[k].to_numpy().dtype != object:
                # Raw bits (NaN payloads, signed zeros) beyond assert_frame_equal.
                assert a[k].to_numpy().tobytes() == b[k].to_numpy().tobytes(), k
        else:
            assert a[k] == b[k], k


@pytest.mark.parametrize("v", [7, 6])
def test_both_modes_store_the_same_records(tmp_path, v):
    a = new(tmp_path, "default", v)
    b = new(tmp_path, "explicit", v)
    c = new(tmp_path, "nosync", v)
    write_all(a, v, None)
    write_all(b, v, True)
    write_all(c, v, False)
    ca = contents(a, v)
    assert_same(ca, contents(b, v))
    assert_same(ca, contents(c, v))
    with dss.open(c) as f:
        assert not f.catalog().attrs["anomalies"]


@pytest.mark.parametrize("v", [7, 6])
def test_sync_reaches_the_native_writers(tmp_path, monkeypatch, v):
    seen = []
    names = ["write_rts", "write_its", "write_rts6", "write_its6", "write_pd", "write_grid",
             "write_profile", "write_location", "write_rts_variants", "write_its_variants",
             "write_pattern"]
    for name in names:
        real = getattr(_native, name)

        def spy(*args, _real=real, _name=name, **kwargs):
            seen.append((_name, kwargs.get("sync", "missing")))
            return _real(*args, **kwargs)

        monkeypatch.setattr(_native, name, spy)
    for sync in (None, True, False):
        seen.clear()
        p = new(tmp_path, f"spy{sync}", v)
        write_all(p, v, sync)
        want = True if sync is None else sync
        assert seen and all(s == want for _, s in seen), seen
        called = {n for n, _ in seen}
        if v == 7:
            assert {"write_rts", "write_its", "write_pd", "write_grid", "write_profile",
                    "write_location", "write_rts_variants", "write_pattern"} <= called
        else:
            assert {"write_rts6", "write_its6", "write_pd", "write_grid", "write_pattern"} <= called


@pytest.mark.parametrize("bad", ["false", 0, 1, None, 0.0])
def test_sync_must_be_a_bool(tmp_path, bad):
    p = new(tmp_path, "bad", 7)
    before = p.read_bytes()
    calls = [
        lambda: dss.write_timeseries(p, "/A/B/FLOW//1HOUR/S/", [1.0], start="01JAN2021 01:00",
                                     units="CFS", data_type="INST-VAL", sync=bad),
        lambda: dss.write_paired_data(p, "/A/PD/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0],
                                      sync=bad, **PD_KW),
        lambda: dss.write_grid(p, "/SHG/A/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
                               np.zeros((2, 2), np.float32), sync=bad, **SHG),
        lambda: dss.write_profile(p, "/A/LAKE/DEPTH-TEMP//1Hour/S/", [0.5], [[1.0]],
                                  start="10JAN2020 01:00", depth_units="m",
                                  value_units="deg C", data_type="INST-VAL", sync=bad),
        lambda: dss.write_location(p, "/A/LAKE/Location Info////", x=1.0, sync=bad),
    ]
    for call in calls:
        with pytest.raises(TypeError, match="sync must be True or False"):
            call()
    assert p.read_bytes() == before


def test_numpy_bool_accepted(tmp_path):
    p = new(tmp_path, "npbool", 7)
    dss.write_timeseries(p, "/A/B/FLOW//1HOUR/S/", [1.0], start="01JAN2021 01:00",
                         units="CFS", data_type="INST-VAL", sync=np.bool_(False))
    assert dss.read_timeseries(p, "/A/B/FLOW//1HOUR/S/").iloc[:, 0].tolist() == [1.0]


def test_dssfile_methods_pass_sync(tmp_path):
    p = new(tmp_path, "handle", 7)
    with dss.open(p) as f:
        f.write_timeseries("/A/B/FLOW//1HOUR/S/", [1.0, 2.0], start="01JAN2021 01:00",
                           units="CFS", data_type="INST-VAL", sync=False)
        f.write_paired_data("/A/PD/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], sync=False, **PD_KW)
    assert dss.read_timeseries(p, "/A/B/FLOW//1HOUR/S/").iloc[:, 0].tolist() == [1.0, 2.0]


CHILD = textwrap.dedent(
    """
    import os
    import sys
    import numpy as np
    import pandas as pd
    import ras_commander_fast_dss as dss
    T = pd.to_datetime(["2021-02-01 00:30", "2021-02-03 07:00"])
    path, v, sync, pause = sys.argv[1], int(sys.argv[2]), sys.argv[3] == "1", sys.argv[4] == "1"
    PD = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")
    SHG = dict(grid_type="shg", cell_size=2000.0, units="MM", data_type="PER-CUM",
               lower_left_cell=(-1000, 700))
    print(f"ready {os.getpid()}", flush=True)
    for i in range(100000):
        dss.write_timeseries(path, f"/K/R{i}/FLOW//1HOUR/S/", np.arange(48.0) + i,
                             start="01JAN2021 01:00", units="CFS", data_type="INST-VAL",
                             version=v, sync=sync)
        dss.write_timeseries(path, f"/K/I{i}/STAGE//IR-MONTH/S/", [i + 0.5, i + 1.5],
                             times=T, units="FT", data_type="INST-VAL", version=v, sync=sync)
        dss.write_paired_data(path, f"/K/P{i}/STAGE-FLOW///S/", [0.0, 1.0], [i, i + 0.5],
                              version=v, sync=sync, **PD)
        dss.write_grid(path, f"/SHG/K{i}/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
                       np.full((3, 4), i, np.float32), version=v, sync=sync, **SHG)
        if v == 7:
            dss.write_profile(path, f"/K/L{i}/DEPTH-TEMP//1Hour/S/", [0.5, 1.5],
                              np.arange(6, dtype=np.float64).reshape(3, 2) + i,
                              start="10JAN2020 01:00", depth_units="m", value_units="deg C",
                              data_type="INST-VAL", sync=sync)
            dss.write_location(path, f"/K/L{i}/Location Info////", x=float(i), y=30.5,
                               z=10.0, sync=sync)
        print(f"done {i}", flush=True)
        if pause and sys.stdin.readline().strip() != "go":
            break
    """
)


def kinds(v):
    return ["R", "I", "P", "G"] + (["L", "X"] if v == 7 else [])


def pathname(kind, i):
    return {
        "R": f"/K/R{i}/FLOW//1HOUR/S/",
        "I": f"/K/I{i}/STAGE//IR-MONTH/S/",
        "P": f"/K/P{i}/STAGE-FLOW///S/",
        "G": f"/SHG/K{i}/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
        "L": f"/K/L{i}/DEPTH-TEMP//1Hour/S/",
        "X": f"/K/L{i}/Location Info////",
    }[kind]


def check_record(f, kind, i):
    """Record ``i`` of ``kind`` reads back exactly what the child wrote."""
    p = pathname(kind, i)
    if kind == "R":
        got = f.read_timeseries(p).iloc[:, 0].to_numpy()
        assert got.tobytes() == (np.arange(48.0) + i).tobytes(), p
    elif kind == "I":
        assert f.read_timeseries(p).iloc[:, 0].tolist() == [i + 0.5, i + 1.5], p
    elif kind == "P":
        assert f.read_paired_data(p).values.ravel().tolist() == [i, i + 0.5], p
    elif kind == "G":
        g = f.read_grid(p, missing="raw").data
        assert g.tobytes() == np.full((3, 4), i, np.float32).tobytes(), p
    elif kind == "L":
        pr = f.read_profile(p)
        assert pr.depths.tolist() == [0.5, 1.5], p
        want = np.arange(6, dtype=np.float64).reshape(3, 2) + i
        assert pr.values.tobytes() == want.tobytes(), p
    else:
        loc = f.read_location(p)
        assert (loc.x, loc.y, loc.z) == (float(i), 30.5, 10.0), p


def readable(f, kind, i):
    """``True`` if record ``i`` of ``kind`` reads back exactly, ``False`` if it is absent."""
    try:
        check_record(f, kind, i)
    except dss.DssPathnameNotFoundError:
        return False
    return True


class Lines:
    """The child's stdout lines, each awaited with a deadline, so that a stuck child fails
    the test instead of hanging it."""

    def __init__(self, stream):
        self.q = queue.Queue()
        threading.Thread(target=self._pump, args=(stream,), daemon=True).start()

    def _pump(self, stream):
        for line in stream:
            self.q.put(line.strip())
        self.q.put(None)

    def next(self, timeout=120.0):
        try:
            line = self.q.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError("child produced no line in time") from None
        assert line is not None, "child exited early"
        return line


def after_lock(op, timeout=10.0):
    """``op()`` once the killed child's lock is gone: on Windows the kernel releases it as
    part of process teardown, which can lag ``wait``."""
    end = time.monotonic() + timeout
    while True:
        try:
            return op()
        except (dss.DssLockedError, PermissionError):
            if time.monotonic() > end:
                raise
            time.sleep(0.1)


def hard_kill(child, pid):
    """Kill the writing interpreter itself, without letting it exit, and wait until it is
    gone. On Windows a virtual environment's ``python.exe`` is a launcher that runs the
    base interpreter as a child process, so ``Popen.kill`` would stop only the launcher
    while the writer runs on; ``pid`` is the interpreter's own (``os.getpid()`` in the
    child). ``os.kill`` is TerminateProcess on Windows; SIGKILL elsewhere. The launcher
    exits once its interpreter has."""
    os.kill(pid, signal.SIGKILL if hasattr(signal, "SIGKILL") else signal.SIGTERM)
    child.wait(timeout=60)


def start(lines):
    word, pid = lines.next().split()
    assert word == "ready"
    return int(pid)


def run_child(tmp_path, v, sync, pause):
    p = new(tmp_path, f"killed{int(sync)}{int(pause)}", v)
    script = tmp_path / "child.py"
    script.write_text(CHILD)
    child = subprocess.Popen(
        [sys.executable, "-I", str(script), str(p), str(v), str(int(sync)), str(int(pause))],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    )
    return p, child, Lines(child.stdout)


def probe(f, v, start):
    """``(set, kind)`` and whether it reads back exactly, for the record sets from
    ``start`` in write order, up to and including the first set with no record."""
    out = []
    i = start
    while True:
        flags = [readable(f, k, i) for k in kinds(v)]
        out += [((i, k), x) for k, x in zip(kinds(v), flags)]
        if not any(flags):
            return out
        i += 1


def dataset(path):
    """A catalog row's record identity: the pathname in upper case, with the D part (the
    block date of a time series or profile) blanked except for a grid's date range."""
    parts = path.upper().split("/")
    if parts[1] != "SHG":
        parts[4] = ""
    return "/".join(parts)


def assert_prefix(seen):
    """Ordering of completed writes: the child writes one record after the other, so the
    records that read back are a prefix of the write order."""
    flags = [x for _, x in seen]
    n = sum(flags)
    assert flags == [True] * n + [False] * (len(flags) - n), seen


def verify_after_kill(p, v, completed):
    """The process-death contract at the public API. Before any repair: every record set
    ``< completed`` reads back exactly and the later records are each absent or exact, in
    a prefix of the write order. The next default write then reconciles; after it the
    readable records are still a prefix, include the earlier ones and at most the one
    interrupted write more (rolled forward), the catalog has no anomalies, and it lists
    exactly the records that read."""
    with after_lock(lambda: dss.open(p)) as f:
        for i in range(completed):
            for k in kinds(v):
                check_record(f, k, i)
        before = probe(f, v, completed)
    assert_prefix(before)
    rep = after_lock(lambda: dss.write_timeseries(
        p, "/K/AFTER/FLOW//1HOUR/S/", [1.0], start="01JAN2021 01:00", units="CFS",
        data_type="INST-VAL", version=v))
    with dss.open(p) as f:
        cat = f.catalog()
        assert not cat.attrs["anomalies"], cat.attrs["anomalies"]
        listed = {dataset(s) for s in cat["pathname"]}
        for i in range(completed):
            for k in kinds(v):
                check_record(f, k, i)
                assert dataset(pathname(k, i)) in listed
        after = probe(f, v, completed)
        check_record(f, "R", 0)
    assert_prefix(after)
    nb, na = sum(x for _, x in before), sum(x for _, x in after)
    assert nb <= na <= nb + 1, (before, after)
    for (i, k), x in after:
        assert x == (dataset(pathname(k, i)) in listed), (i, k)
    assert dataset("/K/AFTER/FLOW//1HOUR/S/") in listed
    return rep, na


@pytest.mark.parametrize("sync", [False, True], ids=["nosync", "default"])
@pytest.mark.parametrize("v", [7, 6])
def test_completed_writes_survive_an_os_kill_between_writes(tmp_path, v, sync):
    """Every write that returned before the writing process was killed (``hard_kill``:
    TerminateProcess on Windows, SIGKILL elsewhere) reads back exactly, in both flush
    modes, and the next default-mode write finds nothing to reconcile."""
    p, child, lines = run_child(tmp_path, v, sync, pause=True)
    n = 5
    try:
        pid = start(lines)
        for i in range(n):
            assert lines.next() == f"done {i}"
            if i < n - 1:
                child.stdin.write("go\n")
                child.stdin.flush()
        # Paused between writes, its handle closed by the API; killed without exiting.
        hard_kill(child, pid)
    finally:
        child.kill()
        child.wait(timeout=60)
    assert child.returncode != 0
    rep, extra = verify_after_kill(p, v, n)
    assert extra == 0
    assert not rep["reconciled_interrupted_write"]


@pytest.mark.parametrize("sync", [False, True], ids=["nosync", "default"])
@pytest.mark.parametrize("v", [7, 6])
def test_writes_survive_an_os_kill_during_a_write(tmp_path, v, sync):
    """The child writes without pausing; it is killed a short, varied delay after it
    reports a completed record set, so the kill lands somewhere in the following writes,
    often inside one record's transaction. Where exactly it lands is not controlled (the
    delay only spreads the kills; a loaded host shifts them), and the assertions hold
    wherever it lands:
    completed sets read back exactly, each later record is absent or exact and the
    readable ones are a prefix of the write order, and after the next write's
    reconciliation the catalog lists exactly the records that read. (The step-by-step kill points are the Rust matrix
    ``crates/dss-core/tests/p64_kill.rs``.)"""
    scale = 1.0 if sync else 0.2  # a default-mode record write is several times slower
    for trial, (after, delay) in enumerate(((0, 0.0), (1, 0.004), (2, 0.015), (3, 0.04),
                                            (4, 0.1))):
        sub = tmp_path / f"t{trial}"
        sub.mkdir()
        p, child, lines = run_child(sub, v, sync, pause=False)
        try:
            pid = start(lines)
            for i in range(after + 1):
                assert lines.next() == f"done {i}"
            time.sleep(delay * scale)
            hard_kill(child, pid)
        finally:
            child.kill()
            child.wait(timeout=60)
        assert child.returncode != 0
        verify_after_kill(p, v, after + 1)


def test_feature_flag():
    assert "write_sync_policy" in dss.FEATURES

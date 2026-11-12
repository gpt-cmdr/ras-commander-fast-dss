"""The public flush policy (``sync=``) of the TS, paired-data, grid, profile and
location writers, of ``create`` and of grid-to-point.

Since 0.1.2 the default is ``sync="end"``: one flush at the end of each write call.
``"step"`` also flushes at the transaction's flush points (the default up to 0.1.1) and
``"none"`` does not flush; ``True`` means ``"end"`` and ``False`` ``"none"``. The three write
the same records in the same order; the killed-process qualification is the Rust matrix
``crates/dss-core/tests/p64_kill.rs``.
These tests check the public surface: the default, the pass-through to the native writers,
the number of flushes each mode makes, equal records in every mode, strict input, and OS
kills of a process writing through the public API in every mode, between writes and during
them, through the module functions and through one ``DssFile`` session (its held handle).
They also cover the ``DssFile`` session's handle use.

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
    assert p.default == "end"
    assert p.kind is inspect.Parameter.KEYWORD_ONLY


def test_the_modes_are_named():
    assert dss.SYNC_MODES == ("end", "step", "none")
    assert "write_sync_modes" in dss.FEATURES


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
    write_all(a, v, None)
    ca = contents(a, v)
    for k, sync in enumerate((True, False, "end", "step", "none")):
        b = new(tmp_path, f"mode{k}_", v)
        write_all(b, v, sync)
        assert_same(ca, contents(b, v))
        with dss.open(b) as f:
            assert not f.catalog().attrs["anomalies"]


NAME = {None: "end", True: "end", False: "none", "end": "end", "step": "step", "none": "none"}


def flushes(fn):
    """Flushes the native writers make during ``fn()`` (the hidden process-wide counter)."""
    before = _native._flush_count()
    fn()
    return _native._flush_count() - before


@pytest.mark.parametrize("v", [7, 6])
def test_flushes_per_write_call(tmp_path, v):
    """``"end"``: exactly one flush per public write call, whatever the record kind or the
    number of blocks. ``"none"``: none. ``"step"``: several."""
    vals = np.arange(3 * 8760, dtype=np.float64)  # hourly, several blocks in one call
    for k, sync in enumerate((None, True, "end", False, "none", "step")):
        kw = {} if sync is None else {"sync": sync}
        p = tmp_path / f"flush_{k}_{v}.dss"
        assert flushes(lambda: dss.create(p, version=v, **kw)) == (0 if NAME[sync] == "none" else 1)
        calls = [
            lambda: dss.write_timeseries(p, "/A/RTS/FLOW//1HOUR/S/", vals, start="01JAN2021 01:00",
                                         units="CFS", data_type="INST-VAL", version=v, **kw),
            lambda: dss.write_timeseries(p, "/A/RTS/FLOW//1HOUR/S/", vals[:10] + 1,
                                         start="01JAN2021 05:00", units="CFS",
                                         data_type="INST-VAL", version=v, **kw),
            lambda: dss.write_paired_data(p, "/A/PD/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0],
                                          version=v, **PD_KW, **kw),
            lambda: dss.write_grid(p, "/SHG/A/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
                                   np.ones((3, 4), np.float32), version=v, **SHG, **kw),
        ]
        if v == 7:
            calls += [
                lambda: dss.write_profile(p, "/A/LAKE/DEPTH-TEMP//1Hour/S/", [0.5, 1.5],
                                          np.arange(6, dtype=np.float64).reshape(3, 2),
                                          start="10JAN2020 01:00", depth_units="m",
                                          value_units="deg C", data_type="INST-VAL", **kw),
                lambda: dss.write_location(p, "/A/LAKE/Location Info////", x=1.0, y=2.0, **kw),
            ]
        counts = [flushes(c) for c in calls]
        if NAME[sync] == "none":
            assert counts == [0] * len(calls), (sync, counts)
        elif NAME[sync] == "end":
            assert counts == [1] * len(calls), (sync, counts)
        else:
            assert all(c > 3 for c in counts), (sync, counts)
            assert counts[0] > 20, counts  # every block of the long series flushes its steps


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
    for k, sync in enumerate((None, True, False, "end", "step", "none")):
        seen.clear()
        p = new(tmp_path, f"spy{k}_", v)
        write_all(p, v, sync)
        want = NAME[sync]
        assert seen and all(s == want for _, s in seen), seen
        called = {n for n, _ in seen}
        if v == 7:
            assert {"write_rts", "write_its", "write_pd", "write_grid", "write_profile",
                    "write_location", "write_rts_variants", "write_pattern"} <= called
        else:
            assert {"write_rts6", "write_its6", "write_pd", "write_grid", "write_pattern"} <= called


@pytest.mark.parametrize("bad", ["false", "True", "END", "", 0, 1, None, 0.0])
def test_sync_must_be_a_mode_or_a_bool(tmp_path, bad):
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
        lambda: dss.create(tmp_path / "never.dss", sync=bad),
    ]
    for call in calls:
        with pytest.raises(ValueError if isinstance(bad, str) else TypeError, match="sync must be"):
            call()
    assert p.read_bytes() == before
    assert not (tmp_path / "never.dss").exists()


def test_the_native_writers_refuse_other_values(tmp_path):
    """The native layer has the same rule, for callers that bypass the Python checks."""
    for bad in ("false", 1, None):
        with pytest.raises(TypeError, match="sync must be"):
            _native.create_dss7(str(tmp_path / "n.dss"), sync=bad)
    assert not (tmp_path / "n.dss").exists()


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


@pytest.mark.parametrize("v", [7, 6])
def test_dssfile_session_opens_the_file_once_per_handle(tmp_path, monkeypatch, v):
    """Writes through a ``DssFile`` never reopen the read handle. The first write opens one read+write handle, which every later write and read
    uses; reads after a write see every record written."""
    p = new(tmp_path, "lazy", v)
    opens = []
    real = _native.open
    monkeypatch.setattr(_native, "open", lambda *a, **k: (opens.append(a), real(*a, **k))[1])
    with dss.open(p) as f:
        assert len(opens) == 1
        assert f._native._held_opens == 0 and not f._native._holds_write_handle
        for i in range(5):
            f.write_timeseries(f"/A/R{i}/FLOW//1HOUR/S/", [1.0 + i, 2.0], start="01JAN2021 01:00",
                               units="CFS", data_type="INST-VAL")
            f.write_paired_data(f"/A/P{i}/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0 + i], **PD_KW)
            f.write_grid(f"/SHG/A{i}/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
                         np.full((3, 4), i, np.float32), **SHG)
            if v == 7:
                f.write_location(f"/A/L{i}/Location Info////", x=float(i) + 1.0)
            assert f.version == v and f.recover is False and not f.closed
        assert len(opens) == 1, "a write reopened the read handle"
        assert f._native._held_opens == 1 and f._native._holds_write_handle
        # The next read uses the held handle and sees everything.
        assert f.read_timeseries("/A/R4/FLOW//1HOUR/S/").iloc[:, 0].tolist() == [5.0, 2.0]
        assert len(f.catalog()) >= 15
        assert f.read_paired_data("/A/P3/STAGE-FLOW///S/").values.ravel().tolist() == [1.0, 5.0]
        # A write after a read; the read after it sees the new record.
        f.write_timeseries("/A/R4/FLOW//1HOUR/S/", [9.0], start="01JAN2021 01:00",
                           units="CFS", data_type="INST-VAL")
        assert f.read_timeseries("/A/R4/FLOW//1HOUR/S/").iloc[:, 0].tolist() == [9.0, 2.0]
        f.write_paired_data("/A/LAST/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        assert len(opens) == 1 and f._native._held_opens == 1
    # Closed with a write pending a read: closed, both handles released, stays closed.
    assert f.closed and not f._native._holds_write_handle
    with pytest.raises(ValueError):
        f.read_timeseries("/A/R4/FLOW//1HOUR/S/")
    with pytest.raises(ValueError, match="closed DssFile"):
        f.write_paired_data("/A/NO/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
    f.close()
    assert dss.read_paired_data(p, "/A/LAST/STAGE-FLOW///S/").values.ravel().tolist() == [1.0, 2.0]


def test_dssfile_write_failure_leaves_the_file_readable(tmp_path):
    p = new(tmp_path, "fail", 7)
    with dss.open(p) as f:
        f.write_timeseries("/A/B/FLOW//1HOUR/S/", [1.0], start="01JAN2021 01:00",
                           units="CFS", data_type="INST-VAL")
        with pytest.raises(Exception):
            f.write_timeseries("not a pathname", [1.0], start="01JAN2021 01:00",
                               units="CFS", data_type="INST-VAL")
        assert not f.closed
        assert f.read_timeseries("/A/B/FLOW//1HOUR/S/").iloc[:, 0].tolist() == [1.0]


P51 = __import__("pathlib").Path(__file__).resolve().parents[2] / "fixtures" / "public" / "vortex_p51"


@pytest.mark.parametrize("v", [7, 6])
def test_grid_to_point_takes_sync(tmp_path, v):
    """Grid to point stores several series in one call: ``"end"`` flushes once, after the
    last one (plus nothing for the new file, which that flush covers)."""
    from ras_commander_fast_dss import gridded

    assert inspect.signature(gridded.grid_to_point).parameters["sync"].default == "end"
    args = (P51 / "regular_1h.dss", P51 / "polys.shp", "Name")
    frames = {}
    modes = ((None, 1), ("end", 1), (True, 1), ("none", 0), (False, 0), ("step", None))
    for k, (sync, want) in enumerate(modes):
        kw = {} if sync is None else {"sync": sync}
        out = tmp_path / f"g2p_{k}_{v}.dss"
        n = flushes(lambda: gridded.grid_to_point(*args, out, version=v, **kw))
        if want is None:
            assert n > 5, n
        else:
            assert n == want, (sync, n)
        with dss.open(out) as f:
            assert not f.catalog().attrs["anomalies"]
            frames[sync] = {
                s: f.read_arrays(s, trim_missing=False)["values"].tobytes() for s in f.pathnames()
            }
    for sync, got in frames.items():
        assert got and got == frames[None], sync
    with pytest.raises(ValueError, match="sync must be"):
        gridded.grid_to_point(*args, tmp_path / "bad.dss", sync="sometimes")
    assert not (tmp_path / "bad.dss").exists()


def test_gridded_import_flushes_once(tmp_path):
    """The importer's ``"end"`` is one flush for the whole run, not one per grid."""
    from ras_commander_fast_dss import gridded

    src = tmp_path / "src.dss"
    dss.create(src)
    for i in range(6):
        dss.write_grid(src, f"/SHG/A/PRECIP/01SEP2018:{i:02d}00/01SEP2018:{i + 1:02d}00/S/",
                       np.full((3, 4), i + 1, np.float32), **SHG)
    for k, (sync, want) in enumerate(((None, 1), ("none", 0))):
        kw = {} if sync is None else {"sync": sync}
        out = tmp_path / f"shift_{k}.dss"
        n = flushes(lambda: gridded.shift_times(src, out, shift=__import__("datetime").timedelta(hours=1), **kw))
        assert n == want, (sync, n)
        with dss.open(out) as f:
            assert len(f.catalog()) == 6
    out = tmp_path / "shift_step.dss"
    assert flushes(lambda: gridded.shift_times(src, out, shift=__import__("datetime").timedelta(hours=1), sync="step")) > 12


CHILD = textwrap.dedent(
    """
    import os
    import sys
    import numpy as np
    import pandas as pd
    import ras_commander_fast_dss as dss
    T = pd.to_datetime(["2021-02-01 00:30", "2021-02-03 07:00"])
    path, v, sync, pause = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4] == "1"
    # "session" writes through one DssFile (its held read+write handle),
    # "module" through the module-level functions (one open per write).
    session = sys.argv[5] == "session"
    if session:
        f = dss.open(path)
        W = lambda fn, *a, **k: getattr(f, fn.__name__)(*a, **k)
    else:
        W = lambda fn, *a, **k: fn(path, *a, **k)
    PD = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")
    SHG = dict(grid_type="shg", cell_size=2000.0, units="MM", data_type="PER-CUM",
               lower_left_cell=(-1000, 700))
    print(f"ready {os.getpid()}", flush=True)
    for i in range(100000):
        W(dss.write_timeseries, f"/K/R{i}/FLOW//1HOUR/S/", np.arange(48.0) + i,
          start="01JAN2021 01:00", units="CFS", data_type="INST-VAL", version=v, sync=sync)
        W(dss.write_timeseries, f"/K/I{i}/STAGE//IR-MONTH/S/", [i + 0.5, i + 1.5],
          times=T, units="FT", data_type="INST-VAL", version=v, sync=sync)
        W(dss.write_paired_data, f"/K/P{i}/STAGE-FLOW///S/", [0.0, 1.0], [i, i + 0.5],
          version=v, sync=sync, **PD)
        W(dss.write_grid, f"/SHG/K{i}/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
          np.full((3, 4), i, np.float32), version=v, sync=sync, **SHG)
        if v == 7:
            W(dss.write_profile, f"/K/L{i}/DEPTH-TEMP//1Hour/S/", [0.5, 1.5],
              np.arange(6, dtype=np.float64).reshape(3, 2) + i,
              start="10JAN2020 01:00", depth_units="m", value_units="deg C",
              data_type="INST-VAL", sync=sync)
            W(dss.write_location, f"/K/L{i}/Location Info////", x=float(i), y=30.5,
              z=10.0, sync=sync)
        if session:
            # A read through the session between writes, as an interactive caller does.
            assert f.read_paired_data(f"/K/P{i}/STAGE-FLOW///S/").values.ravel().tolist() == [i, i + 0.5]
            assert f._native._held_opens == 1
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


def run_child(tmp_path, v, sync, pause, api="module"):
    p = new(tmp_path, f"killed{sync}{int(pause)}", v)
    script = tmp_path / "child.py"
    script.write_text(CHILD)
    child = subprocess.Popen(
        [sys.executable, "-I", str(script), str(p), str(v), sync, str(int(pause)), api],
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


def verify_after_kill(p, v, completed, api="module"):
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
    after_kw = dict(start="01JAN2021 01:00", units="CFS", data_type="INST-VAL", version=v)
    if api == "session":
        # P74: the reconciling write through a DssFile's held handle.
        def repair_write():
            with dss.open(p) as s:
                return s.write_timeseries("/K/AFTER/FLOW//1HOUR/S/", [1.0], **after_kw)

        rep = after_lock(repair_write)
    else:
        rep = after_lock(lambda: dss.write_timeseries(
            p, "/K/AFTER/FLOW//1HOUR/S/", [1.0], **after_kw))
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


@pytest.mark.parametrize("api", ["module", "session"])
@pytest.mark.parametrize("sync", ["none", "end", "step"])
@pytest.mark.parametrize("v", [7, 6])
def test_completed_writes_survive_an_os_kill_between_writes(tmp_path, v, sync, api):
    """Every write that returned before the writing process was killed (``hard_kill``:
    TerminateProcess on Windows, SIGKILL elsewhere) reads back exactly, in every flush
    mode, and the next default-mode write finds nothing to reconcile."""
    p, child, lines = run_child(tmp_path, v, sync, pause=True, api=api)
    n = 5
    try:
        pid = start(lines)
        for i in range(n):
            assert lines.next() == f"done {i}"
            if i < n - 1:
                child.stdin.write("go\n")
                child.stdin.flush()
        # Paused between writes (module: no handle open; session: the held read+write
        # handle open, unlocked); killed without exiting.
        hard_kill(child, pid)
    finally:
        child.kill()
        child.wait(timeout=60)
    assert child.returncode != 0
    rep, extra = verify_after_kill(p, v, n, api)
    assert extra == 0
    assert not rep["reconciled_interrupted_write"]


@pytest.mark.parametrize("api", ["module", "session"])
@pytest.mark.parametrize("sync", ["none", "end", "step"])
@pytest.mark.parametrize("v", [7, 6])
def test_writes_survive_an_os_kill_during_a_write(tmp_path, v, sync, api):
    """The child writes without pausing; it is killed a short, varied delay after it
    reports a completed record set, so the kill lands somewhere in the following writes,
    often inside one record's transaction. Where exactly it lands is not controlled (the
    delay only spreads the kills; a loaded host shifts them), and the assertions hold
    wherever it lands:
    completed sets read back exactly, each later record is absent or exact and the
    readable ones are a prefix of the write order, and after the next write's
    reconciliation the catalog lists exactly the records that read. (The step-by-step kill points are the Rust matrix
    ``crates/dss-core/tests/p64_kill.rs``.)"""
    scale = 1.0 if sync == "step" else 0.2  # a per-step record write is several times slower
    for trial, (after, delay) in enumerate(((0, 0.0), (1, 0.004), (2, 0.015), (3, 0.04),
                                            (4, 0.1))):
        sub = tmp_path / f"t{trial}"
        sub.mkdir()
        p, child, lines = run_child(sub, v, sync, pause=False, api=api)
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
        verify_after_kill(p, v, after + 1, api)


def test_feature_flag():
    assert "write_sync_policy" in dss.FEATURES


# -- repair() takes the same flush policies ------------------------------------------------


def test_repair_sync_signature():
    p = inspect.signature(dss.repair).parameters["sync"]
    assert p.default == "end"
    assert p.kind is inspect.Parameter.KEYWORD_ONLY


@pytest.mark.parametrize("bad", ["false", "always", 1, None])
def test_repair_rejects_bad_sync(tmp_path, bad):
    src = new(tmp_path, "rsrc", 7)
    with pytest.raises((TypeError, ValueError)):
        dss.repair(src, tmp_path / "rdest.dss", sync=bad)
    assert not (tmp_path / "rdest.dss").exists()


@pytest.mark.parametrize(
    "sync, want",
    [("default", 1), ("end", 1), (True, 1), ("step", 1), ("none", 0), (False, 0)],
)
def test_repair_flushes_once_or_never(tmp_path, sync, want):
    """``dest`` is unpublished until verified, so ``end`` and ``step`` both flush it once
    and ``none`` never. Before the fix: no ``sync`` argument, two uncounted flushes."""
    src = new(tmp_path, "rsrc", 7)
    for i in range(3):
        dss.write_timeseries(src, f"/R/REP{i}/FLOW//1HOUR/S/", [1.0 + i, 2.0], start="01JAN2021 0100",
                             units="CFS", data_type="INST-VAL", sync=False)
    dest = tmp_path / "rdest.dss"
    kw = {} if sync == "default" else {"sync": sync}
    before = _native._flush_count()
    r = dss.repair(src, dest, **kw)
    assert _native._flush_count() - before == want
    assert r["complete"] and r["records_copied"] == 3
    with dss.open(dest) as f:
        got = sorted(p.upper() for p in f.pathnames())
    assert got == sorted(f"/R/REP{i}/FLOW/01JAN2021/1HOUR/S/" for i in range(3))

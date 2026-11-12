"""P72: the gridded import uses every core by default, and the file it writes
does not depend on the worker count. Public synthetic inputs only.

The writers stamp each file and record with the time of writing, so two runs are compared
with that clock fixed through the hidden test hook ``_native._set_write_clock``.
"""

from __future__ import annotations

import inspect

import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import _native, gridded

from test_gridded import APCP, BOX, PARTS, inputs

# 2026-11-10 00:00:00 UTC, an arbitrary fixed stamp.
CLOCK_MS = 1_794_268_800_000


@pytest.fixture
def fixed_clock():
    _native._set_write_clock(CLOCK_MS)
    try:
        yield
    finally:
        _native._set_write_clock(None)


def test_threads_default_is_all_cores():
    for fn in (gridded.import_gridded, gridded.import_arrays, gridded.import_zarr):
        assert inspect.signature(fn).parameters["threads"].default is None, fn.__name__


@pytest.mark.parametrize("bad", [0, -1, True])
def test_threads_must_be_at_least_one(tmp_path, bad):
    with pytest.raises(ValueError, match="threads"):
        gridded.import_gridded(
            inputs("apcp_a.grib2"), tmp_path / "o.dss", variable=APCP, parts=PARTS, threads=bad, **BOX
        )


@pytest.mark.parametrize("version", [7, 6])
def test_default_threads_write_the_same_bytes_as_one(tmp_path, fixed_clock, version):
    files = {}
    for name, kw in (("one", {"threads": 1}), ("default", {}), ("eight", {"threads": 8})):
        out = tmp_path / f"{name}.dss"
        gridded.import_gridded(
            inputs("apcp_a.grib2", "apcp_b.grib2"), out, variable=APCP, parts=PARTS, version=version,
            **kw, **BOX,
        )
        files[name] = out.read_bytes()
    assert files["default"] == files["one"]
    assert files["eight"] == files["one"]


def test_the_write_clock_is_restored(tmp_path):
    # Without the hook two runs a second apart differ only by the stamp; with it they do not.
    _native._set_write_clock(CLOCK_MS)
    try:
        a = tmp_path / "a.dss"
        dss.create(a)
    finally:
        _native._set_write_clock(None)
    b = tmp_path / "b.dss"
    dss.create(b)
    assert a.read_bytes() != b.read_bytes()

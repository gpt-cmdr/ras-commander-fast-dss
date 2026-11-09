"""P63: grid and profile reads adopt the fresh native arrays instead of copying them.

``Grid._from_native`` and ``Profile._from_native`` take ``owned=True`` only from the
internal readers, which hold the only reference to a fresh native dict. Every result
must equal the copying path (``owned=False``, the earlier behaviour) bit for bit: dtype,
shape, C-contiguity, raw bits (signed zero, NaN payloads, HEC missing markers), missing
-> NaN and orientation. Results must survive ``close()`` and never share memory with
another result. Inputs that are not fresh writeable C-contiguous arrays are still copied.
"""

import dataclasses

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import GRID_UNDEFINED, Grid, Profile

from conftest import FIXTURES, fixture_path

GRID_FILES = sorted(
    p.name
    for p in FIXTURES.glob("*.dss")
    if p.name.startswith(("real_grid", "synthetic_grid", "synthetic_albers", "p24_hec_grid"))
)
PROFILE_FILES = ["synthetic_profile7.dss", "synthetic_profile_f32_7.dss"]
MODES = [(m, o) for m in ("nan", "raw") for o in ("native", "north_up")]


def _grid_paths(f):
    c = f.catalog()
    return c.loc[(c.record_type >= 400) & (c.record_type < 500), "pathname"].tolist()


def _same_array(a: np.ndarray, b: np.ndarray):
    assert a.dtype == b.dtype and a.shape == b.shape
    assert a.flags.c_contiguous and b.flags.c_contiguous
    assert a.tobytes() == b.tobytes()


def _same_value(x, y, where):
    assert type(x) is type(y), where
    if isinstance(x, np.ndarray):
        assert x.dtype == y.dtype and x.shape == y.shape, where
        assert x.tobytes() == y.tobytes(), where
    elif isinstance(x, dict):
        assert list(x) == list(y), where
        for k in x:
            _same_value(x[k], y[k], f"{where}.{k}")
    elif isinstance(x, (list, tuple)):
        assert len(x) == len(y), where
        for i, (u, v) in enumerate(zip(x, y)):
            _same_value(u, v, f"{where}[{i}]")
    elif isinstance(x, float) and np.isnan(x):
        assert np.isnan(y), where
    else:
        assert x == y, where


def _same_record(a, b):
    for fld in dataclasses.fields(a):
        _same_value(getattr(a, fld.name), getattr(b, fld.name), fld.name)


def test_grid_fixtures_present():
    assert len(GRID_FILES) >= 6, GRID_FILES


@pytest.mark.parametrize("name", GRID_FILES)
def test_grid_reads_equal_the_copying_path(name):
    """Every grid of every public grid fixture, all missing/order modes."""
    path = fixture_path(name)
    with dss.open(path) as f:
        paths = _grid_paths(f)
        assert paths
        for p in paths:
            for missing, order in MODES:
                new = f.read_grid(p, missing=missing, order=order)
                ref = Grid._from_native(
                    f._native.read_grid(p, None, False),
                    missing=missing,
                    order=order,
                    dss_file=str(f._resolved_path),
                )
                _same_array(new.data, ref.data)
                assert new.data.dtype == np.float32
                _same_record(new, ref)


def _native_grid(f, p, cells):
    d = dict(f._native.read_grid(p, None, False))
    d["data"] = cells
    return d


SPECIAL = np.array(
    [0.0, -0.0, 1.5, np.nan, GRID_UNDEFINED, -901.0, -902.0, 3.0e38, 1.0e-45],
    dtype=np.float32,
)


def _special_cells(ny, nx):
    bits = np.resize(SPECIAL, ny * nx).view(np.uint32).copy()
    bits[3::7] = 0x7FC0_1234  # quiet NaN with a payload
    bits[5::11] = 0xFF80_0001  # negative signalling-NaN bit pattern
    return bits.view(np.float32).reshape(ny, nx)


@pytest.mark.parametrize("missing,order", MODES)
def test_grid_special_bits_owned_equals_copied(missing, order):
    with dss.open(fixture_path("synthetic_grid7.dss")) as f:
        p = _grid_paths(f)[0]
        g = f.read_grid(p, missing="raw")
        ny, nx = g.data.shape
        cells = _special_cells(ny, nx)
        expect = cells.copy()
        ref = Grid._from_native(
            _native_grid(f, p, cells.copy()), missing=missing, order=order, dss_file=""
        )
        owned_in = cells.copy()
        new = Grid._from_native(
            _native_grid(f, p, owned_in), missing=missing, order=order, dss_file="", owned=True
        )
    _same_array(new.data, ref.data)
    if missing == "raw":
        want = expect[::-1] if order == "north_up" else expect
        assert new.data.tobytes() == np.ascontiguousarray(want).tobytes()
    else:
        assert np.isnan(new.data).sum() > 0
        assert not np.isin(new.data, [GRID_UNDEFINED, -901.0, -902.0]).any()
    # The native array itself is adopted in native order and replaced by one new
    # array when flipped.
    if order == "native":
        assert new.data is owned_in
    else:
        assert not np.shares_memory(new.data, owned_in)


def test_grid_input_that_is_not_fresh_is_copied():
    with dss.open(fixture_path("synthetic_grid7.dss")) as f:
        p = _grid_paths(f)[0]
        base = f.read_grid(p, missing="raw").data
    variants = {
        "read_only": base.copy(),
        "strided": np.repeat(base, 2, axis=1)[:, ::2],
        "float64": base.astype(np.float64),
        "byteswapped": base.astype(base.dtype.newbyteorder()),
    }
    variants["read_only"].flags.writeable = False
    with dss.open(fixture_path("synthetic_grid7.dss")) as f:
        for label, cells in variants.items():
            before = cells.copy()
            g = Grid._from_native(
                _native_grid(f, p, cells), missing="nan", order="native", dss_file="", owned=True
            )
            assert not np.shares_memory(g.data, cells), label
            assert cells.tobytes() == before.tobytes(), label
            assert g.data.dtype == np.float32 and g.data.flags.c_contiguous, label


def test_grid_results_survive_close_and_do_not_alias():
    path = fixture_path("real_grid7.dss")
    f = dss.open(path)
    p = _grid_paths(f)[0]
    a = f.read_grid(p)
    b = f.read_grid(p)
    up = f.read_grid(p, order="north_up")
    raw = f.read_grid(p, missing="raw")
    keep = [x.data.copy() for x in (a, b, up, raw)]
    f.close()
    arrays = [a.data, b.data, up.data, raw.data]
    for i, x in enumerate(arrays):
        for y in arrays[i + 1 :]:
            assert not np.shares_memory(x, y)
    a.data[...] = 123.0
    for x, k in zip(arrays[1:], keep[1:]):
        assert x.tobytes() == k.tobytes()
    assert up.data.tobytes() == np.ascontiguousarray(keep[0][::-1]).tobytes()
    assert up.data.flags.c_contiguous


@pytest.mark.parametrize("name", PROFILE_FILES)
@pytest.mark.parametrize("raw", [False, True])
def test_profile_reads_equal_the_copying_path(name, raw):
    with dss.open(fixture_path(name)) as f:
        c = f.catalog()
        recs = c.loc[c.record_type.isin([102, 107]), "pathname"].tolist()
        assert recs
        seen = set()
        for rec in recs:
            parts = rec.split("/")
            parts[4] = ""
            ds = "/".join(parts)
            for p, read, native in (
                (rec, lambda: f.read_profile_record(rec, raw=raw), lambda: f._native.read_profile_record(rec)),
                (ds, lambda: f.read_profile(ds, raw=raw), lambda: f._native.read_profile(ds, None, None, True)),
            ):
                if p in seen:
                    continue
                seen.add(p)
                new = read()
                ref = Profile._from_native(native(), pathname=p, raw=raw, dss_file=str(f._resolved_path))
                _same_array(new.depths, ref.depths)
                _same_array(new.values, ref.values)
                assert new.values.dtype == new.stored_dtype
                _same_record(new, ref)


def test_profile_one_sided_window_still_copies_and_results_do_not_alias():
    with dss.open(fixture_path("synthetic_profile7.dss")) as f:
        c = f.catalog()
        rec = c.loc[c.record_type.isin([102, 107]), "pathname"].iloc[0]
        parts = rec.split("/")
        parts[4] = ""
        ds = "/".join(parts)
        whole = f.read_profile(ds)
        start = whole.times[1]
        cut = f.read_profile(ds, start=start)
        a = f.read_profile(ds)
        b = f.read_profile(ds)
    assert cut.values.flags.owndata and cut.depths.flags.owndata
    assert len(cut.times) == len(whole.times) - 1
    np.testing.assert_array_equal(cut.values, whole.values[1:])
    for x, y in ((a.values, b.values), (a.depths, b.depths), (a.values, cut.values)):
        assert not np.shares_memory(x, y)
    keep = b.values.copy()
    a.values[...] = 0
    assert b.values.tobytes() == keep.tobytes()

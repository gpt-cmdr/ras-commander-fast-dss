"""P30: the default trim keeps a stored NaN, as HEC's ``ztsTrim`` does.

``heclib_c/src/Internal/ztsTrim.c`` (lines 83-170, hec-dss fork tag ``7-IU-15``) trims
leading and trailing values for which ``zisMissingFloat`` / ``zisMissingDouble`` is true:
-901, -902 and -FLT_MAX (``Utilities/zmissingFlag.c`` lines 115-145). A stored NaN is a
value. Regular data only (``Public/ztsRetrieve.c`` lines 618-626).

Fixtures: ``synthetic_no_data6.dss`` (P29: F = -901 with one -FLT_MAX, Z = -901 with one
NaN) and ``synthetic_nan_trim6.dss``, both generated and byte-checked by the Rust
tests (``dss6_no_data.rs``, ``dss6_nan_trim.rs``).
"""

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from conftest import fixture_path

P29 = "synthetic_no_data6.dss"
P30 = "synthetic_nan_trim6.dss"
F = "/P29/GAGE F/FLOW//1DAY/OBS/"  # -901 except one -FLT_MAX (slot 100)
Z = "/P29/GAGE Z/FLOW//1DAY/OBS/"  # -901 except one NaN (slot 200)
A = "/P30/GAGE A/FLOW//1DAY/OBS/"  # -902, -FLT_MAX, -901, NaN, data, NaN, -FLT_MAX, -902, -901
D = "/P30/GAGE D/FLOW//1DAY/OBS/"  # the same, REAL*8
B = "/P30/GAGE B/FLOW//1DAY/OBS/"  # 2000: -901 x10, NaN, data, NaN; 2001 all -FLT_MAX
N = "/P30/GAGE N/FLOW//1DAY/OBS/"  # all NaN


@pytest.fixture
def f29():
    with dss.open(fixture_path(P29)) as fh:
        yield fh


@pytest.fixture
def f30():
    with dss.open(fixture_path(P30)) as fh:
        yield fh


@pytest.mark.parametrize("nd", ["return", "refuse"])
def test_z_whole_read_returns_the_nan(f29, nd):
    d = f29.read_arrays(Z, no_data=nd)
    assert len(d["values"]) == 1 and np.isnan(d["values"][0])
    assert d["times"][0] == dss.to_hec_seconds("2000-07-20")
    df = f29.read_timeseries(Z, no_data=nd)
    assert len(df) == 1 and np.isnan(df["value"].iloc[0])
    raw = f29.read_timeseries(Z, raw=True)
    assert len(raw) == 1 and np.isnan(raw["value"].iloc[0])


def test_f_block_unchanged(f29):
    # -FLT_MAX is HEC's missing flag: trimmed as before.
    assert len(f29.read_arrays(F)["values"]) == 0
    with pytest.raises(dss.DssPathnameNotFoundError):
        f29.read_timeseries(F)
    assert len(f29.read_arrays(F, trim_missing=False)["values"]) == 366


@pytest.mark.parametrize("path, width", [(A, np.float32), (D, np.float64)])
def test_leading_and_trailing_nan_kept(f30, path, width):
    d = f30.read_arrays(path)
    v = d["values"]
    assert v.dtype == width
    assert len(v) == 360
    assert np.isnan(v[0]) and np.isnan(v[-1])
    assert v[1] == 4.0 and v[-2] == 361.0
    assert d["times"][0] == dss.to_hec_seconds("2000-01-05")
    assert d["times"][-1] == dss.to_hec_seconds("2000-12-29")
    # Untrimmed: every slot, markers kept in raw form.
    u = f30.read_arrays(path, trim_missing=False)
    assert len(u["values"]) == 366
    assert u["values"][0] == -902.0 and u["values"][1] == width(-np.finfo(np.float32).max)
    # raw=True keeps the NaN rows, not markers, at both ends.
    df = f30.read_timeseries(path, raw=True)
    assert len(df) == 360 and df["value"].iloc[[0, -1]].isna().all()


@pytest.mark.parametrize("path", [A, D, B, Z])
def test_one_and_two_sided_windows_agree(f30, f29, path):
    f = f29 if path == Z else f30
    whole = f.read_arrays(path)
    s, e = "1999-01-01", "2002-01-01"
    two = f.read_arrays(path, s, e)
    start_only = f.read_arrays(path, s, None)
    end_only = f.read_arrays(path, None, e)
    for d in (two, start_only, end_only):
        assert np.array_equal(d["times"], whole["times"])
        assert np.array_equal(d["values"], whole["values"], equal_nan=True)


def test_trailing_nan_kept_across_a_trimmed_block(f30):
    d = f30.read_arrays(B)
    v = d["values"]
    assert len(v) == 356 and np.isnan(v[0]) and np.isnan(v[-1])
    assert d["times"][-1] == dss.to_hec_seconds("2001-01-01")


def test_all_nan_block_kept(f30):
    for nd in ("return", "refuse"):
        d = f30.read_arrays(N, no_data=nd)
        assert len(d["values"]) == 366 and np.isnan(d["values"]).all()

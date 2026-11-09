"""P39: profile and location writers through the Python API. Synthetic data only."""

import hashlib
import struct
from pathlib import Path

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssPathnameNotFoundError, DssUnsupportedRecordError

DEPTHS = [0.5, 1.5, 3.0]
PATH = "/P39/LAKE/DEPTH-TEMP//1Hour/PY/"
KW = dict(depth_units="m", value_units="deg C", data_type="INST-VAL")


def sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()


@pytest.fixture
def f7(tmp_path):
    p = tmp_path / "a7.dss"
    dss.create(p)
    return p


@pytest.fixture
def f6(tmp_path):
    p = tmp_path / "a6.dss"
    dss.create(p, version=6)
    return p


def matrix(n=10):
    return np.arange(n * 3, dtype=np.float64).reshape(n, 3) + 1.5


def test_features_and_exports():
    assert {"write_profile", "write_location"} <= dss.FEATURES
    assert {"write_profile", "write_location"} <= set(dss.__all__)


def test_profile_round_trip_matrix_across_blocks(f7):
    v = matrix()
    v[4, 1] = np.nan
    rep = dss.write_profile(f7, PATH, DEPTHS, v, start="31JAN2020 20:00", time_zone="UTC", **KW)
    assert len(rep["blocks"]) == 2
    with dss.open(f7) as f:
        got = f.read_profile(PATH)
    assert got.record_type == 107
    assert got.shape == (10, 3)
    assert got.depth_units == "m" and got.value_units == "deg C" and got.time_zone == "UTC"
    assert np.array_equal(got.depths, DEPTHS)
    assert np.array_equal(got.values, v, equal_nan=True)
    assert got.times[0] == np.datetime64("2020-01-31T20:00:00")
    raw = dss.read_profile(f7, PATH, raw=True)
    assert raw.values[4, 1] == -np.finfo(np.float32).max


def test_profile_float_width_kept(f7):
    v = matrix(3).astype(np.float32)
    rep = dss.write_profile(f7, PATH, np.array(DEPTHS, np.float32), v, start="10JAN2020 01:00", **KW)
    assert rep["blocks"][0]["record_type"] == 102
    got = dss.read_profile(f7, PATH)
    assert got.values.dtype == np.float32
    assert np.array_equal(got.values, v)
    # value_type="auto" with float64 input refuses to change the stored width.
    before = sha(f7)
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_profile(f7, PATH, DEPTHS, matrix(3), start="10JAN2020 01:00", **KW)
    assert sha(f7) == before


def test_profile_quality(f7):
    q = [1, 2, 3]
    dss.write_profile(f7, PATH, DEPTHS, matrix(3), start="10JAN2020 01:00", quality=q, **KW)
    got = dss.read_profile(f7, PATH)
    assert list(got.quality[:3]) == q or q[0] in got.quality


@pytest.mark.parametrize(
    "depths,values,err",
    [
        ([], matrix(2), ValueError),
        (DEPTHS, matrix(2).ravel(), ValueError),  # never flattened or reshaped
        (DEPTHS, matrix(2)[None, ...], ValueError),
        (DEPTHS, matrix(2).T, ValueError),
        (DEPTHS, np.empty((0, 3)), ValueError),
        ([0.5, np.nan, 3.0], matrix(2), ValueError),
        (["a", "b", "c"], matrix(2), ValueError),
    ],
)
def test_profile_shape_refusals(f7, depths, values, err):
    before = sha(f7)
    with pytest.raises(err):
        dss.write_profile(f7, PATH, depths, values, start="10JAN2020 01:00", **KW)
    assert sha(f7) == before


def test_profile_refused_on_dss6(f6):
    before = sha(f6)
    with pytest.raises(DssUnsupportedRecordError, match="DSS6"):
        dss.write_profile(f6, PATH, DEPTHS, matrix(2), start="10JAN2020 01:00", **KW)
    assert sha(f6) == before


def test_other_depths_refused(f7):
    dss.write_profile(f7, PATH, DEPTHS, matrix(3), start="10JAN2020 01:00", **KW)
    before = sha(f7)
    with pytest.raises(DssUnsupportedRecordError, match="depths"):
        dss.write_profile(f7, PATH, [0.5, 1.5, 9.0], matrix(3), start="10JAN2020 02:00", **KW)
    with pytest.raises(DssUnsupportedRecordError, match="depths"):
        dss.write_profile(f7, PATH, [0.5, 1.5, 9.0], matrix(3), start="10MAR2020 02:00", **KW)
    assert sha(f7) == before


def test_location_dss7_full_and_partial(f7):
    dss.write_location(
        f7,
        PATH,
        x=-97.25,
        y=30.5,
        z=612.5,
        coordinate_system=1,
        horizontal_units=3,
        horizontal_datum=1,
        vertical_units=1,
        vertical_datum=2,
        time_zone="America/Chicago",
        supplemental="Operator:Synth;VerticalDatum:NAVD88;",
    )
    with dss.open(f7) as f:
        loc = f.read_location("/P39/LAKE/Location Info////")
        assert loc.x == -97.25 and loc.vertical_datum_code == 2
        assert loc.time_zone == "America/Chicago"
        assert dict(loc.supplemental_items)["Operator"] == "Synth"
        # A datum in the text never fills the typed field.
        assert loc.vertical_datum_code == 2 and loc.vertical_datum_named
        assert loc.supplemental_vertical_datum == "NAVD88"
        # The handle-level call reopens and sees the update; other fields stay.
        f.write_location(PATH, z=1000.0)
        loc = f.read_associated_location(PATH)
        assert loc.z == 1000.0 and loc.x == -97.25
        assert loc.supplemental.startswith("Operator")
    # Replacing the data leaves the location record alone.
    dss.write_profile(f7, PATH, DEPTHS, matrix(2), start="10JAN2020 01:00", **KW)
    dss.write_profile(f7, PATH, DEPTHS, matrix(2) * 2, start="10JAN2020 01:00", **KW)
    with dss.open(f7) as f:
        assert f.read_associated_location(PATH).z == 1000.0
    # keep_existing=False clears what is not given.
    dss.write_location(f7, PATH, x=1.0, keep_existing=False)
    with dss.open(f7) as f:
        loc = f.read_associated_location(PATH)
        assert loc.z == 0.0 and loc.supplemental is None and not loc.time_zone


def test_location_nothing_to_store(f7):
    before = sha(f7)
    with pytest.raises(ValueError, match="nothing to store"):
        dss.write_location(f7, PATH, coordinate_system=1)
    with pytest.raises(ValueError, match="time_zone"):
        dss.write_location(f7, PATH, x=1.0, time_zone="X" * 73)
    with pytest.raises(ValueError, match="NUL"):
        dss.write_location(f7, PATH, x=1.0, supplemental="a\0b")
    assert sha(f7) == before


def test_location_dss6(f6):
    p = "/P39/G/FLOW//1HOUR/PY/"
    dss.write_timeseries(f6, p, np.arange(8, dtype=np.float32), start="31JAN2020 22:00", units="CFS", data_type="INST-VAL", version=6)
    dss.write_timeseries(f6, "/P39/H/FLOW//1HOUR/PY/", np.arange(2, dtype=np.float32), start="10JAN2020 01:00", units="CFS", data_type="INST-VAL", version=6)
    before = sha(f6)
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_location(f6, "/P39/NONE/FLOW//1HOUR/PY/", x=1.0)
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_location(f6, p, x=1.0, time_zone="UTC")
    with pytest.raises(ValueError, match="non-zero"):
        dss.write_location(f6, p, coordinate_system=1)
    assert sha(f6) == before
    rep = dss.write_location(
        f6, p, x=1.5, y=2.5, z=3.5, coordinate_system=5, vertical_datum=3, supplemental="Key:Value;"
    )
    assert len(rep["blocks"]) == 2 and rep["version"] == 6
    with dss.open(f6) as f:
        loc = f.read_associated_location(p)
        assert (loc.x, loc.y, loc.z) == (1.5, 2.5, 3.5)
        assert loc.supplemental == "Key:Value;" and loc.supplemental_source == "dss6_user_header"
        assert f.read_associated_location("/P39/H/FLOW//1HOUR/PY/") is None
        ts = f.read_timeseries(p)
    assert np.array_equal(ts["value"].to_numpy() if "value" in ts else ts.iloc[:, 0].to_numpy(), np.arange(8))
    # A time-series rewrite keeps the location, text included.
    dss.write_timeseries(f6, p, np.array([9.0], np.float32), start="31JAN2020 23:00", units="CFS", data_type="INST-VAL", version=6)
    with dss.open(f6) as f:
        loc = f.read_associated_location(p)
        assert loc.x == 1.5 and loc.supplemental == "Key:Value;"
    # A partial update through the handle.
    with dss.open(f6) as f:
        f.write_location(p, z=9.0)
        loc = f.read_associated_location(p)
        assert loc.z == 9.0 and loc.supplemental == "Key:Value;"


def test_missing_file_and_type_checks(tmp_path, f7):
    with pytest.raises(OSError):
        dss.write_location(tmp_path / "none.dss", PATH, x=1.0)
    with pytest.raises(TypeError):
        dss.write_location(f7, 3, x=1.0)
    with pytest.raises(TypeError):
        dss.write_location(f7, PATH, x=1.0, supplemental=3)
    with pytest.raises(DssPathnameNotFoundError):
        dss.read_profile(f7, PATH)


def test_monolith_written_dss6_location_reads_and_replays(tmp_path):
    """HEC write, native read, and the native writers giving the same location."""
    root = Path(__file__).resolve().parents[2] / "fixtures" / "public"
    hec = root / "p39_hec_loc6.dss"
    native = tmp_path / "n.dss"
    dss.create(native, version=6)
    lines = (root / "p39_hec_loc6_spec.tsv").read_text().splitlines()
    bits = lambda h: struct.unpack("<d", bytes.fromhex(h)[::-1])[0]  # noqa: E731
    checked = 0
    for t, l in zip(lines[::2], lines[1::2]):
        t, l = t.split("\t"), l.split("\t")
        kind = np.float32 if t[1] == "float" else np.float64
        fmt = "<f" if t[1] == "float" else "<d"
        vals = np.array([struct.unpack(fmt, bytes.fromhex(h)[::-1])[0] for h in t[4].split(",")], kind)
        start = np.datetime64("1899-12-31") + np.timedelta64(int(t[3]), "s")
        dss.write_timeseries(native, t[2], vals, start=start, units=t[5], data_type=t[6], version=6)
        sup = None if l[12] == "~" else bytes.fromhex(l[12])
        dss.write_location(native, l[1], x=bits(l[2]), y=bits(l[3]), z=bits(l[4]),
                           coordinate_system=int(l[5]), coordinate_id=int(l[6]),
                           horizontal_units=int(l[7]), horizontal_datum=int(l[8]),
                           vertical_units=int(l[9]), vertical_datum=int(l[10]), supplemental=sup)
        with dss.open(hec) as fh, dss.open(native) as fn:
            a, b = fh.read_associated_location(l[1]), fn.read_associated_location(l[1])
        assert (a.x, a.y, a.z) == (bits(l[2]), bits(l[3]), bits(l[4]))
        assert (a.x, a.y, a.z, a.supplemental) == (b.x, b.y, b.z, b.supplemental)
        assert a.coordinate_id == b.coordinate_id and a.vertical_datum_code == b.vertical_datum_code
        checked += 1
    assert checked == 3

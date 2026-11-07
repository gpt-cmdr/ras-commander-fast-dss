"""P24: grid writer (DSS7 and DSS6), Python API.

Synthetic data only; every file is created in ``tmp_path`` or is a copy of a public
fixture. The HEC-written fixture ``p24_hec_grid{6,7}.dss`` (HEC Monolith 3.3.27) is read
natively and compared with the Monolith reference dump committed next to it; the other
oracle comparisons are local-only (evidence/P24/REPORT.md).
"""

from __future__ import annotations

import datetime as dt
import hashlib
import shutil
import struct

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssUnsupportedRecordError

from conftest import fixture_path

UNDEF = np.float32(-3.4028235e38)
SHG_KW = dict(grid_type="shg", cell_size=2000.0, units="MM", data_type="PER-CUM")


def new(tmp_path, v):
    p = tmp_path / f"g{v}.dss"
    dss.create(p, version=v)
    return p


def cells(ny=3, nx=4, seed=0):
    r = np.random.default_rng(seed)
    a = np.round(r.random((ny, nx)) * 8, 2).astype(np.float32)
    a[r.random((ny, nx)) < 0.3] = 0.0
    return a


# ---------------------------------------------------------------------------------
# Round trips
# ---------------------------------------------------------------------------------


@pytest.mark.parametrize("v", [7, 6])
def test_round_trip_and_outcomes(tmp_path, v):
    p = new(tmp_path, v)
    a = cells()
    a[0, 1] = np.nan
    path = "/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/"
    r = dss.write_grid(p, path, a, lower_left_cell=(-1000, 700), version=v, **SHG_KW)
    assert r["outcome"] == "added" and r["record_type"] == 420
    assert r["pathname"] == (path.upper() if v == 6 else path)
    with dss.open(p) as f:
        g = f.read_grid(path)
        raw = f.read_grid(path, missing="raw")
    assert g.data.shape == (3, 4)
    assert np.isnan(g.data[0, 1])
    assert raw.data[0, 1] == UNDEF
    np.testing.assert_array_equal(np.nan_to_num(g.data, nan=-1), np.nan_to_num(a, nan=-1))
    assert (g.lower_left_cell_x, g.lower_left_cell_y, g.cell_size) == (-1000, 700, 2000.0)
    assert g.srs_definition == dss.SHG_WKT
    assert g.data_type == 1 and g.data_units == "MM"
    assert g.start_time == np.datetime64("2018-09-01T01:00") and g.end_time == np.datetime64("2018-09-01T02:00")
    st = r["statistics"]
    assert st["max"] == pytest.approx(float(np.nanmax(a)))
    assert (g.min_value, g.max_value) == (st["min"], st["max"])
    # Equal rewrite writes nothing; a changed one replaces.
    before = p.read_bytes()
    assert dss.write_grid(p, path, a, lower_left_cell=(-1000, 700), version=v, **SHG_KW)["outcome"] == "unchanged"
    assert p.read_bytes() == before
    b = a + 1
    assert dss.write_grid(p, path, b, lower_left_cell=(-1000, 700), version=v, **SHG_KW)["outcome"] == "replaced"
    with dss.open(p) as f:
        np.testing.assert_array_equal(np.nan_to_num(f.read_grid(path).data, nan=-1), np.nan_to_num(b, nan=-1))


@pytest.mark.parametrize("v", [7, 6])
def test_start_end_north_up_and_origin(tmp_path, v):
    p = new(tmp_path, v)
    north_up = np.arange(12, dtype=np.float32).reshape(3, 4)  # row 0 = north
    r = dss.write_grid(
        p, "/SHG/P24/PRECIP///T/", north_up, north_up=True,
        start=dt.datetime(2018, 9, 1, 23), end=dt.datetime(2018, 9, 2, 0),
        origin=(-2000000.0, 1400000.0), version=v, **SHG_KW,
    )
    # HEC's grid convention: a midnight end is the previous day's 2400.
    assert r["pathname"].upper() == "/SHG/P24/PRECIP/01SEP2018:2300/01SEP2018:2400/T/"
    with dss.open(p) as f:
        g = f.read_grid(r["pathname"], order="north_up")
        n = f.read_grid(r["pathname"])
    np.testing.assert_array_equal(g.data, north_up)
    np.testing.assert_array_equal(n.data[0], north_up[-1])  # stored row 0 is the south row
    assert (n.lower_left_cell_x, n.lower_left_cell_y) == (-1000, 700)
    # The extent's lower-left corner is the origin given.
    assert n.lower_left_cell_x * 2000.0 == -2000000.0


@pytest.mark.parametrize("v", [7, 6])
def test_specified_hrap_albers_and_time_invariant(tmp_path, v):
    p = new(tmp_path, v)
    wkt = ('PROJCS["NAD_1983_UTM_Zone_17N",GEOGCS["GCS_North_American_1983",'
           'DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],'
           'PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],'
           'PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],'
           'PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-81.0],'
           'PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]')
    t = cells(seed=3) - 4
    t[1, 1] = -9999.0
    dss.write_grid(p, "/UTM17N/P24/TEMPERATURE/01JAN2020:0600//T/", t, grid_type="specified",
                   srs_definition=wkt, srs_name="UTM17N", null_value=-9999.0, cell_size=1000.0,
                   lower_left_cell=(250, 4400), cell_zero=(500.0, -250.0), units="DEG C",
                   data_type="INST-VAL", version=v)
    dss.write_grid(p, "/HRAP/P24/PRECIP/18JAN2019:0600/18JAN2019:0700/T/", cells(seed=4),
                   grid_type="hrap", data_source="STAGE3", cell_size=4762.5,
                   lower_left_cell=(350, 120), units="MM", data_type="PER-CUM", version=v)
    dss.write_grid(p, "/ALB/P24/SWE/03SEP2018:1200//T/", cells(seed=5), grid_type="albers",
                   albers=dict(datum="NAD27", units="FEET", first_standard_parallel=33.0,
                               second_standard_parallel=45.0, central_meridian=-89.5,
                               latitude_of_origin=23.0, false_easting=1000.0, false_northing=-250.0),
                   cell_size=500.0, lower_left_cell=(10, 20), cell_zero=(15.0, -30.0),
                   units="IN", data_type="INST-VAL", version=v)
    dss.write_grid(p, "/SHG/P24/ELEVATION///T/", cells(seed=6), time_stamped=False,
                   grid_type="shg", cell_size=2000.0, lower_left_cell=(0, 0), units="M",
                   data_type="INST-VAL", version=v)
    up = (lambda s: s.upper()) if v == 6 else (lambda s: s)
    with dss.open(p) as f:
        s = f.read_grid(up("/UTM17N/P24/TEMPERATURE/01JAN2020:0600//T/"))
        h = f.read_grid(up("/HRAP/P24/PRECIP/18JAN2019:0600/18JAN2019:0700/T/"))
        a = f.read_grid(up("/ALB/P24/SWE/03SEP2018:1200//T/"))
        e = f.read_grid(up("/SHG/P24/ELEVATION///T/"))
    assert s.srs_definition == wkt and s.null_value == -9999.0 and np.isnan(s.data[1, 1])
    assert (s.x_coord_of_grid_cell_zero, s.y_coord_of_grid_cell_zero) == (500.0, -250.0)
    assert h.grid_type == "hrap" and h.record_type == 410
    assert a.metadata["albers"]["datum"] == 1 and a.metadata["albers"]["central_meridian"] == -89.5
    assert a.srs_definition == ""  # Monolith reports "" for a non-SHG Albers grid
    # P60: HEC writes a time-invariant SHG grid as 420 (DSS6 and DSS7); Monolith
    # cannot read DSS7 421.
    assert e.record_type == 420 and e.start_time is None


def test_precip_2_byte_dss6(tmp_path):
    p = new(tmp_path, 6)
    a = cells(seed=7)
    a[0, 0] = np.nan
    path = "/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/"
    dss.write_grid(p, path, a, lower_left_cell=(0, 0), compression="precip_2_byte", version=6, **SHG_KW)
    with dss.open(p) as f:
        g = f.read_grid(path)
    assert g.metadata["compression_method"] == 101001
    assert g.metadata["compression_scale_factor"] == 100.0
    assert np.isnan(g.data[0, 0])
    np.testing.assert_allclose(g.data[1:], a[1:], atol=0.005 + 1e-6)
    # Monolith's automatic scale would store the maximum as missing here: refused.
    big = np.array([[0.0, 870.71]], dtype=np.float32)
    with pytest.raises(ValueError, match="PRECIP_2_BYTE range"):
        dss.write_grid(p, "/SHG/P24/PRECIP/01SEP2018:0200/01SEP2018:0300/T/", big,
                       lower_left_cell=(0, 0), compression="precip_2_byte", version=6, **SHG_KW)
    dss.write_grid(p, "/SHG/P24/PRECIP/01SEP2018:0200/01SEP2018:0300/T/", big,
                   lower_left_cell=(0, 0), compression="precip_2_byte", scale_factor=10.0,
                   base=0.0, version=6, **SHG_KW)


# ---------------------------------------------------------------------------------
# Refusals (each leaves the file unchanged)
# ---------------------------------------------------------------------------------


_DROP = object()


@pytest.mark.parametrize("v", [7, 6])
def test_refusals(tmp_path, v):
    p = new(tmp_path, v)
    a = cells()
    ok = "/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/"
    dss.write_grid(p, ok, a, lower_left_cell=(0, 0), version=v, **SHG_KW)
    before = p.read_bytes()

    def call(**over):
        kw = dict(SHG_KW, lower_left_cell=(0, 0), version=v)
        path = over.pop("pathname", ok)
        data = over.pop("data", a)
        for k, x in over.items():
            if x is _DROP:
                kw.pop(k, None)
            else:
                kw[k] = x
        dss.write_grid(p, path, data, **kw)

    bad = [
        (ValueError, dict(pathname="/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/X/")),
        (ValueError, dict(pathname="/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0100/T/")),
        (ValueError, dict(pathname="/SHG/P24/PRECIP/01SEP2018:0100//T/")),
        (ValueError, dict(origin=(1000.0, 0.0), lower_left_cell=_DROP)),
        (ValueError, dict(lower_left_cell=_DROP)),
        (ValueError, dict(origin=(0.0, 0.0))),
        (ValueError, dict(data=np.zeros(5, dtype=np.float32))),
        (ValueError, dict(data=np.full((3, 4), np.inf, dtype=np.float32))),
        (ValueError, dict(data_type="PER-AVG")),
        (ValueError, dict(compression="lzw")),
        (ValueError, dict(start=dt.datetime(2018, 9, 1, 1))),
        (ValueError, dict(null_value=-9999.0)),
        (ValueError, dict(grid_type="albers")),
        (DssUnsupportedRecordError, dict(grid_type="hrap")),  # 420 -> 410
    ]
    if v == 7:
        bad.append((DssUnsupportedRecordError, dict(compression="precip_2_byte")))
    else:
        bad.append((DssUnsupportedRecordError, dict(compression="none")))
        bad.append((ValueError, dict(units="MILLIMETRES PER HOUR")))
    for exc, change in bad:
        with pytest.raises(exc):
            call(**dict(change))
        assert p.read_bytes() == before, change


def test_distinct_pathnames_stay_distinct(tmp_path):
    """HEC's HMS Atlas 14 path (P17 B2) truncated 8-slash pathnames silently; the writer
    refuses them, so pathnames that differ only after a seventh slash never collide."""
    p = new(tmp_path, 7)
    a = cells()
    names = [f"/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/A14/{q}PCT/" for q in (10, 50)]
    for n in names:
        with pytest.raises(ValueError, match="7 slashes"):
            dss.write_grid(p, n, a, lower_left_cell=(0, 0), **SHG_KW)
    good = [f"/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/A14 {q}PCT/" for q in (10, 50)]
    for i, n in enumerate(good):
        dss.write_grid(p, n, a + i, lower_left_cell=(0, 0), **SHG_KW)
    with dss.open(p) as f:
        assert sorted(x for x in f.catalog()["pathname"]) == sorted(good)
        assert f.read_grid(good[1]).data[0, 0] == a[0, 0] + 1


def test_type_change_refused(tmp_path):
    for name, v in (("sample7.dss", 7), ("sample6.dss", 6)):
        p = tmp_path / name
        shutil.copyfile(fixture_path(name), p)
        before = p.read_bytes()
        with pytest.raises(DssUnsupportedRecordError):
            dss.write_grid(p, "/MY BASIN/DEER CREEK/STAGE-FLOW///USGS/", cells(), time_stamped=False,
                           grid_type="shg", cell_size=2000.0, lower_left_cell=(0, 0), units="M",
                           data_type="INST-VAL", version=v)
        assert p.read_bytes() == before


def test_dssfile_write_grid_uses_file_version(tmp_path):
    for v in (7, 6):
        p = new(tmp_path, v)
        with dss.open(p) as f:
            r = f.write_grid("/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/", cells(),
                             lower_left_cell=(0, 0), **SHG_KW)
            assert r["outcome"] == "added"
            assert len(f.catalog()["pathname"]) == 1  # reopened after the write


# ---------------------------------------------------------------------------------
# Consumer shapes (P17 CONSUMER_INVENTORY: RasDss and HMS grid writers)
# ---------------------------------------------------------------------------------


@pytest.mark.parametrize("v", [7, 6])
def test_interval_series_like_write_grid_timeseries(tmp_path, v):
    """n frames with n+1 interval bounds (RasDss.write_grid_timeseries,
    write_precip_grid_arrays): one PER-CUM record per interval, end > start."""
    p = new(tmp_path, v)
    bounds = [dt.datetime(2018, 9, 1, 22) + dt.timedelta(hours=i) for i in range(4)]
    frames = np.stack([cells(seed=i) for i in range(3)])
    names = []
    for i in range(3):
        r = dss.write_grid(p, "/SHG/BASIN/PRECIP///NEXRAD/", frames[i], start=bounds[i],
                           end=bounds[i + 1], lower_left_cell=(-1000, 700), version=v, **SHG_KW)
        names.append(r["pathname"])
    assert names[-1].upper().endswith("/02SEP2018:0000/02SEP2018:0100/NEXRAD/")
    assert names[1].upper().endswith("/01SEP2018:2300/01SEP2018:2400/NEXRAD/")
    with dss.open(p) as f:
        for i, n in enumerate(names):
            np.testing.assert_array_equal(f.read_grid(n).data, frames[i])


def test_copy_grid_with_zero_tail_shape(tmp_path):
    """Rewrite a real grid (decoded values and metadata, as copy_grid_with_zero_tail does)
    into a new file and append an all-zero tail interval."""
    src = tmp_path / "src7.dss"
    shutil.copyfile(fixture_path("real_grid7.dss"), src)
    out = new(tmp_path, 7)
    path = "/SHG/RUSSIAN RIVER/PRECIP/01SEP2018:0100/01SEP2018:0200/NDGD-RTMA/"
    with dss.open(src) as f:
        g = f.read_grid(path, missing="raw")
    kw = dict(grid_type="shg", cell_size=g.cell_size, units=g.data_units, data_type="PER-CUM",
              lower_left_cell=(g.lower_left_cell_x, g.lower_left_cell_y), raw=True)
    dss.write_grid(out, path, g.data, **kw)
    dss.write_grid(out, path.replace("01SEP2018:0100/01SEP2018:0200", "01SEP2018:0200/01SEP2018:0300"),
                   np.where(g.data == UNDEF, UNDEF, 0).astype(np.float32), **kw)
    with dss.open(out) as f:
        back = f.read_grid(path, missing="raw")
        tail = f.read_grid(path.replace("01SEP2018:0100/01SEP2018:0200", "01SEP2018:0200/01SEP2018:0300"))
    np.testing.assert_array_equal(back.data.view(np.uint32), g.data.view(np.uint32))
    assert np.nanmax(tail.data) == 0.0
    assert back.srs_definition == g.srs_definition


# ---------------------------------------------------------------------------------
# HEC-written fixture vs the committed Monolith reference
# ---------------------------------------------------------------------------------


def _ref(v):
    rows = {}
    for line in fixture_path(f"p24_hec_grid{v}_monolith.tsv").read_text(encoding="utf-8").splitlines():
        c = line.split("\t")
        rows[c[0]] = dict(kv.split("=", 1) for kv in c[1:] if "=" in kv)
        if "ERROR" in c[1:2]:
            rows[c[0]] = None
    return rows


def _bits(x):
    return "%08x" % struct.unpack("<I", struct.pack("<f", float(x)))[0]


@pytest.mark.parametrize("v", [7, 6])
def test_hec_fixture_matches_monolith(tmp_path, v):
    src = fixture_path(f"p24_hec_grid{v}.dss")
    sha_before = hashlib.sha256(src.read_bytes()).hexdigest()
    ref = _ref(v)
    assert len(ref) == (19 if v == 6 else 18)
    n = 0
    with dss.open(src) as f:
        for path, m in ref.items():
            g = f.read_grid(path, missing="raw")
            if m is None:
                # Monolith 3.3.27 cannot read its own DSS6 specified grid with a 9-character
                # time zone (SpecifiedGridInfoFlat.unflatten; P24 REPORT section 3); the engine can.
                assert v == 6 and "/SWE/" in path and g.metadata["time_zone_id"] == "GMT-05:00"
                continue
            n += 1
            assert hashlib.sha256(g.data.astype("<f4").tobytes()).hexdigest() == m["cells_sha256"], path
            assert f"{g.lower_left_cell_x},{g.lower_left_cell_y}" == m["ll"]
            assert f"{g.nx},{g.ny}" == m["n"] and _bits(g.cell_size) == m["cell"]
            assert g.srs_definition == m["crs"] and g.data_units == m["units"]
            assert str(g.data_type) == m["data_type"]
            smin, smax, smean = m["stats"].split(";")
            if v == 6 and " EMPTY/" in path:
                # All-missing DSS6 grid: Monolith reports -inf for the stored -2e38/2e38/
                # -FLT_MAX statistics; the engine keeps the stored bits (P11).
                assert (smin, smax, smean) == ("ff800000",) * 3
            else:
                assert (_bits(g.min_value), _bits(g.max_value), _bits(g.mean_value)) == (smin, smax, smean)
            nr, _, rest = m["ranges"].partition(":")
            want = [tuple(x.split("=")) for x in rest.split(",")]
            have = [(_bits(a), str(int(c))) for a, c in zip(g.range_limit_table, g.range_counts)]
            assert have == want, path
    assert n == len(ref) - (1 if v == 6 else 0)
    assert hashlib.sha256(src.read_bytes()).hexdigest() == sha_before


# ---------------------------------------------------------------------------------
# Review follow-up (P24_VERIFY m1, m5, n4)
# ---------------------------------------------------------------------------------


@pytest.mark.parametrize("v", [7, 6])
def test_float_metadata_refusals_and_rounding(tmp_path, v):
    p = new(tmp_path, v)
    path = "/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/"
    before = p.read_bytes()
    spec = dict(grid_type="specified", srs_definition=dss.SHG_WKT, cell_size=2000.0,
                lower_left_cell=(0, 0), units="MM", data_type="PER-CUM", version=v)
    for kw in (dict(null_value=1e39), dict(null_value=float("inf")), dict(null_value=float("-inf")),
               dict(null_value=float("nan")), dict(cell_zero=(float("inf"), 0.0)),
               dict(cell_size=1e39)):
        with pytest.raises(ValueError):
            dss.write_grid(p, path, cells(), **{**spec, **kw})
        assert p.read_bytes() == before, kw
    # Header floats are stored as the nearest float32, as HEC stores them.
    dss.write_grid(p, path, cells(), **{**spec, "cell_zero": (1234567.123, 0.0)})
    with dss.open(p) as f:
        g = f.read_grid(path.upper() if v == 6 else path)
    assert g.x_coord_of_grid_cell_zero == float(np.float32(1234567.123))


def test_module_write_grid_defaults_to_dss7(tmp_path):
    """A DSS6 file written without version=6 is refused with a typed error, unchanged."""
    p = new(tmp_path, 6)
    before = p.read_bytes()
    with pytest.raises(dss.DssError):
        dss.write_grid(p, "/SHG/P24/PRECIP/01SEP2018:0100/01SEP2018:0200/T/", cells(),
                       lower_left_cell=(0, 0), **SHG_KW)
    assert p.read_bytes() == before


def test_copy_with_zero_tail_dss6_precip_2_byte(tmp_path):
    """copy_grid_with_zero_tail into a DSS6 PRECIP_2_BYTE target, keeping the scale and
    base of the source (the case the RasDss base/scale workaround is about)."""
    src = tmp_path / "src6.dss"
    shutil.copyfile(fixture_path("real_grid6.dss"), src)
    out = new(tmp_path, 6)
    path = "/SHG/RUSSIAN RIVER/PRECIP/01SEP2018:0100/01SEP2018:0200/NDGD-RTMA/"
    with dss.open(src) as f:
        g = f.read_grid(path, missing="raw")
    md = g.metadata
    assert md["compression_method"] == 101001
    kw = dict(grid_type="shg", cell_size=g.cell_size, units=g.data_units, data_type="PER-CUM",
              lower_left_cell=(g.lower_left_cell_x, g.lower_left_cell_y), raw=True, version=6,
              compression="precip_2_byte", scale_factor=md["compression_scale_factor"],
              base=md["compression_base"])
    dss.write_grid(out, path, g.data, **kw)
    tail = path.replace("01SEP2018:0100/01SEP2018:0200", "01SEP2018:0200/01SEP2018:0300")
    dss.write_grid(out, tail, np.where(g.data == UNDEF, UNDEF, 0).astype(np.float32), **kw)
    with dss.open(out) as f:
        back = f.read_grid(path, missing="raw")
        t = f.read_grid(tail)
    miss = g.data == UNDEF
    np.testing.assert_array_equal(back.data == UNDEF, miss)
    np.testing.assert_allclose(back.data[~miss], g.data[~miss],
                               atol=0.5 / md["compression_scale_factor"] + 1e-6)
    assert back.metadata["compression_scale_factor"] == md["compression_scale_factor"]
    assert int(np.isnan(t.data).sum()) == int(miss.sum()) and np.nanmax(t.data) == 0.0


@pytest.mark.parametrize("v,comp", [(7, "zlib"), (6, "zlib"), (6, "precip_2_byte")])
def test_specified_wkt_series(tmp_path, v, comp):
    """RasDss.write_grid_timeseries with an EPSG:5070-style WKT: a specified-grid series
    over n+1 interval bounds, DSS7 and DSS6, ZLIB and PRECIP_2_BYTE."""
    p = new(tmp_path, v)
    bounds = [dt.datetime(2018, 9, 1, 23) + dt.timedelta(minutes=30 * i) for i in range(4)]
    frames = np.stack([cells(seed=10 + i) for i in range(3)])
    names = []
    for i in range(3):
        r = dss.write_grid(p, "/EPSG5070/BASIN/PRECIP///AORC/", frames[i], grid_type="specified",
                           srs_definition=dss.SHG_WKT, srs_name="EPSG:5070",
                           start=bounds[i], end=bounds[i + 1], cell_size=2000.0,
                           origin=(-2000000.0, 1400000.0), units="MM", data_type="PER-CUM",
                           compression=comp, version=v)
        names.append(r["pathname"])
        assert r["record_type"] == 430
    assert names[1].upper().endswith("/01SEP2018:2330/01SEP2018:2400/AORC/")
    tol = 0.005 + 1e-6 if comp == "precip_2_byte" else 0
    with dss.open(p) as f:
        for i, n in enumerate(names):
            g = f.read_grid(n)
            assert g.srs_definition == dss.SHG_WKT and g.srs_name == "EPSG:5070"
            assert (g.lower_left_cell_x, g.lower_left_cell_y) == (-1000, 700)
            np.testing.assert_allclose(g.data, frames[i], atol=tol, rtol=0)


def test_hms_equal_start_and_end_is_refused(tmp_path):
    """HmsDssGrid writes PER-CUM grids with equal start and end (P17 requirement 7); the
    writer refuses them, and the consumer must pass the period end instead."""
    p = new(tmp_path, 7)
    t0 = dt.datetime(2018, 9, 1, 1)
    with pytest.raises(ValueError, match="later than its start"):
        dss.write_grid(p, "/SHG/HMS/PRECIP///AORC/", cells(), start=t0, end=t0,
                       lower_left_cell=(0, 0), **SHG_KW)
    r = dss.write_grid(p, "/SHG/HMS/PRECIP///AORC/", cells(), start=t0,
                       end=t0 + dt.timedelta(hours=1), lower_left_cell=(0, 0), **SHG_KW)
    assert r["pathname"] == "/SHG/HMS/PRECIP/01SEP2018:0100/01SEP2018:0200/AORC/"

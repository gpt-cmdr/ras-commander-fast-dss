"""A local Zarr v2 store through the public API.

Inputs are ``fixtures/public/zarr``: real AORC v1.1 chunks, and a store built here. Expected cells
are what xarray 2026.9.0 decodes (``expected.json``); stamps are literals. These tests do not run
xarray, Monolith or hecdss.
"""

import datetime as dt
import hashlib
import json

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssUnsupportedRecordError, gridded

from conftest import FIXTURES

ZARR = FIXTURES / "zarr"
AORC = ZARR / "aorc_2019_subset.zarr"
PANHANDLE = (-102.0, 34.6, -100.7, 35.6)
SHG_BOX = (-536000, 1302000, -430000, 1402000)
PARTS = {"A": "SHG", "B": "PANHANDLE", "F": "AORC"}
TIMES = ("2019-09-19T22:00:00", "2019-09-20T01:00:00")
STAMPS = [
    ("19SEP2019:2100", "19SEP2019:2200"),
    ("19SEP2019:2200", "19SEP2019:2300"),
    ("19SEP2019:2300", "19SEP2019:2400"),
    ("20SEP2019:0000", "20SEP2019:0100"),
]


def expected(variable, nx):
    cases = json.loads((ZARR / "expected.json").read_text(encoding="utf-8"))["cases"]
    (case,) = [c for c in cases if c["store"] == AORC.name and c["variable"] == variable and c["nx"] == nx]
    return case


def test_feature_flag():
    assert "gridded_zarr" in dss.FEATURES


def test_variables_state_what_the_store_states():
    rows = {v["name"]: v for v in gridded.zarr_variables(AORC)}
    assert len(rows) == 8
    rain = rows["APCP_surface"]
    assert rain["error"] is None
    assert (rain["steps"], rain["ny"], rain["nx"]) == (8760, 4201, 8401)
    assert rain["units"] == "kg/m^2"
    assert rain["accumulation_seconds"] == 3600
    assert 'AUTHORITY["EPSG","4326"]' in rain["crs"]
    assert rain["first_time"] == 1546300800  # 2019-01-01 00:00 UTC
    assert rows["TMP_2maboveground"]["accumulation_seconds"] is None
    assert gridded.list_variables(AORC) == sorted(rows)


def test_a_window_reads_as_xarray_decodes_it():
    case = expected("APCP_surface", 157)
    w = gridded.read_zarr(AORC, "APCP_surface", steps=tuple(case["steps"]), bbox=PANHANDLE)
    assert w.cells.shape == (4, 121, 157) and w.cells.dtype == np.float32
    assert (list(w.rows), list(w.cols)) == (case["rows"], case["cols"])
    assert hashlib.sha256(np.ascontiguousarray(w.cells).tobytes()).hexdigest() == case["cells_sha256"]
    assert w.times == case["instants"]
    assert w.accumulation_seconds == 3600
    assert [np.float64(v).tobytes()[::-1].hex() for v in w.transform] == case["transform_bits"]
    # Fall et al. 2023, Fig. 13: 6.6 in. "for the hour ending at 00 UTC" on 20 Sep 2019.
    peaks = w.cells.reshape(4, -1).max(axis=1) / 25.4
    assert np.allclose(peaks[1:], [1.98, 6.59, 2.34], atol=0.005)
    assert dt.datetime.fromtimestamp(w.times[2], dt.timezone.utc).isoformat() == "2019-09-20T00:00:00+00:00"
    # The same window by index, and one cell changed is seen by the hash.
    again = gridded.read_zarr(AORC, "APCP_surface", steps=(6286, 6290), rows=(1752, 1873), cols=(3360, 3517))
    assert np.array_equal(again.cells, w.cells)
    changed = w.cells.copy()
    changed[2, 60, 80] += 1
    assert hashlib.sha256(changed.tobytes()).hexdigest() != case["cells_sha256"]


def test_sea_cells_and_absent_chunks_are_missing_not_zero():
    coast = [c for c in json.loads((ZARR / "expected.json").read_text(encoding="utf-8"))["cases"] if c["missing"] == 921400]
    (coast,) = coast
    w = gridded.read_zarr(AORC, "APCP_surface", steps=tuple(coast["steps"]), rows=tuple(coast["rows"]), cols=tuple(coast["cols"]))
    assert int(np.isnan(w.cells).sum()) == 921400 and int(np.isfinite(w.cells).sum()) == 717000
    assert hashlib.sha256(w.cells.tobytes()).hexdigest() == coast["cells_sha256"]
    # A chunk the folder does not hold: every cell missing, none zero.
    absent = gridded.read_zarr(AORC, "APCP_surface", steps=(0, 2), rows=(2000, 2010), cols=(4000, 4010))
    assert np.isnan(absent.cells).all() and absent.cells.size == 200


@pytest.mark.parametrize("version", [7, 6])
def test_aorc_hours_end_at_their_time(tmp_path, version):
    out = tmp_path / "aorc.dss"
    r = gridded.import_zarr(
        AORC, out, variable="APCP_surface", bbox=PANHANDLE, times=TIMES, envelope=SHG_BOX, parts=PARTS, version=version
    )
    assert r.pathnames == [f"/SHG/PANHANDLE/PRECIPITATION/{d}/{e}/AORC/" for d, e in STAMPS]
    for record in r.records:
        assert record["data_type"] == "PER-CUM"
        assert record["units"] == "MM"
        assert (record["nx"], record["ny"], record["cell_size"]) == (53, 50, 2000.0)
    with dss.open(out) as f:
        grids = [f.read_grid(p, missing="raw") for p in r.pathnames]
    assert [str(g.end_time) for g in grids] == [
        "2019-09-19T22:00:00",
        "2019-09-19T23:00:00",
        "2019-09-20T00:00:00",
        "2019-09-20T01:00:00",
    ]
    assert all(str(g.end_time - np.timedelta64(1, "h")) == str(g.start_time) for g in grids)
    # The storm's hour is the record that ends at 2400, not the one after it.
    peaks = [float(np.asarray(g.data).max()) for g in grids]
    assert peaks.index(max(peaks)) == 2 and peaks[2] > 100.0


def test_a_stated_hour_agrees_and_another_is_refused(tmp_path):
    kwargs = dict(variable="APCP_surface", bbox=PANHANDLE, times=TIMES, envelope=SHG_BOX, parts=PARTS)
    r = gridded.import_zarr(AORC, tmp_path / "a.dss", window_rule="accumulation:01H", **kwargs)
    assert [x["accumulation_stated_by"] for x in r.records] == ["store"] * 4
    out = tmp_path / "b.dss"
    with pytest.raises(DssUnsupportedRecordError, match="the store states 3600 s"):
        gridded.import_zarr(AORC, out, window_rule="accumulation:03H", **kwargs)
    assert not out.exists()


def test_temperature_is_not_precipitation(tmp_path):
    out = tmp_path / "t.dss"
    corner = dict(bbox=(-129.9, 20.1, -129.0, 20.9), times=(TIMES[0], TIMES[0]), parts=PARTS)
    with pytest.raises(DssUnsupportedRecordError, match="not precipitation"):
        gridded.import_zarr(
            AORC, out, variable="TMP_2maboveground", window_rule="accumulation:01H", target_crs=None,
            cell_size=None, align="vortex", **corner
        )
    assert not out.exists()
    r = gridded.import_zarr(
        AORC, out, variable="TMP_2maboveground", target_crs=None, cell_size=None, align="vortex", **corner
    )
    assert r.pathnames == ["/SHG/PANHANDLE/TEMPERATURE/19SEP2019:2200//AORC/"]
    assert r.records[0]["data_type"] == "INST-VAL"


def build(root, attrs, hours=(0.0, 1.0, 2.0)):
    """A store of `<i2` rain on 4 x 6 cells of half a degree, uncompressed, in one chunk each."""

    def array(name, data, dims, extra):
        d = root / name
        d.mkdir(parents=True)
        meta = dict(chunks=list(data.shape), compressor=None, dtype=data.dtype.str, fill_value=None, filters=None,
                    order="C", shape=list(data.shape), zarr_format=2)
        (d / ".zarray").write_text(json.dumps(meta))
        (d / ".zattrs").write_text(json.dumps({"_ARRAY_DIMENSIONS": dims, **extra}))
        (d / ".".join(["0"] * data.ndim)).write_bytes(data.tobytes())

    root.mkdir()
    (root / ".zgroup").write_text('{"zarr_format": 2}')
    array("time", np.array(hours, "<f8"), ["time"], {"units": "hours since 2000-01-01 00:00:00"})
    array("latitude", 30.0 + 0.5 * np.arange(4, dtype="<f8"), ["latitude"], {"units": "degrees_north"})
    array("longitude", -100.0 + 0.5 * np.arange(6, dtype="<f8"), ["longitude"], {"units": "degrees_east"})
    rain = np.arange(len(hours) * 24, dtype="<i2").reshape(len(hours), 4, 6)
    array("precipitation", rain, ["time", "latitude", "longitude"], attrs)
    return root


def test_import_gridded_takes_a_store_and_refuses_rain_with_no_period(tmp_path):
    store = build(tmp_path / "plain.zarr", {"units": "mm", "crs": "EPSG:4326", "_FillValue": -1})
    plain = dict(variable="precipitation", target_crs=None, cell_size=None, align="vortex", parts=PARTS)
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="precipitation at an instant"):
        gridded.import_gridded(store, out, **plain)
    assert not out.exists()
    r = gridded.import_gridded(store, out, window_rule="accumulation:01H", **plain)
    assert [p.split("/")[4:6] for p in r.pathnames] == [
        ["31DEC1999:2300", "31DEC1999:2400"],
        ["01JAN2000:0000", "01JAN2000:0100"],
        ["01JAN2000:0100", "01JAN2000:0200"],
    ]
    assert {x["accumulation_stated_by"] for x in r.records} == {"caller"}
    with dss.open(out) as f:
        g = f.read_grid(r.pathnames[1], missing="raw")
    # Rows are stored south to north, as the store has them: no value or order changes.
    assert np.array_equal(np.asarray(g.data, dtype=np.float32).reshape(4, 6), np.arange(24, 48, dtype=np.float32).reshape(4, 6))


def test_a_store_with_no_crs_is_not_given_one(tmp_path):
    store = build(tmp_path / "nocrs.zarr", {"units": "mm", "_FillValue": -1})
    (row,) = gridded.zarr_variables(store)
    assert row["crs"] is None and row["error"] is None
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="states no CRS"):
        gridded.import_zarr(store, out, variable="precipitation", window_rule="accumulation:01H", envelope=SHG_BOX)
    assert not out.exists()


@pytest.mark.parametrize(
    "kwargs, text",
    [
        (dict(bbox=(1, 2, 3)), "bbox must be"),
        (dict(bbox=(3, 0, 1, 1)), "west < east"),
        (dict(times=("2019-09-20", "2019-09-19")), "first no later than last"),
        (dict(times=("2019-09-20",)), "first, last"),
        (dict(version=5), "version must be 6 or 7"),
    ],
)
def test_bad_arguments_are_value_errors(tmp_path, kwargs, text):
    out = tmp_path / "o.dss"
    with pytest.raises(ValueError, match=text):
        gridded.import_zarr(AORC, out, variable="APCP_surface", **kwargs)
    assert not out.exists()


def test_selections_that_take_nothing_are_refused(tmp_path):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="holds no frames"):
        gridded.import_zarr(AORC, out, variable="APCP_surface", bbox=PANHANDLE, times=(0, 10), envelope=SHG_BOX)
    with pytest.raises(DssUnsupportedRecordError, match="does not touch its grid"):
        gridded.import_zarr(AORC, out, variable="APCP_surface", bbox=(10, 0, 11, 1), times=TIMES, envelope=SHG_BOX)
    with pytest.raises(DssUnsupportedRecordError, match="not a Zarr v2 store"):
        gridded.zarr_variables(tmp_path)
    assert not out.exists()


# ---- Limits, stop, a complete store, the selection, recognition, links.


def test_limits_are_named_and_settable():
    names = gridded.zarr_limits()
    assert names["max_cells_per_import"] == 1 << 32 and names["refuse_absent_chunks"] is False
    assert names["max_seconds_per_read"] is None
    kwargs = dict(steps=(6288, 6289), bbox=PANHANDLE)
    assert gridded.read_zarr(AORC, "APCP_surface", **kwargs).cells.size == 18997
    for limits, text in [
        ({"max_cells": 18996}, "max_cells"),
        ({"max_read_bytes": 100}, "max_read_bytes"),
        ({"max_decoded_bytes_per_read": 100, "chunk_cache_bytes": 0}, "max_decoded_bytes_per_read"),
        ({"max_chunks_per_read": 1}, "max_chunks_per_read"),
        ({"max_chunk_bytes": 1000}, "max_chunk_bytes"),
    ]:
        with pytest.raises(DssUnsupportedRecordError, match=text):
            gridded.read_zarr(AORC, "APCP_surface", limits=limits, **kwargs)
    # Exactly enough reads.
    assert gridded.read_zarr(AORC, "APCP_surface", limits={"max_cells": 18997}, **kwargs).cells.size == 18997
    with pytest.raises(ValueError, match="limits has no"):
        gridded.read_zarr(AORC, "APCP_surface", limits={"max_cell": 1}, **kwargs)
    with pytest.raises(ValueError, match="limit max_cells"):
        gridded.read_zarr(AORC, "APCP_surface", limits={"max_cells": "many"}, **kwargs)


def test_a_stop_flag_ends_a_read_and_an_import(tmp_path):
    stop = gridded.ZarrStop()
    kwargs = dict(steps=(6288, 6289), bbox=PANHANDLE, stop=stop)
    assert gridded.read_zarr(AORC, "APCP_surface", **kwargs).cells.size == 18997
    stop.set()
    assert stop.is_set()
    with pytest.raises(DssUnsupportedRecordError, match="the read was stopped"):
        gridded.read_zarr(AORC, "APCP_surface", **kwargs)
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="the read was stopped"):
        gridded.import_zarr(AORC, out, variable="APCP_surface", times=TIMES, envelope=SHG_BOX, stop=stop)
    stop.clear()
    assert gridded.read_zarr(AORC, "APCP_surface", **kwargs).cells.size == 18997
    r = gridded.import_zarr(AORC, out, variable="APCP_surface", times=TIMES, envelope=SHG_BOX, stop=stop)
    assert len(r.records) == 4


def test_strict_refuses_an_incomplete_store(tmp_path):
    absent = dict(steps=(6288, 6289), rows=(1700, 1710), cols=(3000, 3010))
    assert np.isnan(gridded.read_zarr(AORC, "APCP_surface", **absent).cells).all()
    with pytest.raises(DssUnsupportedRecordError, match="refuse_absent_chunks"):
        gridded.read_zarr(AORC, "APCP_surface", strict=True, **absent)
    present = dict(steps=(6288, 6289), rows=(1700, 1710), cols=(3400, 3410))
    assert gridded.read_zarr(AORC, "APCP_surface", strict=True, **present).cells.size == 100
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="refuse_absent_chunks"):
        gridded.import_zarr(
            AORC, out, variable="APCP_surface", times=TIMES, bbox=(-104.5, 34.6, -100.7, 35.6), envelope=SHG_BOX, strict=True
        )


def test_import_gridded_takes_the_same_selection(tmp_path):
    kwargs = dict(variable="APCP_surface", times=TIMES, envelope=SHG_BOX, parts=PARTS)
    a = gridded.import_zarr(AORC, tmp_path / "a.dss", **kwargs)
    b = gridded.import_gridded(AORC, tmp_path / "b.dss", **kwargs)
    c = gridded.import_gridded(AORC, tmp_path / "c.dss", bbox=(-180, -90, 180, 90), **kwargs)
    assert a.pathnames == b.pathnames == c.pathnames and len(a.pathnames) == 4
    cells = 0
    for name in a.pathnames:
        grids = []
        for path in ("a.dss", "b.dss", "c.dss"):
            with dss.open(tmp_path / path) as f:
                grids.append(np.asarray(f.read_grid(name, missing="raw").data, dtype=np.float32))
        # The envelope's own cells, and the whole grid read as it was before: the same bits.
        assert grids[0].tobytes() == grids[1].tobytes() == grids[2].tobytes()
        assert grids[0].max() > 1.0
        cells += grids[0].size
    assert cells == 10600


def test_a_year_of_the_whole_grid_is_refused_while_planning(tmp_path):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="max_cells_per_import") as e:
        gridded.import_gridded(AORC, out, variable="APCP_surface", target_crs=None, cell_size=None)
    assert e.value.phase == "planning"
    assert "8760 steps of 8401 x 4201 cells" in str(e.value) and "State times" in str(e.value)
    assert not out.exists()
    # A selection on an input that is not a store is refused, not ignored.
    grib = FIXTURES / "vortex_p46" / "apcp_a.grib2"
    with pytest.raises(DssUnsupportedRecordError, match="takes bbox or times"):
        gridded.import_gridded(
            grib, out, variable="Total_precipitation_surface_1_Hour_Accumulation", times=("2000-01-01", "2030-01-01")
        )
    assert not out.exists()


def aorc_like(root, attrs, hours):
    """`build`, with the rain named as AORC names it."""
    build(root, attrs, hours)
    (root / "precipitation").rename(root / "APCP_surface")
    return root


@pytest.mark.parametrize(
    "extra, hours, why",
    [
        ({"aorc_version": None}, (1.0, 2.0), "aorc_version"),
        ({"aorc_version": "v9.9"}, (1.0, 2.0), "aorc_version"),
        ({"aorc_version": "v1.1"}, (0.5, 1.5), "top of the hour"),
        ({"aorc_version": "v1.1"}, (5.5,), "top of the hour"),
        ({"aorc_version": "v1.1", "time_label": "start"}, (1.0, 2.0), "time_label"),
        ({"aorc_version": "v1.1", "accumulation_interval_hours": 24}, (1.0, 2.0), "accumulation_interval_hours"),
    ],
)
def test_a_store_not_recognized_as_aorc_has_no_period(tmp_path, extra, hours, why):
    attrs = {"units": "kg/m^2", "crs": "EPSG:4326", "_FillValue": -1, **extra}
    store = aorc_like(tmp_path / "s.zarr", attrs, hours)
    (row,) = gridded.zarr_variables(store)
    assert row["accumulation_seconds"] is None and why in row["period_note"]
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match=why) as e:
        gridded.import_zarr(store, out, variable="APCP_surface", target_crs=None, cell_size=None, align="vortex")
    assert "accumulation:" in str(e.value)
    assert not out.exists()


@pytest.mark.parametrize("hours", [(1.0, 2.0, 3.0), (1.0, 2.0, 7.0), (5.0,)])
def test_a_recognized_store_is_hourly_whatever_its_spacing(tmp_path, hours):
    attrs = {"units": "kg/m^2", "crs": "EPSG:4326", "_FillValue": -1, "aorc_version": "v1.1"}
    store = aorc_like(tmp_path / "s.zarr", attrs, hours)
    (row,) = gridded.zarr_variables(store)
    assert row["accumulation_seconds"] == 3600 and row["period_note"] is None
    r = gridded.import_zarr(
        store, tmp_path / "o.dss", variable="APCP_surface", target_crs=None, cell_size=None, align="vortex"
    )
    last = int(hours[-1])
    assert r.pathnames[-1].split("/")[4:6] == [f"01JAN2000:{last - 1:02d}00", f"01JAN2000:{last:02d}00"]
    assert len(r.pathnames) == len(hours)


def test_a_junction_below_the_store_is_not_followed(tmp_path):
    _winapi = pytest.importorskip("_winapi")
    if not hasattr(_winapi, "CreateJunction"):
        pytest.skip("no junctions on this platform")
    attrs = {"units": "mm", "crs": "EPSG:4326", "_FillValue": -1}
    store = build(tmp_path / "s.zarr", attrs)
    assert gridded.read_zarr(store, "precipitation", steps=(0, 1)).cells.size == 24
    outside = tmp_path / "outside"
    (store / "precipitation").rename(outside)
    _winapi.CreateJunction(str(outside), str(store / "precipitation"))
    assert (store / "precipitation" / ".zarray").exists(), "the junction resolves for an ordinary reader"
    assert gridded.zarr_variables(store) == []
    with pytest.raises(DssUnsupportedRecordError):
        gridded.read_zarr(store, "precipitation", steps=(0, 1))


@pytest.mark.parametrize(
    "kwargs, word",
    [
        (dict(steps=(6288.9, 6289.9)), "steps"),
        (dict(steps=(6288, 6289), rows=(1810.5, 1812)), "rows"),
        (dict(steps=(6288, 6289), cols=(3440, True)), "cols"),
        (dict(steps=(6288, 6289), strict="False"), "strict"),
        (dict(steps=(6288, 6289), strict=1), "strict"),
        (dict(steps=(6288, 6289), limits={"max_cells": True}), "max_cells"),
    ],
)
def test_arguments_that_are_not_what_they_say_are_refused_not_coerced(kwargs, word):
    with pytest.raises(ValueError, match=word):
        gridded.read_zarr(AORC, "APCP_surface", **kwargs)


def test_whole_number_floats_are_still_indexes():
    a = gridded.read_zarr(AORC, "APCP_surface", steps=(6288, 6289), rows=(1810, 1812), cols=(3440, 3442))
    b = gridded.read_zarr(AORC, "APCP_surface", steps=(6288.0, 6289.0), rows=(1810, 1812), cols=(3440, 3442))
    assert np.ascontiguousarray(a.cells).tobytes() == np.ascontiguousarray(b.cells).tobytes()

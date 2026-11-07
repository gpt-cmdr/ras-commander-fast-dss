"""NetCDF files through ``import_gridded`` and ``list_variables`` (packet P46b), by the public API.

The AORC hours in ``fixtures/public/netcdf_p46b`` hold NOAA's rain for 19 and 20 September 2019 in the
layout of the real ``AORC_APCP_MARFC_1984010100.nc4``, which is a dry hour. Expected stamps are literals.
Vortex and HEC's readers are in ``oracle/vortex/run_import.py --netcdf``.
"""

import os
import shutil
import threading

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssError, DssUnsupportedRecordError, gridded

from conftest import FIXTURES

NC = FIXTURES / "netcdf"
P46B = FIXTURES / "netcdf_p46b"
APCP = "APCP_surface"
HOURS = [P46B / f"AORC_APCP_TXPAN_{t}.nc4" for t in ("2019091923", "2019092000", "2019092001")]
SIX = P46B / "AORC_APCP_TXPAN_6H_2019091921.nc4"
PARTS = {"A": "SHG", "B": "P46B", "F": "CI"}
BOX = dict(envelope=(-500000.0, 1300000.0, -400000.0, 1380000.0))
STAMPS = [
    "/SHG/P46B/PRECIPITATION/19SEP2019:2200/19SEP2019:2300/CI/",
    "/SHG/P46B/PRECIPITATION/19SEP2019:2300/19SEP2019:2400/CI/",
    "/SHG/P46B/PRECIPITATION/20SEP2019:0000/20SEP2019:0100/CI/",
]


def test_feature_flag_and_names():
    assert "gridded_netcdf" in dss.FEATURES
    assert {"StopHandle", "netcdf_limits"} <= set(gridded.__all__)
    limits = gridded.netcdf_limits()
    assert limits == {"max_decoded_bytes_per_read": 32 << 30, "chunk_cache_bytes": 64 << 20, "max_seconds_per_read": None}


def test_list_variables_reads_netcdf_4_classic_and_groups():
    assert gridded.list_variables(HOURS[0]) == [APCP]
    assert gridded.list_variables(NC / "AORC_TMP_MARFC_1984010100.nc4") == ["TMP_2maboveground"]
    assert gridded.list_variables(NC / "nb731_float32.nc") == ["precip_incremental"]
    # A variable in a group is listed, and selected, by its full name.
    assert gridded.list_variables(NC / "group_x_first.nc4") == ["Grid/Intermediate/source", "Grid/precipitation"]


@pytest.mark.parametrize("version", [7, 6])
def test_aorc_hours_end_at_the_files_time_across_midnight(tmp_path, version):
    out = tmp_path / "o.dss"
    seen = []
    r = gridded.import_gridded(HOURS, out, variable=APCP, parts=PARTS, version=version, progress=seen.append, **BOX)
    assert r.pathnames == STAMPS
    assert [s["done"] for s in seen] == [1, 2, 3]
    assert {(x["data_type"], x["units"], x["outcome"]) for x in r.records} == {("PER-CUM", "MM", "added")}
    with dss.open(out) as f:
        assert sorted(f.catalog()["pathname"].tolist()) == sorted(STAMPS)
        assert int(f.version) == version
        peaks = []
        for name, start, end in zip(STAMPS, ("19T22", "19T23", "20T00"), ("19T23", "20T00", "20T01")):
            g = f.read_grid(name)
            assert str(g.start_time)[:13] == f"2019-09-{start}" and str(g.end_time)[:13] == f"2019-09-{end}"
            assert (g.data_units, g.nx, g.ny) == ("MM", 50, 40)
            peaks.append(float(np.nanmax(g.data)))
        # Real rain, a different field each hour.
        assert min(peaks) > 10.0 and len(set(peaks)) == 3


def test_the_real_dry_aorc_file_and_its_temperature(tmp_path):
    box = dict(envelope=(1462000.0, 1856000.0, 1696000.0, 2180000.0))
    r = gridded.import_gridded(NC / "AORC_APCP_MARFC_1984010100.nc4", tmp_path / "p.dss", variable=APCP, parts=PARTS, **box)
    assert r.pathnames == ["/SHG/P46B/PRECIPITATION/31DEC1983:2300/31DEC1983:2400/CI/"]
    r = gridded.import_gridded(
        NC / "AORC_TMP_MARFC_1984010100.nc4", tmp_path / "t.dss", variable="TMP_2maboveground", parts=PARTS, **box
    )
    assert r.pathnames == ["/SHG/P46B/TEMPERATURE/31DEC1983:2400//CI/"]
    assert (r.records[0]["data_type"], r.records[0]["units"]) == ("INST-VAL", "DEG C")


def test_the_notebook_731_shape_keeps_its_bounds_and_inches(tmp_path):
    r = gridded.import_gridded(
        NC / "nb731_float32.nc", tmp_path / "o.dss", variable="precip_incremental", parts=PARTS,
        envelope=(-142000.0, 822000.0, -140000.0, 824000.0),
    )
    assert [p.split("/")[4:6] for p in r.pathnames] == [
        ["01JUN2020:0600", "01JUN2020:0630"], ["01JUN2020:0630", "01JUN2020:0700"],
        ["01JUN2020:0700", "01JUN2020:0730"], ["01JUN2020:0730", "01JUN2020:0800"],
    ]
    assert {(x["data_type"], x["units"]) for x in r.records} == {("PER-CUM", "IN")}


def refused(tmp_path, path, variable, text, **kw):
    """A typed refusal while planning, holding `text`, that leaves an existing output byte for byte."""
    out = tmp_path / "kept.dss"
    if not out.exists():
        gridded.import_gridded(HOURS[1], out, variable=APCP, parts=PARTS, **BOX)
    before = out.read_bytes()
    with pytest.raises(DssUnsupportedRecordError, match=text) as e:
        gridded.import_gridded(path, out, variable=variable, parts=PARTS, **{**BOX, **kw})
    assert (e.value.phase, e.value.completed, e.value.in_doubt) == ("planning", [], None)
    assert out.read_bytes() == before
    return str(e.value)


def test_refusals_are_typed_and_leave_the_output_as_it_was(tmp_path):
    whole = dict(envelope=None, align="vortex")
    texts = [
        refused(tmp_path, P46B / "AORC_APCP_TXPAN_3HOURLY.nc4", APCP, "not one hour apart"),
        refused(tmp_path, P46B / "precip_percent.nc4", "precipitation", "not a rain depth", **whole),
        refused(tmp_path, NC / "two_d_axes.nc", "data", "two-dimensional", **whole),
        refused(tmp_path, NC / "uneven_axis.nc", "data", "not evenly spaced", **whole),
        refused(tmp_path, NC / "levels.nc", "data", "several vertical levels", **whole),
        refused(tmp_path, NC / "seconds_float.nc", "data", "fraction of a second", **whole),
        refused(tmp_path, NC / "instants.nc", "rain", "does not fit the frame's time", **whole),
        refused(tmp_path, NC / "gfs.nc", "Precipitation_rate_surface", "file-name rule GFS", **whole),
        refused(tmp_path, HOURS[0], "TMP", "not in this input"),
        refused(tmp_path, HOURS[0], APCP, "a stated accumulation is for GRIB2", window_rule="accumulation:01H"),
    ]
    assert len(set(texts)) == len(texts), "each refusal says its own reason"


def test_a_name_rule_that_moves_a_window_is_vortex_only(tmp_path):
    out = tmp_path / "o.dss"
    r = gridded.import_gridded(
        P46B / "AORC_APCP_TXPAN_3HOURLY.nc4", out, variable=APCP, parts=PARTS, window_rule="vortex", **BOX
    )
    assert [p.split("/")[4:6] for p in r.pathnames] == [
        ["19SEP2019:1700", "19SEP2019:1800"], ["19SEP2019:2000", "19SEP2019:2100"], ["19SEP2019:2300", "19SEP2019:2400"],
    ]


def test_an_output_that_is_an_input_by_hard_link_is_refused(tmp_path):
    source = tmp_path / HOURS[1].name
    shutil.copyfile(HOURS[1], source)
    link = tmp_path / "o.dss"
    os.link(source, link)
    before = source.read_bytes()
    for output in (link, source):
        with pytest.raises(DssUnsupportedRecordError, match="it is the input") as e:
            gridded.import_gridded(source, output, variable=APCP, parts=PARTS, **BOX)
        assert e.value.phase == "planning"
        assert source.read_bytes() == before


def test_limits_reach_the_reader_and_bad_ones_are_value_errors(tmp_path):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="max_decoded_bytes_per_read") as e:
        gridded.import_gridded(HOURS[1], out, variable=APCP, parts=PARTS, max_decoded_bytes_per_read=1, **BOX)
    assert (e.value.completed, e.value.in_doubt) == ([], None)
    assert not out.exists()
    for bad in (dict(max_seconds_per_read=0), dict(max_decoded_bytes_per_read=-1), dict(chunk_cache_bytes=1.5),
                dict(max_seconds_per_read=True), dict(stop=object())):
        with pytest.raises(ValueError):
            gridded.import_gridded(HOURS[1], out, variable=APCP, parts=PARTS, **bad, **BOX)
    assert not out.exists()
    r = gridded.import_gridded(
        HOURS[1], out, variable=APCP, parts=PARTS, max_seconds_per_read=600, max_decoded_bytes_per_read=1 << 30,
        chunk_cache_bytes=1 << 20, **BOX,
    )
    assert r.pathnames == [STAMPS[1]]


def test_a_stop_ends_a_run_and_reports_what_was_written(tmp_path):
    stop = gridded.StopHandle()
    assert stop.requested is False and "False" in repr(stop)
    stop.request()
    out = tmp_path / "before.dss"
    with pytest.raises(DssError, match="stopped by the caller's stop handle") as e:
        gridded.import_gridded(SIX, out, variable=APCP, parts=PARTS, stop=stop, **BOX)
    assert (e.value.phase, e.value.completed, e.value.in_doubt) == ("planning", [], None)
    assert not out.exists()
    stop.clear()
    assert stop.requested is False

    # From another thread, once the second record is written.
    out = tmp_path / "during.dss"
    second = threading.Event()
    asked = threading.Event()

    def ask():
        second.wait(60)
        stop.request()
        asked.set()

    def progress(p):
        if p["done"] == 2:
            second.set()
            assert asked.wait(60)

    t = threading.Thread(target=ask)
    t.start()
    with pytest.raises(DssError, match="stopped by the caller's stop handle") as e:
        gridded.import_gridded(SIX, out, variable=APCP, parts=PARTS, stop=stop, progress=progress, **BOX)
    t.join()
    assert e.value.phase == "writing" and e.value.in_doubt is None
    done = [r["pathname"] for r in e.value.completed]
    assert len(done) == 2
    with dss.open(out) as f:
        assert sorted(f.catalog()["pathname"].tolist()) == sorted(done)


def test_bbox_and_times_are_refused_on_netcdf_and_never_ignored(tmp_path):
    """P49's selection arguments are for a Zarr store. On a NetCDF file each is a refusal, not a silent whole read."""
    out = tmp_path / "o.dss"
    for stated in (dict(bbox=(-102.0, 35.0, -101.0, 35.5)), dict(times=("2019-09-19T23:00", "2019-09-20T00:00")),
                   dict(bbox=(-102.0, 35.0, -101.0, 35.5), times=("2019-09-19T23:00", "2019-09-20T00:00"))):
        with pytest.raises(DssUnsupportedRecordError, match="not a source that takes bbox or times") as e:
            gridded.import_gridded(SIX, out, variable=APCP, parts=PARTS, **stated, **BOX)
        assert (e.value.phase, e.value.completed, e.value.in_doubt) == ("planning", [], None)
        assert not out.exists()
    assert len(gridded.import_gridded(SIX, out, variable=APCP, parts=PARTS, **BOX).records) == 6

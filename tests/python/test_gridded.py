"""P46: gridded import through the public API. Public synthetic inputs only.

``fixtures/public/vortex_p46/`` holds GRIB2 files encoded from a formula and, in ``expected.json``,
what Vortex 1.1.0 wrote from them. Value parity is ``crates/dss-vortex/tests/import_fixture.rs``; this
file covers the API: typed errors, where a run stopped, progress, locks and the process itself. HEC
oracle comparisons are local-only (evidence/P46/REPORT.md).
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import shutil
import sys
import threading
from pathlib import Path

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssFormatError, DssLockedError, DssUnsupportedRecordError, gridded

from conftest import FIXTURES, fixture_path
from test_lock import holder_factory, ranges  # noqa: F401  (a fixture, used by name)

P46 = FIXTURES / "vortex_p46"
APCP = "Total_precipitation_surface_1_Hour_Accumulation"
TEMPERATURE = "Temperature_height_above_ground"
GEO_WKT = (
    'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433]]'
)
BOX = dict(envelope=(-90.2, 34.9, -89.8, 35.1), envelope_crs=GEO_WKT, cell_size=2000.0)
PARTS = {"A": "SHG", "B": "P46", "F": "CI"}
UNDEF = np.float32(-3.4028235e38)
JUNE_14_23 = dt.datetime(2024, 6, 14, 23)
HOUR = dt.timedelta(hours=1)


def inputs(*names):
    return [P46 / n for n in names]


def records_in(path):
    with dss.open(path) as f:
        return sorted(str(p) for p in f.catalog()["pathname"].tolist())


def cells_of(path, pathname):
    with dss.open(path) as f:
        return np.asarray(f.read_grid(pathname, missing="raw").data, dtype=np.float32).copy()


def test_feature_flag():
    assert "gridded_import" in dss.FEATURES


@pytest.mark.parametrize("version", [7, 6])
def test_import_records_what_it_wrote(tmp_path, version):
    out = tmp_path / "o.dss"
    seen = []
    r = gridded.import_gridded(
        inputs("apcp_a.grib2", "apcp_b.grib2"), out, variable=APCP, parts=PARTS, version=version,
        progress=seen.append, **BOX,
    )
    names = ["/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/",
             "/SHG/P46/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/CI/"]
    assert r.pathnames == names
    assert records_in(out) == names
    assert [s["done"] for s in seen] == [1, 2]
    assert {s["total"] for s in seen} == {2}
    assert [s["input"] for s in seen] == [0, 1]
    first = r.records[0]
    assert (first["data_type"], first["units"], first["outcome"]) == ("PER-CUM", "MM", "added")
    assert first["c_part_source"] == "short_name"
    assert Path(first["input"]).name == "apcp_a.grib2"
    with dss.open(out) as f:
        assert f.version == version
        g = f.read_grid(names[0], missing="raw")
    assert (g.nx, g.ny) == (first["nx"], first["ny"])
    assert g.data_units == "MM"


def test_product_selection_and_list_variables(tmp_path):
    assert gridded.list_variables(P46 / "apcp_a.grib2") == [APCP]
    out = tmp_path / "o.dss"
    r = gridded.import_gridded(inputs("apcp_a.grib2"), out, product="total_precipitation", parts=PARTS, **BOX)
    assert len(r.records) == 1
    with pytest.raises(DssUnsupportedRecordError, match="no variable matches"):
        gridded.import_gridded(inputs("apcp_a.grib2"), tmp_path / "n.dss", product="mrms_qpe", parts=PARTS, **BOX)
    assert not (tmp_path / "n.dss").exists()


def test_mrms_stamp_crosses_midnight(tmp_path):
    out = tmp_path / "o.dss"
    names = [n for n in P46.iterdir() if n.name.startswith("MRMS")]
    r = gridded.import_gridded(sorted(names), out, product="mrms_qpe", parts=PARTS, **BOX)
    assert r.pathnames == ["/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/",
                           "/SHG/P46/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/CI/"]


# --- Refusals are typed, name their cause, and leave nothing.


@pytest.mark.parametrize(
    "kwargs, error, text",
    [
        (dict(variable=APCP, envelope=(-90.0, 35.0, -90.2, 35.1)), DssUnsupportedRecordError, "empty"),
        (dict(variable="nothing"), DssUnsupportedRecordError, "Observed behaviour we did not implement"),
        (dict(variable=APCP, data_type="PER-MAX"), ValueError, "data_type"),
        (dict(variable=APCP, method="cubic"), ValueError, "method"),
        (dict(variable=APCP, align="close"), ValueError, "align"),
        (dict(variable=APCP, window_rule="x"), ValueError, "window_rule"),
        (dict(variable=APCP, duplicates="x"), ValueError, "duplicates"),
        (dict(variable=APCP, version=5), ValueError, "version"),
        (dict(variable=APCP, threads=0), ValueError, "threads"),
        (dict(variable=APCP, parts={"D": "01JAN2000"}), ValueError, "D and E"),
        (dict(variable=APCP, parts={"B": "a/b"}), ValueError, "slashes"),
        (dict(variable=APCP, parts={"A": "*"}), ValueError, "wildcard"),
        (dict(variable=APCP, product="mrms_qpe"), ValueError, "exactly one"),
        (dict(), ValueError, "exactly one"),
        (dict(variable=APCP, target_crs="not a crs"), DssFormatError, "WKT"),
    ],
)
def test_bad_requests_are_typed_and_create_nothing(tmp_path, kwargs, error, text):
    out = tmp_path / "o.dss"
    args = {**BOX, "parts": PARTS, **kwargs}
    with pytest.raises(error, match=text):
        gridded.import_gridded(inputs("apcp_a.grib2"), out, **args)
    assert not out.exists()


def test_unsnapped_origin_needs_the_opt_in(tmp_path):
    out = tmp_path / "o.dss"
    source = dict(target_crs=None, cell_size=None, parts=PARTS)
    with pytest.raises(DssUnsupportedRecordError, match="Observed behaviour we did not implement"):
        gridded.import_gridded(inputs("tmp_instant.grib2"), out, variable=TEMPERATURE, **source)
    assert not out.exists()


def test_input_that_is_not_grib_or_is_missing(tmp_path):
    junk = tmp_path / "junk.grib2"
    junk.write_bytes(b"this holds no message")
    with pytest.raises(DssUnsupportedRecordError, match="no reader recognises") as e:
        gridded.import_gridded([junk], tmp_path / "o.dss", variable=APCP, parts=PARTS, **BOX)
    assert e.value.phase == "planning"
    assert Path(e.value.input) == junk
    with pytest.raises(OSError):
        gridded.import_gridded([tmp_path / "gone.grib2"], tmp_path / "o.dss", variable=APCP, parts=PARTS, **BOX)
    assert not (tmp_path / "o.dss").exists()


def test_truncated_grib_is_typed(tmp_path):
    data = (P46 / "apcp_a.grib2").read_bytes()
    bad = tmp_path / "bad.grib2"
    for cut in (10, 40, len(data) // 2, len(data) - 2):
        bad.write_bytes(data[:cut])
        with pytest.raises((DssFormatError, DssUnsupportedRecordError)):
            gridded.import_gridded([bad], tmp_path / "o.dss", variable=APCP, parts=PARTS, **BOX)
        assert not (tmp_path / "o.dss").exists()


def test_repeated_pathname_is_refused_unless_stated(tmp_path):
    out = tmp_path / "o.dss"
    both = inputs("apcp_a.grib2", "apcp_joined.grib2")
    with pytest.raises(DssUnsupportedRecordError, match="twice") as e:
        gridded.import_gridded(both, out, variable=APCP, parts=PARTS, **BOX)
    assert e.value.frame == 1
    assert not out.exists()
    r = gridded.import_gridded(both, out, variable=APCP, parts=PARTS, duplicates="last", **BOX)
    assert len(r.records) == 3
    assert all(Path(x["input"]).name == "apcp_joined.grib2" for x in r.records)


def test_hrrr_window_rule_is_explicit(tmp_path):
    out = tmp_path / "o.dss"
    r = gridded.import_gridded(inputs("hrrr.t06z.wrfsfcf01.tmp.grib2"), out, variable=TEMPERATURE, parts=PARTS, **BOX)
    # An instant under the file-name rule keeps Vortex's one-hour window, as PER-AVER.
    assert r.records[0]["data_type"] == "PER-AVER"
    assert r.pathnames == ["/SHG/P46/TEMPERATURE/15JUN2024:0600/15JUN2024:0700/CI/"]


# --- A failure partway.


def test_failure_at_second_input_keeps_the_first(tmp_path):
    """The real route: the next input is damaged after it was planned."""
    a, b = tmp_path / "a.grib2", tmp_path / "b.grib2"
    shutil.copy(P46 / "apcp_a.grib2", a)
    shutil.copy(P46 / "apcp_b.grib2", b)
    out = tmp_path / "o.dss"

    def damage(_):
        b.write_bytes(b.read_bytes()[:30])

    with pytest.raises((DssFormatError, DssUnsupportedRecordError)) as e:
        gridded.import_gridded([a, b], out, variable=APCP, parts=PARTS, progress=damage, **BOX)
    err = e.value
    assert err.phase == "writing"
    assert Path(err.input) == b
    assert len(err.completed) == 1
    assert "b.grib2" in str(err)
    names = records_in(out)
    assert names == [err.completed[0]["pathname"]]
    assert np.isfinite(cells_of(out, names[0])).all()


def test_an_input_that_changes_after_planning_is_not_written_under_the_planned_window(tmp_path):
    """The real route: the next input now states another hour than the plan named it."""
    a, b = tmp_path / "a.grib2", tmp_path / "b.grib2"
    shutil.copy(P46 / "apcp_a.grib2", a)
    whole = (P46 / "apcp_b.grib2").read_bytes()
    b.write_bytes(whole)
    out = tmp_path / "o.dss"

    def move_to_five(_):
        # Reference time 00:00 to 05:00, and period end 01:00 to 06:00, of 15 June 2024.
        changed = whole.replace(b"\x07\xe8\x06\x0f\x00", b"\x07\xe8\x06\x0f\x05")
        changed = changed.replace(b"\x07\xe8\x06\x0f\x01", b"\x07\xe8\x06\x0f\x06")
        assert changed != whole
        b.write_bytes(changed)

    with pytest.raises(DssUnsupportedRecordError, match="decoded frame states") as e:
        gridded.import_gridded([a, b], out, variable=APCP, parts=PARTS, progress=move_to_five, **BOX)
    assert e.value.phase == "writing"
    assert Path(e.value.input) == b
    assert e.value.frame == 0
    # Only the first input's record is there, under its own window.
    assert records_in(out) == ["/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/"]


def test_a_raising_progress_callback_stops_the_run(tmp_path):
    out = tmp_path / "o.dss"

    class Stop(Exception):
        pass

    def stop(p):
        raise Stop(p["pathname"])

    with pytest.raises(Stop) as e:
        gridded.import_gridded(inputs("apcp_joined.grib2"), out, variable=APCP, parts=PARTS, progress=stop, **BOX)
    assert len(e.value.completed) == 1
    assert len(records_in(out)) == 1


def test_other_records_are_unchanged_on_a_public_grid_file(tmp_path):
    for name, version in (("p24_hec_grid7.dss", 7), ("p24_hec_grid6.dss", 6)):
        out = tmp_path / name
        shutil.copy(fixture_path(name), out)
        before = {n: cells_of(out, n).tobytes() for n in records_in(out)}
        assert before
        r = gridded.import_gridded(inputs("apcp_a.grib2"), out, variable=APCP, parts=PARTS, **BOX)
        assert r.records[0]["outcome"] == "added"
        after = records_in(out)
        assert len(after) == len(before) + 1
        for n, cells in before.items():
            assert cells_of(out, n).tobytes() == cells, n
        with pytest.raises(DssUnsupportedRecordError, match="version"):
            gridded.import_gridded(inputs("apcp_b.grib2"), out, variable=APCP, parts=PARTS,
                                   version=13 - version, **BOX)
        assert len(records_in(out)) == len(after)


# --- Locks: inherited from the writer, and checked before decoding.


def lock_word(out):
    reason, off, n = ranges(out)[0]
    assert reason == "primary_write"
    return off, n


def test_a_held_file_is_refused_and_untouched(tmp_path, holder_factory):  # noqa: F811
    out = tmp_path / "o.dss"
    gridded.import_gridded(inputs("apcp_a.grib2"), out, variable=APCP, parts=PARTS, **BOX)
    before = out.read_bytes()
    off, n = lock_word(out)
    h = holder_factory(out)
    h.lock(off, n)
    with pytest.raises(DssLockedError) as e:
        gridded.import_gridded(inputs("apcp_b.grib2"), out, variable=APCP, parts=PARTS, **BOX)
    assert e.value.phase == "planning"
    assert e.value.completed == []
    h.exit()
    assert out.read_bytes() == before


def test_a_lock_taken_partway_stops_the_run_with_nothing_partial(tmp_path, holder_factory):  # noqa: F811
    out = tmp_path / "o.dss"
    held = []

    def take_lock(_):
        if not held:
            off, n = lock_word(out)
            h = holder_factory(out)
            h.lock(off, n)
            held.append(h)

    with pytest.raises(DssLockedError) as e:
        gridded.import_gridded(inputs("apcp_joined.grib2"), out, variable=APCP, parts=PARTS, progress=take_lock, **BOX)
    assert e.value.phase == "writing"
    assert e.value.frame == 1
    assert len(e.value.completed) == 1
    held[0].exit()
    assert records_in(out) == [e.value.completed[0]["pathname"]]


# --- Arrays.


def field(frames=2, ny=3, nx=4):
    return np.arange(frames * ny * nx, dtype=np.float32).reshape(frames, ny, nx) * 0.25


ARRAY = dict(transform=(2000.0, 2000.0, 8000.0, -2000.0), crs="SHG", variable="Precip", source_units="mm", parts=PARTS)


@pytest.mark.parametrize("version", [7, 6])
def test_arrays_stamp_type_and_cells(tmp_path, version):
    out = tmp_path / "o.dss"
    r = gridded.import_arrays(field(), out, times=[(JUNE_14_23, JUNE_14_23 + HOUR), (JUNE_14_23 + HOUR, JUNE_14_23 + 2 * HOUR)],
                              version=version, **ARRAY)
    a, b = "/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/", "/SHG/P46/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/CI/"
    assert r.pathnames == [a, b]
    assert [x["data_type"] for x in r.records] == ["PER-CUM", "PER-CUM"]
    # South-up on disk; each frame under its own window.
    assert np.array_equal(cells_of(out, a).reshape(3, 4), field()[0][::-1])
    assert np.array_equal(cells_of(out, b).reshape(3, 4), field()[1][::-1])


# --- Stamps and units per variable, stated literally and against what Vortex wrote.
# Precipitation with no period is the AORC record that once gave HEC-HMS no rain.


def expected_cases():
    return json.loads((P46 / "expected.json").read_text(encoding="utf-8"))["cases"]


def run_expected(case, out, version=7):
    w = case["write"]
    parts = {p: w[f"part{p}"] for p in "ABCF" if f"part{p}" in w}
    e = case["envelope"]
    return gridded.import_gridded(
        [P46 / i for i in case["inputs"]], out, variable=case["variable"], method=case["method"],
        envelope=(e[0], e[2], e[1], e[3]), envelope_crs=case["envelope_wkt"], cell_size=case["cell"],
        parts=parts, units=w.get("units"), data_type=w.get("dataType"), version=version,
    )


def stamps_units_types(path):
    with dss.open(path) as f:
        out = {}
        for name in f.catalog()["pathname"].tolist():
            g = f.read_grid(name, missing="raw")
            out[str(name)] = (g.data_units, int(g.data_type), str(g.start_time), str(g.end_time))
        return out


@pytest.mark.parametrize("case", expected_cases(), ids=lambda c: c["id"])
def test_every_fixture_record_has_vortexs_stamps_units_and_type(tmp_path, case):
    out = tmp_path / "o.dss"
    run_expected(case, out)
    want = {r["pathname"]: (r["data_units"], r["data_type"]) for r in case["records"]}
    got = stamps_units_types(out)
    assert want, "the reference holds no records"
    assert sorted(want) == sorted(got)
    for name, (units, data_type) in want.items():
        assert got[name][:2] == (units, data_type), name
        d_part, e_part = name.split("/")[4:6]
        # D and E as the header reads them, by this test's own arithmetic.
        assert _part_time(e_part or d_part) == dt.datetime.fromisoformat(got[name][3]), name
        if e_part:
            assert _part_time(d_part) == dt.datetime.fromisoformat(got[name][2]), name


_MONTHS = "JAN FEB MAR APR MAY JUN JUL AUG SEP OCT NOV DEC".split()


def _part_time(text):
    day, clock = text.split(":")
    base = dt.datetime(int(day[5:]), _MONTHS.index(day[2:5]) + 1, int(day[:2]))
    return base + dt.timedelta(hours=int(clock[:2]), minutes=int(clock[2:]))


@pytest.mark.parametrize(
    "variable, units, data_type, d_part, e_part",
    [
        # Vortex 1.1.0 and 0.13.3 on HEC's AORC_APCP_MARFC_1984010100.nc4 and its TMP: this window and
        # these units. The stamp T = 1984-01-01 00:00 is the end of the hour.
        ("APCP_surface", "MM", 1, "31DEC1983:2300", "31DEC1983:2400"),
        ("TMP_2maboveground", "DEG C", 2, "31DEC1983:2400", ""),
    ],
)
def test_aorc_shaped_arrays_get_the_records_vortex_writes(tmp_path, variable, units, data_type, d_part, e_part):
    t = dt.datetime(1984, 1, 1)
    is_rain = variable.startswith("APCP")
    out = tmp_path / "o.dss"
    r = gridded.import_arrays(
        field()[:1], out, times=[(t - HOUR, t) if is_rain else t], transform=(2000.0, 2000.0, 8000.0, -2000.0), crs="SHG",
        variable=variable, source_units="kg/m^2" if is_rain else "K", parts={"A": "SHG", "B": "AORC", "F": "RUN"},
    )
    c_part = "PRECIPITATION" if is_rain else "TEMPERATURE"
    assert r.pathnames == [f"/SHG/AORC/{c_part}/{d_part}/{e_part}/RUN/"]
    with dss.open(out) as f:
        g = f.read_grid(r.pathnames[0], missing="raw")
    assert (g.data_units, int(g.data_type)) == (units, data_type)
    if is_rain:
        assert (str(g.start_time), str(g.end_time)) == ("1983-12-31T23:00:00", "1984-01-01T00:00:00")


@pytest.mark.parametrize("version", [7, 6])
def test_precipitation_at_an_instant_is_refused_and_leaves_nothing(tmp_path, version):
    out = tmp_path / "o.dss"
    t = dt.datetime(1984, 1, 1)
    with pytest.raises(DssUnsupportedRecordError, match="precipitation at an instant") as e:
        gridded.import_arrays(field()[:1], out, times=[t], transform=(2000.0, 2000.0, 8000.0, -2000.0), crs="SHG",
                              variable="APCP_surface", source_units="kg/m^2", version=version)
    assert "Observed behaviour we did not implement" in str(e.value)
    assert not out.exists()


def test_a_stated_instant_type_gives_vortexs_record(tmp_path):
    out = tmp_path / "o.dss"
    r = gridded.import_arrays(field()[:1], out, times=[dt.datetime(1984, 1, 1)], transform=(2000.0, 2000.0, 8000.0, -2000.0),
                              crs="SHG", variable="APCP_surface", source_units="kg/m^2", data_type="INST-VAL",
                              parts={"A": "SHG", "B": "AORC", "F": "RUN"})
    assert r.pathnames == ["/SHG/AORC/PRECIPITATION/31DEC1983:2400//RUN/"]


@pytest.mark.parametrize("data_type", ["PER-CUM", "PER-AVER"])
def test_a_zero_length_period_is_refused(tmp_path, data_type):
    out = tmp_path / "o.dss"
    t = dt.datetime(1984, 1, 1)
    with pytest.raises(DssUnsupportedRecordError, match="a period type needs a period"):
        gridded.import_arrays(field()[:1], out, times=[(t, t)], transform=(2000.0, 2000.0, 8000.0, -2000.0), crs="SHG",
                              variable="APCP_surface", source_units="kg/m^2", data_type=data_type)
    assert not out.exists()


def test_a_period_ending_before_it_starts_is_refused(tmp_path):
    out = tmp_path / "o.dss"
    t = dt.datetime(1984, 1, 1)
    with pytest.raises((DssUnsupportedRecordError, ValueError)):
        gridded.import_arrays(field()[:1], out, times=[(t, t - HOUR)], transform=(2000.0, 2000.0, 8000.0, -2000.0), crs="SHG",
                              variable="APCP_surface", source_units="kg/m^2")
    assert not out.exists()


def test_cells_are_stored_under_their_own_window(tmp_path):
    # Identity: each frame's own cells under each frame's own stamp, per variable.
    out = tmp_path / "o.dss"
    r = gridded.import_gridded(inputs("apcp_a.grib2", "apcp_b.grib2"), out, variable=APCP, parts=PARTS, **BOX)
    alone = tmp_path / "b.dss"
    gridded.import_gridded(inputs("apcp_b.grib2"), alone, variable=APCP, parts=PARTS, **BOX)
    name = "/SHG/P46/PRECIPITATION/15JUN2024:0000/15JUN2024:0100/CI/"
    assert name in r.pathnames
    assert np.array_equal(cells_of(out, name), cells_of(alone, name))
    other = "/SHG/P46/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/"
    assert not np.array_equal(cells_of(out, other), cells_of(out, name))


def test_a_failed_import_leaves_an_existing_file_byte_identical(tmp_path):
    out = tmp_path / "o.dss"
    gridded.import_gridded(inputs("apcp_a.grib2"), out, variable=APCP, parts=PARTS, **BOX)
    before = out.read_bytes()
    with pytest.raises(DssUnsupportedRecordError):
        gridded.import_gridded(inputs("apcp_a.grib2"), out, variable="nothing", parts=PARTS, **BOX)
    with pytest.raises(DssFormatError):
        gridded.import_gridded(inputs("apcp_a.grib2"), out, variable=APCP, parts=PARTS, **{**BOX, "envelope_crs": "not a crs"})
    with pytest.raises(DssUnsupportedRecordError, match="version"):
        gridded.import_gridded(inputs("apcp_a.grib2"), out, variable=APCP, parts=PARTS, version=6, **BOX)
    assert out.read_bytes() == before


def test_a_grid_over_the_limit_is_refused_before_any_copy(tmp_path):
    # Read-only and costing nothing here. It must not reach the copy on any platform.
    huge = np.broadcast_to(np.float32(1.0), (2 ** 40,))
    with pytest.raises(DssUnsupportedRecordError, match="cells in a source grid"):
        dss._native.import_arrays(
            huge, 2 ** 20, 2 ** 20, (0.0, 2000.0, 0.0, -2000.0), "SHG", float("nan"), [(0, 3600)], "Precip", None, "mm",
            str(tmp_path / "o.dss"),
        )
    assert not (tmp_path / "o.dss").exists()


def test_more_frames_than_the_limit_are_refused_before_any_copy(tmp_path):
    frames = dss._native.gridded_limits()["max_records"] + 1
    huge = np.broadcast_to(np.float32(1.0), (frames,))
    with pytest.raises(DssUnsupportedRecordError, match="records"):
        dss._native.import_arrays(
            huge, 1, 1, (0.0, 2000.0, 0.0, -2000.0), "SHG", float("nan"), [(0, 3600)] * frames, "Precip", None, "mm",
            str(tmp_path / "o.dss"),
        )
    assert not (tmp_path / "o.dss").exists()


@pytest.mark.skipif(sys.platform == "darwin", reason="macOS grants the allocation and kills the process filling it")
def test_an_array_too_large_to_copy_is_a_memory_error_and_not_an_abort(tmp_path):
    # Within both limits, and read-only here: 2**14 frames of 2**26 cells. An abort would end pytest.
    huge = np.broadcast_to(np.float32(1.0), (2 ** 40,))
    with pytest.raises(MemoryError):
        dss._native.import_arrays(
            huge, 2 ** 13, 2 ** 13, (0.0, 2000.0, 0.0, -2000.0), "SHG", float("nan"), [(0, 3600)] * 2 ** 14, "Precip",
            None, "mm", str(tmp_path / "o.dss"),
        )
    assert not (tmp_path / "o.dss").exists()


def test_array_conversions_follow_the_table(tmp_path):
    k = np.full((1, 3, 4), 300.0, dtype=np.float32)
    out = tmp_path / "k.dss"
    r = gridded.import_arrays(k, out, times=[JUNE_14_23], transform=ARRAY["transform"], crs="SHG",
                              variable="Temperature", source_units="K", parts=PARTS)
    assert (r.records[0]["units"], r.records[0]["data_type"]) == ("DEG C", "INST-VAL")
    assert cells_of(out, r.pathnames[0]).ravel()[0] == np.float32(300.0) + np.float32(-273.15)
    # A rate over two hours is depth.
    rate = np.full((1, 3, 4), 2.0, dtype=np.float32)
    out = tmp_path / "r.dss"
    r = gridded.import_arrays(rate, out, times=[(JUNE_14_23, JUNE_14_23 + 2 * HOUR)], transform=ARRAY["transform"],
                              crs="SHG", variable="Precip", source_units="mm/hr", parts=PARTS)
    assert r.records[0]["units"] == "MM"
    assert cells_of(out, r.pathnames[0]).ravel()[0] == 4.0


def test_array_missing_markers_become_hec_undefined(tmp_path):
    a = field(1)
    a[0, 0, 0], a[0, 0, 1], a[0, 0, 2] = -9999.0, np.nan, np.inf
    out = tmp_path / "o.dss"
    r = gridded.import_arrays(a, out, times=[(JUNE_14_23, JUNE_14_23 + HOUR)], nodata=-9999.0, **ARRAY)
    stored = cells_of(out, r.pathnames[0]).reshape(3, 4)[::-1]
    assert (stored[0, :3] == UNDEF).all()
    masked = np.ma.masked_equal(field(1), 0.25)
    out = tmp_path / "m.dss"
    r = gridded.import_arrays(masked, out, times=[(JUNE_14_23, JUNE_14_23 + HOUR)], **ARRAY)
    assert cells_of(out, r.pathnames[0]).reshape(3, 4)[::-1][0, 1] == UNDEF


@pytest.mark.parametrize(
    "change, error, text",
    [
        (dict(times=[]), ValueError, "time"),
        (dict(data=np.zeros((2, 3), dtype=np.float32)), ValueError, "time"),
        (dict(data=np.zeros(5, dtype=np.float32)), ValueError, "2-D or 3-D"),
        (dict(data=np.array([["a"]])), ValueError, "numeric"),
        (dict(data=np.full((1, 3, 4), 1e300), times=[JUNE_14_23]), ValueError, "overflow"),
        (dict(nodata=float("inf")), ValueError, "nodata"),
        (dict(transform=(1.0, 2.0)), ValueError, "transform"),
        (dict(data_type="PER-CUM", times=[JUNE_14_23, JUNE_14_23 + HOUR]), DssUnsupportedRecordError, "needs a period"),
        (dict(crs="nonsense"), DssFormatError, "WKT"),
        (dict(variable=""), DssUnsupportedRecordError, "variable"),
        (dict(source_units="furlongs"), DssUnsupportedRecordError, "reviewed"),
        (dict(times=[(dt.datetime(2024, 1, 1, 0, 0, 30), dt.datetime(2024, 1, 1, 1, 0, 30)), (JUNE_14_23, JUNE_14_23 + HOUR)]), DssUnsupportedRecordError, "whole minute"),
        (dict(times=[(JUNE_14_23, JUNE_14_23 + HOUR)] * 2), DssUnsupportedRecordError, "twice"),
        (dict(times=[(JUNE_14_23, JUNE_14_23 - HOUR), JUNE_14_23]), (ValueError, DssUnsupportedRecordError), "."),
    ],
)
def test_bad_arrays_are_typed_and_create_nothing(tmp_path, change, error, text):
    out = tmp_path / "o.dss"
    args = {"times": [JUNE_14_23, JUNE_14_23 + HOUR], **ARRAY, **change}
    data = args.pop("data", field())
    with pytest.raises(error, match=text):
        gridded.import_arrays(data, out, **args)
    assert not out.exists()


def test_arrays_go_through_the_warp(tmp_path):
    lon = np.arange(60, dtype=np.float32) + np.arange(40, dtype=np.float32)[:, None] * 0.5
    out = tmp_path / "o.dss"
    r = gridded.import_arrays(lon[None], out, times=[(JUNE_14_23, JUNE_14_23 + HOUR)], transform=(-90.305, 0.01, 35.205, -0.01),
                              crs=GEO_WKT, variable="Precip", source_units="mm", target_crs="SHG", parts=PARTS, **BOX)
    rec = r.records[0]
    assert (rec["nx"], rec["ny"], rec["cell_size"]) == (20, 14, 2000.0)


# --- The process.


def test_the_gil_is_released_during_an_import(tmp_path):
    a = np.random.default_rng(1).random((20, 1500, 1500), dtype=np.float32)
    times = [(JUNE_14_23 + i * HOUR, JUNE_14_23 + (i + 1) * HOUR) for i in range(20)]
    out = tmp_path / "o.dss"
    stop = threading.Event()
    ticks = []

    def count():
        while not stop.is_set():
            ticks.append(1)
            stop.wait(0.001)

    t = threading.Thread(target=count)
    interval = sys.getswitchinterval()
    sys.setswitchinterval(30.0)  # A thread would wait for the import if it held the GIL.
    try:
        t.start()
        gridded.import_arrays(a, out, times=times, sync=False, **ARRAY)
        during = len(ticks)
    finally:
        sys.setswitchinterval(interval)
        stop.set()
        t.join()
    assert during > 3


def test_no_java_in_the_process(tmp_path):
    gridded.import_gridded(inputs("apcp_a.grib2"), tmp_path / "o.dss", variable=APCP, parts=PARTS, **BOX)
    assert not {"jpype", "jnius", "py4j", "pyjnius"} & set(sys.modules)
    names = loaded_modules()
    if names is None:
        pytest.skip("no way to list loaded modules on this platform")
    assert names, "the listing must not be empty"
    assert not [n for n in names if "jvm" in n.lower() or "javaw" in n.lower() or "libjava" in n.lower()]


def loaded_modules():
    if sys.platform == "win32":
        import ctypes
        from ctypes import wintypes

        psapi = ctypes.WinDLL("psapi")
        kernel = ctypes.WinDLL("kernel32")
        kernel.GetCurrentProcess.restype = wintypes.HANDLE
        psapi.EnumProcessModules.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
        psapi.GetModuleFileNameExW.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.LPWSTR, wintypes.DWORD]
        handles = (ctypes.c_void_p * 4096)()
        needed = wintypes.DWORD()
        me = kernel.GetCurrentProcess()
        if not psapi.EnumProcessModules(me, handles, ctypes.sizeof(handles), ctypes.byref(needed)):
            return None
        out = []
        for h in handles[: needed.value // ctypes.sizeof(ctypes.c_void_p)]:
            buf = ctypes.create_unicode_buffer(1024)
            psapi.GetModuleFileNameExW(me, h, buf, 1024)
            out.append(buf.value)
        return out
    maps = Path("/proc/self/maps")
    if maps.exists():
        return [line.split()[-1] for line in maps.read_text().splitlines() if "/" in line]
    return None


def test_fixture_is_what_the_readme_lists():
    expected = json.loads((P46 / "expected.json").read_text(encoding="utf-8"))
    assert expected["source"] == "Vortex 1.1.0 BatchImporter"
    assert len(expected["cases"]) == 22
    assert sum(len(c["records"]) for c in expected["cases"]) == 40
    # Every file is listed with its own digest, and nothing is listed that is not there.
    readme = (FIXTURES / "README.md").read_text(encoding="utf-8")
    listed = {}
    for line in readme.splitlines():
        cells = [c.strip(" `") for c in line.split("|")]
        if len(cells) > 5 and cells[1].startswith("vortex_p46/"):
            listed[cells[1].split("/", 1)[1]] = cells[5]
    on_disk = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in P46.iterdir()}
    assert on_disk and listed == on_disk


# Findings of the independent review of 2026-11-02, each through the public API.

SIGNS = {
    "north_up": lambda a: (a, (2000.0, 2000.0, 12000.0, -2000.0)),
    "south_up": lambda a: (a[..., ::-1, :], (2000.0, 2000.0, 6000.0, 2000.0)),
    "negative_dx": lambda a: (a[..., :, ::-1], (10000.0, -2000.0, 12000.0, -2000.0)),
    "negative_dx_south_up": lambda a: (a[..., ::-1, ::-1], (10000.0, -2000.0, 6000.0, 2000.0)),
}
ONE_HOUR = [(JUNE_14_23, JUNE_14_23 + HOUR)]


def stored_grid(out, pathname):
    with dss.open(out) as f:
        return f.read_grid(pathname, missing="raw")


def sign_args(transform):
    return {**ARRAY, "transform": transform}


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("align", ["refuse", "vortex"])
@pytest.mark.parametrize("sign", ["negative_dx", "negative_dx_south_up"])
def test_negative_dx_is_refused_when_not_warped(tmp_path, version, align, sign):
    data, transform = SIGNS[sign](field(1))
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="columns must run west to east") as e:
        gridded.import_arrays(data, out, times=ONE_HOUR, align=align, version=version, **sign_args(transform))
    assert e.value.phase == "planning"
    assert not out.exists()


@pytest.mark.parametrize("version", [7, 6])
def test_south_up_unwarped_is_placed_as_north_up(tmp_path, version):
    results = []
    for sign in ("north_up", "south_up"):
        data, transform = SIGNS[sign](field(1))
        out = tmp_path / f"{sign}.dss"
        r = gridded.import_arrays(data, out, times=ONE_HOUR, version=version, **sign_args(transform))
        g = stored_grid(out, r.pathnames[0])
        results.append((g.lower_left_cell_x, g.lower_left_cell_y, g.cell_size, g.data.tobytes()))
    assert results[0] == results[1]
    # Control on the expected place: x from 2000 and y from 6000, with row 0 of the input at the top.
    assert results[0][:3] == (1, 3, 2000.0)
    stored = np.frombuffer(results[0][3], dtype=np.float32).reshape(3, 4)
    assert (stored == field(1)[0][::-1]).all()


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("method", ["near", "bilinear", "average"])
def test_every_sign_warps_to_the_same_place_and_cells(tmp_path, version, method):
    """A warp reads each cell through the transform, so a mirrored description is the same grid."""
    results = {}
    for sign, describe in SIGNS.items():
        data, transform = describe(field(1))
        out = tmp_path / f"{sign}.dss"
        r = gridded.import_arrays(
            data, out, times=ONE_HOUR, version=version, cell_size=2000.0, method=method, **sign_args(transform)
        )
        g = stored_grid(out, r.pathnames[0])
        results[sign] = (g.lower_left_cell_x, g.lower_left_cell_y, g.cell_size, g.data.shape, g.data.tobytes())
    assert len(set(results.values())) == 1, {k: v[:4] for k, v in results.items()}
    # Control: a grid that really is mirrored does not come out the same.
    out = tmp_path / "mirrored.dss"
    r = gridded.import_arrays(
        field(1)[..., :, ::-1], out, times=ONE_HOUR, version=version, cell_size=2000.0, method=method,
        **sign_args(SIGNS["north_up"](field(1))[1]),
    )
    assert stored_grid(out, r.pathnames[0]).data.tobytes() != results["north_up"][4]


def test_a_rotated_transform_has_no_route_in_arrays(tmp_path):
    # Rotation cannot be stated here; `import_pipeline.rs` holds the refusal for sources that state it.
    out = tmp_path / "o.dss"
    with pytest.raises(ValueError, match="transform must be"):
        gridded.import_arrays(field(1), out, times=ONE_HOUR, **{**ARRAY, "transform": (0, 1, 0, 0, 0, -1)})
    assert not out.exists()


@pytest.mark.parametrize("version", [7, 6])
def test_a_converted_cell_equal_to_the_source_marker_stays_a_value(tmp_path, version):
    cells = np.array([[[273.15, 274.15], [0.0, 275.15]]], dtype=np.float32)
    out = tmp_path / "t.dss"
    r = gridded.import_arrays(
        cells, out, times=ONE_HOUR, nodata=0.0, version=version,
        **{**ARRAY, "variable": "Temperature", "source_units": "K"},
    )
    g = stored_grid(out, r.pathnames[0])
    # South-up: the source's second row is first. Only the true marker cell is missing.
    assert g.data[0, 0] == UNDEF
    assert g.data[1, 0] == 0.0
    assert g.data[1, 1] == pytest.approx(1.0, abs=1e-4)
    assert g.data[0, 1] == pytest.approx(2.0, abs=1e-4)


@pytest.mark.parametrize("version", [7, 6])
def test_statistics_follow_the_corrected_mask(tmp_path, version):
    cells = np.array([[[1.0, 2.0], [1000.0, 4.0]]], dtype=np.float32)
    out = tmp_path / "p.dss"
    r = gridded.import_arrays(
        cells, out, times=ONE_HOUR, nodata=1000.0, version=version, **{**ARRAY, "source_units": "m"}
    )
    g = stored_grid(out, r.pathnames[0])
    assert g.data[0, 0] == UNDEF  # The source's own marker.
    assert sorted(g.data[g.data != UNDEF].tolist()) == [1000.0, 2000.0, 4000.0]
    assert g.mean_value == pytest.approx(7000.0 / 3.0)
    assert g.max_value == 4000.0
    assert g.min_value == 1000.0


@pytest.mark.parametrize("marker", [float("nan"), -9999.0])
def test_marker_cells_stay_missing_and_nan_is_missing_under_any_marker(tmp_path, marker):
    cells = np.array([[[1.0, np.nan], [marker, 2.0]]], dtype=np.float32)
    out = tmp_path / "n.dss"
    r = gridded.import_arrays(cells, out, times=ONE_HOUR, nodata=marker, **{**ARRAY, "source_units": "m"})
    g = stored_grid(out, r.pathnames[0])
    assert (g.data == np.array([[UNDEF, 2000.0], [1000.0, UNDEF]], dtype=np.float32)).all()


@pytest.mark.parametrize("version", [7, 6])
def test_a_warp_then_conversion_keeps_the_marker_collision_a_value(tmp_path, version):
    """The same collision through a warp: near copies 273.15 K, which converts to the marker 0."""
    cells = np.full((1, 4, 4), 280.0, dtype=np.float32)
    cells[0, 1, 2] = 273.15
    cells[0, 2, 1] = 0.0
    out = tmp_path / "w.dss"
    r = gridded.import_arrays(
        cells, out, times=ONE_HOUR, nodata=0.0, version=version, cell_size=2000.0, method="near",
        **{**ARRAY, "transform": (2000.0, 2000.0, 10000.0, -2000.0), "variable": "Temperature", "source_units": "K"},
    )
    g = stored_grid(out, r.pathnames[0])
    assert (g.data == UNDEF).sum() == 1
    assert (g.data == 0.0).sum() == 1
    assert g.data[1, 1] == UNDEF  # South-up: source row 2 is row 1.
    assert g.data[2, 2] == 0.0


CHECKER = np.where(np.indices((8, 8)).sum(axis=0) % 2 == 0, -1.0, 1.0).astype(np.float32)[np.newaxis]
HALF_CELL = {**ARRAY, "transform": (3000.0, 2000.0, 17000.0, -2000.0), "envelope": (6000.0, 4000.0, 14000.0, 14000.0)}


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("method", ["bilinear", "average"])
def test_a_warp_computing_the_marker_from_valid_cells_is_refused(tmp_path, version, method):
    """Half a cell off, each result is the mean of +1 and -1: exactly the marker 0."""
    out = tmp_path / "c.dss"
    with pytest.raises(DssUnsupportedRecordError, match="computed from valid data") as e:
        gridded.import_arrays(
            CHECKER, out, times=ONE_HOUR, nodata=0.0, version=version, cell_size=2000.0, method=method,
            envelope_crs="SHG", **HALF_CELL,
        )
    assert e.value.phase == "writing"
    assert e.value.completed == []
    assert not out.exists()
    # Control: a marker the data cannot reach writes the same warp, with the zeros kept as values.
    r = gridded.import_arrays(
        CHECKER, out, times=ONE_HOUR, nodata=-9999.0, version=version, cell_size=2000.0, method=method,
        envelope_crs="SHG", **HALF_CELL,
    )
    assert (stored_grid(out, r.pathnames[0]).data == 0.0).all()


def test_the_warp_counts_no_collision_when_the_marker_is_nan(tmp_path):
    r = gridded.import_arrays(
        CHECKER, tmp_path / "c.dss", times=ONE_HOUR, nodata=float("nan"), cell_size=2000.0, method="bilinear",
        envelope_crs="SHG", **HALF_CELL,
    )
    assert (stored_grid(tmp_path / "c.dss", r.pathnames[0]).data == 0.0).all()


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize(
    "value, marker, units, variable",
    [
        (UNDEF, float("nan"), "mm", "Precip"),  # HEC's own marker as a valid cell.
        (UNDEF, -9999.0, "K", "Temperature"),  # It survives the addition.
        (np.float32(3.0e36), -9999.0, "m", "Precip"),  # Out of float32 after conversion.
    ],
)
def test_a_valid_cell_a_dss_grid_cannot_hold_is_refused_and_keeps_finished_records(
    tmp_path, version, value, marker, units, variable
):
    good = np.array([[1.0, 2.0], [3.0, 4.0]], dtype=np.float32)
    bad = good.copy()
    bad[0, 1] = value
    out = tmp_path / "u.dss"
    with pytest.raises(DssUnsupportedRecordError, match="cannot hold") as e:
        gridded.import_arrays(
            np.stack([good, bad]), out, times=[(JUNE_14_23 + i * HOUR, JUNE_14_23 + (i + 1) * HOUR) for i in range(2)],
            nodata=marker, version=version, **{**ARRAY, "source_units": units, "variable": variable},
        )
    assert e.value.phase == "writing"
    assert e.value.frame == 1
    assert len(e.value.completed) == 1
    assert records_in(out) == [e.value.completed[0]["pathname"]]


def test_hecs_marker_stated_as_nodata_is_missing(tmp_path):
    cells = np.array([[[UNDEF, 1.0], [2.0, 3.0]]], dtype=np.float32)
    r = gridded.import_arrays(cells, tmp_path / "o.dss", times=ONE_HOUR, nodata=float(UNDEF), **ARRAY)
    assert (stored_grid(tmp_path / "o.dss", r.pathnames[0]).data == UNDEF).sum() == 1


def test_a_wider_cell_rounding_onto_the_marker_is_refused(tmp_path):
    marker = float(np.float32(0.1))
    cells = np.array([[[marker + 1e-12, 1.0], [marker, 2.0]]], dtype=np.float64)
    with pytest.raises(ValueError, match="round to the nodata marker"):
        gridded.import_arrays(cells, tmp_path / "o.dss", times=ONE_HOUR, nodata=marker, **ARRAY)
    assert not (tmp_path / "o.dss").exists()
    # Control: the exact marker alone is missing.
    r = gridded.import_arrays(
        np.array([[[marker, 1.0], [3.0, 2.0]]]), tmp_path / "o.dss", times=ONE_HOUR, nodata=marker, **ARRAY
    )
    assert (stored_grid(tmp_path / "o.dss", r.pathnames[0]).data == UNDEF).sum() == 1


def test_a_marker_outside_float32_is_refused(tmp_path):
    with pytest.raises(ValueError, match="overflows float32"):
        gridded.import_arrays(field(1), tmp_path / "o.dss", times=ONE_HOUR, nodata=1e300, **ARRAY)


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize(
    "variable, part",
    [
        ("unknown_depth", "PRECIPITATION"),
        ("unknown_depth", "precipitation"),
        ("unknown_depth", "PRECIP-INC"),
        ("unknown_depth", "RAINFALL"),
        ("Precip", "MY-C"),
    ],
)
def test_the_final_c_part_cannot_bypass_the_instant_rule(tmp_path, version, variable, part):
    out = tmp_path / "o.dss"
    args = {**ARRAY, "variable": variable, "parts": {**PARTS, "C": part}}
    with pytest.raises(DssUnsupportedRecordError, match="precipitation at an instant") as e:
        gridded.import_arrays(field(1), out, times=[JUNE_14_23], version=version, **args)
    assert e.value.phase == "planning"
    assert not out.exists()
    # Stating the type is the one route, and it gives what was asked.
    r = gridded.import_arrays(field(1), out, times=[JUNE_14_23], version=version, data_type="INST-VAL", **args)
    assert r.records[0]["data_type"] == "INST-VAL"
    # A period needs no statement.
    r = gridded.import_arrays(field(1), tmp_path / "p.dss", times=ONE_HOUR, version=version, **args)
    assert r.records[0]["pathname"].split("/")[5] != ""


def test_a_c_part_that_is_not_precipitation_keeps_the_instant(tmp_path):
    args = {**ARRAY, "variable": "unknown_depth", "parts": {**PARTS, "C": "PRECIPITATION-FREQUENCY"}}
    r = gridded.import_arrays(field(1), tmp_path / "o.dss", times=[JUNE_14_23], **args)
    assert r.records[0]["data_type"] == "INST-VAL"


class _Stop(Exception):
    pass


def _raise_at(last):
    def callback(p):
        if p["done"] == last:
            raise _Stop(p["pathname"])

    return callback


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("frames", [1, 3])
def test_an_exception_in_the_last_callback_reaches_the_caller(tmp_path, version, frames):
    out = tmp_path / "o.dss"
    times = [(JUNE_14_23 + i * HOUR, JUNE_14_23 + (i + 1) * HOUR) for i in range(frames)]
    with pytest.raises(_Stop) as e:
        gridded.import_arrays(field(frames), out, times=times, version=version, progress=_raise_at(frames), **ARRAY)
    assert len(e.value.completed) == frames
    assert str(e.value) == e.value.completed[-1]["pathname"]
    # The records are written and whole.
    assert sorted(records_in(out)) == sorted(r["pathname"] for r in e.value.completed)


def test_an_exception_in_the_last_callback_of_a_file_import_reaches_the_caller(tmp_path):
    out = tmp_path / "o.dss"
    with pytest.raises(_Stop) as e:
        gridded.import_gridded(inputs("apcp_a.grib2"), out, variable=APCP, parts=PARTS, progress=_raise_at(1), **BOX)
    assert len(e.value.completed) == 1
    assert records_in(out) == [e.value.completed[0]["pathname"]]


def test_a_callback_that_does_not_raise_returns_the_records(tmp_path):
    seen = []
    r = gridded.import_arrays(field(1), tmp_path / "o.dss", times=ONE_HOUR, progress=seen.append, **ARRAY)
    assert len(r.records) == 1
    assert len(seen) == 1


def test_arrays_over_the_engines_limits_are_refused_before_any_copy(tmp_path):
    # Broadcast views cost nothing here. A float32 copy of the first alone would be 512 MiB.
    wide = np.broadcast_to(np.float64(1.0), (1, 2 ** 14, 2 ** 13))
    with pytest.raises(DssUnsupportedRecordError, match="source grid"):
        gridded.import_arrays(wide, tmp_path / "o.dss", times=ONE_HOUR, **ARRAY)
    many = np.broadcast_to(np.float64(1.0), (2 ** 20 + 1, 1, 1))
    with pytest.raises(DssUnsupportedRecordError, match="records"):
        gridded.import_arrays(many, tmp_path / "o.dss", times=[None] * (2 ** 20 + 1), **ARRAY)
    assert not (tmp_path / "o.dss").exists()


def test_a_masked_input_is_not_changed(tmp_path):
    data = field(1)
    before = data.copy()
    masked = np.ma.masked_equal(data, 0.25)
    gridded.import_arrays(masked, tmp_path / "o.dss", times=ONE_HOUR, nodata=-1.0, **ARRAY)
    assert (data == before).all()


RASTER = FIXTURES / "raster"
sys.path.insert(0, str(Path(__file__).resolve().parent))


def _sign_file(tmp_path, name, rows, origin_x, dx, origin_y, dy):
    import sign_tiff

    path = tmp_path / name
    sign_tiff.sign_tiff(path, rows, origin_x, dx, origin_y, dy)
    return path


def _sign_files(tmp_path):
    base = [[float(r * 4 + c + 1) for c in range(4)] for r in range(4)]
    return {
        "north_up": _sign_file(tmp_path, "n.tif", base, 1000000.0, 2000.0, 1008000.0, -2000.0),
        "south_up": _sign_file(tmp_path, "s.tif", base[::-1], 1000000.0, 2000.0, 1000000.0, 2000.0),
        "negative_dx": _sign_file(tmp_path, "w.tif", [r[::-1] for r in base], 1008000.0, -2000.0, 1008000.0, -2000.0),
        "negative_dx_south_up": _sign_file(
            tmp_path, "b.tif", [r[::-1] for r in base[::-1]], 1008000.0, -2000.0, 1000000.0, 2000.0
        ),
    }


@pytest.mark.parametrize("version", [7, 6])
def test_raster_files_of_each_sign_import_to_one_place(tmp_path, version):
    files = _sign_files(tmp_path)
    args = dict(variable="", parts={**PARTS, "C": "RASTER"}, version=version)
    as_is = dict(target_crs=None, cell_size=None)
    results = {}
    for name, path in files.items():
        out = tmp_path / f"{name}.dss"
        r = gridded.import_gridded([path], out, cell_size=2000.0, target_crs="SHG", method="near", **args)
        g = stored_grid(out, r.pathnames[0])
        results[name] = (g.lower_left_cell_x, g.lower_left_cell_y, g.data.tobytes())
    assert len(set(results.values())) == 1
    assert results["north_up"][:2] == (500, 500)
    # Unwarped, north-up and south-up are stored the same, and negative dx is refused and writes nothing.
    stored = {}
    for name in ("north_up", "south_up"):
        out = tmp_path / f"{name}_asis.dss"
        r = gridded.import_gridded([files[name]], out, **as_is, **args)
        g = stored_grid(out, r.pathnames[0])
        stored[name] = (g.lower_left_cell_x, g.lower_left_cell_y, g.data.tobytes())
    assert stored["north_up"] == stored["south_up"] == results["north_up"]
    for name in ("negative_dx", "negative_dx_south_up"):
        out = tmp_path / f"{name}_asis.dss"
        with pytest.raises(DssUnsupportedRecordError, match="columns must run west to east") as e:
            gridded.import_gridded([files[name]], out, **as_is, **args)
        assert e.value.phase == "planning"
        assert not out.exists()


def test_a_rotated_raster_is_refused_warped_or_not(tmp_path):
    for extra in ({"cell_size": None, "target_crs": None}, {"cell_size": 2000.0, "target_crs": "SHG"}):
        out = tmp_path / "o.dss"
        with pytest.raises(DssUnsupportedRecordError, match="rotated") as e:
            gridded.import_gridded(
                [RASTER / "geo_rotated.tif"], out, variable="", parts={**PARTS, "C": "RASTER"}, **extra
            )
        assert e.value.phase == "planning"
        assert not out.exists()


def test_a_raster_with_no_name_needs_a_c_part_and_an_ascii_grid_is_read(tmp_path):
    with pytest.raises(DssUnsupportedRecordError, match="state the C part"):
        gridded.import_gridded([RASTER / "crs_5070.tif"], tmp_path / "o.dss", variable="", parts=PARTS)
    assert not (tmp_path / "o.dss").exists()
    r = gridded.import_gridded(
        [RASTER / "p41_v1_1_0.asc"], tmp_path / "a.dss", variable="", parts={**PARTS, "C": "RASTER"}
    )
    assert len(r.records) == 1
    assert gridded.list_variables(RASTER / "p41_v1_1_0.asc") == [""]


def test_text_named_as_an_ascii_grid_is_typed(tmp_path):
    junk = tmp_path / "junk.asc"
    junk.write_text("this is no grid")
    with pytest.raises((DssFormatError, DssUnsupportedRecordError)):
        gridded.import_gridded([junk], tmp_path / "o.dss", variable="", parts={**PARTS, "C": "RASTER"})
    assert not (tmp_path / "o.dss").exists()


def test_a_raster_precipitation_at_an_instant_needs_a_stated_type(tmp_path):
    """The rule is the importer's, so a raster's name reaches it as a GRIB2 variable's does."""
    instant = tmp_path / "2000-01-01T0000.tif"
    shutil.copy(RASTER / "2000-01-01T0000.tif", instant)
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="precipitation at an instant"):
        gridded.import_gridded([instant], out, variable="", parts={**PARTS, "C": "PRECIPITATION"})
    assert not out.exists()
    r = gridded.import_gridded([instant], out, variable="", parts={**PARTS, "C": "PRECIPITATION"}, data_type="INST-VAL")
    assert r.records[0]["data_type"] == "INST-VAL"


def masked(path, pathname):
    with dss.open(path) as f:
        return np.asarray(f.read_grid(pathname).data, dtype=np.float32).copy()


@pytest.mark.parametrize("version", [7, 6])
def test_a_raster_with_no_marker_imports_under_vortexs_default(tmp_path, version):
    """Vortex's reader gives a file with no marker -9999 (`AscDataReader.getDto`): a warp fills what the source does
    not reach with it, and it reads as missing. The same file stating -9999 must give the same cells."""
    src = RASTER / "nodata_none.tif"
    kw = dict(variable="", parts={**PARTS, "C": "RASTER"}, target_crs="SHG", cell_size=2000, align="vortex", version=version)
    plain = gridded.import_gridded([src], tmp_path / "plain.dss", **kw).records[0]
    x, y = plain["lower_left_cell_x"] * 2000.0, plain["lower_left_cell_y"] * 2000.0
    beyond = (x - 6000.0, y - 6000.0, x + (plain["nx"] + 3) * 2000.0, y + (plain["ny"] + 3) * 2000.0)
    out = tmp_path / "beyond.dss"
    r = gridded.import_gridded([src], out, envelope=beyond, envelope_crs="SHG", **kw).records[0]
    assert (r["nx"], r["ny"]) == (plain["nx"] + 6, plain["ny"] + 6)
    cells = masked(out, r["pathname"])
    assert np.isnan(cells[:3]).all() and np.isnan(cells[-3:]).all()
    assert np.isnan(cells[:, :3]).all() and np.isnan(cells[:, -3:]).all()
    held = masked(tmp_path / "plain.dss", plain["pathname"])
    np.testing.assert_array_equal(cells[3:-3, 3:-3], held)
    assert np.isfinite(held).any()


def test_a_raster_with_no_marker_loses_only_minus_9999_as_it_stands(tmp_path):
    src = tmp_path / "m.asc"
    src.write_text("ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 2000\n1 -9999\n3 4\n", encoding="ascii")
    shutil.copy(RASTER / "p41_v1_1_0.prj", tmp_path / "m.prj")
    out = tmp_path / "o.dss"
    r = gridded.import_gridded([src], out, variable="", parts={**PARTS, "C": "RASTER"}, target_crs=None, cell_size=None, align="vortex").records[0]
    cells = masked(out, r["pathname"])
    assert np.isnan(cells).sum() == 1
    assert sorted(cells[np.isfinite(cells)].tolist()) == [1.0, 3.0, 4.0]


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("name, record_type", [("tmin_c.tif", 430), ("crs_5070.tif", 420), ("test_albers.tiff", 420),
                                               ("TrinityXMRG2DSS.tiff", 420), ("crs_4326.tif", 430)])
def test_a_rasters_record_form_follows_gdals_name_for_its_crs(tmp_path, version, name, record_type):
    """`DssUtil.getGridInfo` reads the name. tmin_c.tif states exactly SHG's numbers, and GDAL calls it `unnamed`."""
    out = tmp_path / "o.dss"
    r = gridded.import_gridded([RASTER / name], out, variable=gridded.list_variables(RASTER / name)[0],
                               parts={**PARTS, "C": "RASTER"}, align="vortex", target_crs=None, cell_size=None, version=version).records[0]
    assert r["record_type"] == record_type

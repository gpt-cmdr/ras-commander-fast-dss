"""``import_gridded(window_rule="accumulation:<duration>")`` through the public API (packet P46c).

Inputs are the formula-made MRMS message in ``fixtures/public/vortex_p46``, with its parameter patched.
Expected stamps are literals. Real samples, Vortex and HEC's readers are in ``oracle/vortex/run_window.py``.
"""

import struct
import warnings

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssUnsupportedRecordError, gridded

from conftest import FIXTURES

P46 = FIXTURES / "vortex_p46"
FIXTURE = P46 / "MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240615-000000.grib2"
V24 = "MultiSensor_QPE_24H_Pass2_altitude_above_msl"
V15 = "RadarOnly_QPE_15M_altitude_above_msl"
GEO_WKT = (
    'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433]]'
)
BOX = dict(envelope=(-90.2, 34.9, -89.8, 35.1), envelope_crs=GEO_WKT, cell_size=2000.0)
PARTS = {"A": "SHG", "B": "P46C", "F": "CI"}


def message(number, day=15, hour=0):
    b = bytearray(FIXTURE.read_bytes())
    b[16 + 15] = day
    b[16 + 16] = hour
    at = 16
    while True:
        length = struct.unpack(">I", b[at : at + 4])[0]
        if b[at + 4] == 4:
            b[at + 10] = number
            return bytes(b)
        at += length


def written(tmp_path, name, number=41, day=15, hour=0):
    p = tmp_path / name
    p.write_bytes(message(number, day, hour))
    return p


def test_feature_flag():
    assert "gridded_accumulation_window" in dss.FEATURES


def test_a_24h_file_gets_its_period_and_a_record_says_who_stated_it(tmp_path):
    out = tmp_path / "o.dss"
    r = gridded.import_gridded(
        written(tmp_path, "x.grib2"), out, variable=V24, parts=PARTS, window_rule="accumulation:24H", **BOX
    )
    # A period ending at midnight states 2400 of the day before.
    name = "/SHG/P46C/PRECIPITATION/14JUN2024:0000/14JUN2024:2400/CI/"
    assert r.pathnames == [name]
    first = r.records[0]
    assert first["data_type"] == "PER-CUM"
    assert first["accumulation_seconds"] == 86400
    assert first["accumulation_stated_by"] == "variable"
    assert first["overlaps_earlier"] is False
    assert first["gap_before"] is False
    with dss.open(out) as f:
        g = f.read_grid(name, missing="raw")
    assert str(g.start_time) == "2024-06-14T00:00:00"
    assert str(g.end_time) == "2024-06-15T00:00:00"


def test_other_rules_leave_the_new_fields_empty(tmp_path):
    r = gridded.import_gridded(
        written(tmp_path, "x.grib2"), tmp_path / "o.dss", variable=V24, parts=PARTS, data_type="INST-VAL", **BOX
    )
    for key in ("accumulation_seconds", "accumulation_stated_by", "overlaps_earlier", "gap_before"):
        assert r.records[0][key] is None


@pytest.mark.parametrize(
    "name, number, variable, rule, text",
    [
        ("MRMS_QPE_01H.grib2", 41, V24, "accumulation:01H", "variable"),
        ("MRMS_QPE_01H.grib2", 41, V24, "accumulation:24H", "file name"),
        ("x.grib2", 45, V15, "accumulation:01H", "variable"),
        ("x.grib2", 1, "PrecipRate_altitude_above_msl", "accumulation:01H", "converted"),
        ("x.grib2", 0, "PrecipFlag_altitude_above_msl", "accumulation:01H", "not precipitation"),
    ],
)
def test_disagreement_is_refused_and_creates_nothing(tmp_path, name, number, variable, rule, text):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match=text) as e:
        gridded.import_gridded(written(tmp_path, name, number), out, variable=variable, window_rule=rule, **BOX)
    assert e.value.phase == "planning"
    assert not e.value.completed
    assert e.value.in_doubt is None
    assert not out.exists()


@pytest.mark.parametrize(
    "rule", ["accumulation:", "accumulation:24", "accumulation:24h", "accumulation:30M", "accumulation", "24H", ""]
)
def test_a_bad_rule_is_a_value_error_and_creates_nothing(tmp_path, rule):
    out = tmp_path / "o.dss"
    with pytest.raises(ValueError, match="window_rule"):
        gridded.import_gridded(written(tmp_path, "x.grib2"), out, variable=V24, window_rule=rule, **BOX)
    assert not out.exists()


def test_a_stated_type_other_than_per_cum_is_refused(tmp_path):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="PER-CUM"):
        gridded.import_gridded(
            written(tmp_path, "x.grib2"), out, variable=V24, window_rule="accumulation:24H", data_type="INST-VAL", **BOX
        )
    assert not out.exists()


def test_without_the_option_the_refusal_stands(tmp_path):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="precipitation at an instant"):
        gridded.import_gridded(written(tmp_path, "x.grib2"), out, variable=V24, **BOX)
    assert not out.exists()


def test_a_renamed_file_is_refused_by_default_naming_both_durations(tmp_path):
    out = tmp_path / "o.dss"
    source = written(tmp_path, "MRMS_MultiSensor_QPE_01H_Pass2_00.00_20240615-000000.grib2")
    with pytest.raises(DssUnsupportedRecordError, match=r"states 24H.*one-hour window \(01H\)") as e:
        gridded.import_gridded(source, out, variable=V24, parts=PARTS, **BOX)
    assert e.value.phase == "planning"
    assert not out.exists()
    r = gridded.import_gridded(source, out, variable=V24, parts=PARTS, window_rule="vortex", **BOX)
    assert r.pathnames == ["/SHG/P46C/PRECIPITATION/14JUN2024:2300/14JUN2024:2400/CI/"]


def test_rolling_files_are_all_stored_and_warn(tmp_path):
    out = tmp_path / "o.dss"
    inputs = [written(tmp_path, f"x{h}.grib2", hour=h) for h in (1, 2, 3)]
    with pytest.warns(RuntimeWarning, match="counts the shared rain more than once"):
        r = gridded.import_gridded(inputs, out, variable=V24, parts=PARTS, window_rule="accumulation:24H", **BOX)
    assert r.pathnames == [
        "/SHG/P46C/PRECIPITATION/14JUN2024:0100/15JUN2024:0100/CI/",
        "/SHG/P46C/PRECIPITATION/14JUN2024:0200/15JUN2024:0200/CI/",
        "/SHG/P46C/PRECIPITATION/14JUN2024:0300/15JUN2024:0300/CI/",
    ]
    assert [x["overlaps_earlier"] for x in r.records] == [False, True, True]
    with dss.open(out) as f:
        assert sorted(str(p) for p in f.catalog()["pathname"].tolist()) == sorted(r.pathnames)


def test_abutting_periods_do_not_warn_and_a_gap_does(tmp_path):
    inputs = [written(tmp_path, f"x{h}.grib2", 37, hour=h) for h in (1, 2)]
    variable = "MultiSensor_QPE_01H_Pass2_altitude_above_msl"
    kw = dict(variable=variable, parts=PARTS, window_rule="accumulation:01H", **BOX)
    with warnings.catch_warnings():
        warnings.simplefilter("error", RuntimeWarning)
        r = gridded.import_gridded(inputs, tmp_path / "a.dss", **kw)
    assert [x["gap_before"] for x in r.records] == [False, False]
    inputs.append(written(tmp_path, "x5.grib2", 37, hour=5))
    with pytest.warns(RuntimeWarning, match="leave a gap"):
        r = gridded.import_gridded(inputs, tmp_path / "b.dss", **kw)
    assert [x["gap_before"] for x in r.records] == [False, False, True]


def test_the_cells_are_those_of_the_instant_route(tmp_path):
    p = written(tmp_path, "x.grib2")
    a = gridded.import_gridded(p, tmp_path / "a.dss", variable=V24, parts=PARTS, window_rule="accumulation:24H", **BOX)
    b = gridded.import_gridded(p, tmp_path / "b.dss", variable=V24, parts=PARTS, data_type="INST-VAL", **BOX)
    assert b.pathnames == ["/SHG/P46C/PRECIPITATION/14JUN2024:2400//CI/"]
    with dss.open(tmp_path / "a.dss") as f:
        ga = np.asarray(f.read_grid(a.pathnames[0], missing="raw").data, dtype="<f4").copy()
    with dss.open(tmp_path / "b.dss") as f:
        gb = np.asarray(f.read_grid(b.pathnames[0], missing="raw").data, dtype="<f4").copy()
    assert ga.size > 0
    assert ga.tobytes() == gb.tobytes()


def test_a_raising_callback_reports_the_record_with_its_window(tmp_path):
    inputs = [written(tmp_path, "a.grib2", hour=1), written(tmp_path, "b.grib2", hour=2)]

    def stop(_):
        raise KeyError("stop")

    out = tmp_path / "o.dss"
    with pytest.raises(KeyError) as e:
        gridded.import_gridded(
            inputs, out, variable=V24, parts=PARTS, window_rule="accumulation:24H", progress=stop, **BOX
        )
    assert [r["pathname"] for r in e.value.completed] == ["/SHG/P46C/PRECIPITATION/14JUN2024:0100/15JUN2024:0100/CI/"]
    assert e.value.completed[0]["accumulation_stated_by"] == "variable"

# Review of 8a545c6. Real bytes are in ``oracle/vortex/run_window.py refusals``.
def flash(tmp_path, name, number):
    b = bytearray(FIXTURE.read_bytes())
    at = 16
    while True:
        length = struct.unpack(">I", b[at : at + 4])[0]
        if b[at + 4] == 4:
            b[at + 9] = 12
            b[at + 10] = number
            break
        at += length
    p = tmp_path / name
    p.write_bytes(bytes(b))
    return p


ARI24 = "FLASH_QPE_ARI24H_altitude_above_msl"


@pytest.mark.parametrize("extra", [{}, {"units": "MM"}, {"data_type": "PER-CUM"}])
def test_a_recurrence_interval_is_refused_under_a_stated_accumulation(tmp_path, extra):
    out = tmp_path / "o.dss"
    source = flash(tmp_path, "MRMS_FLASH_QPE_ARI24H_00.00_20240615-000000.grib2", 19)
    with pytest.raises(DssUnsupportedRecordError, match="not a rain depth") as e:
        gridded.import_gridded(source, out, variable=ARI24, window_rule="accumulation:24H", **extra, **BOX)
    assert e.value.phase == "planning"
    assert not out.exists()


@pytest.mark.parametrize("label", ["CFS", "MM/HR", "year", ""])
def test_a_label_that_is_not_a_depth_is_refused(tmp_path, label):
    out = tmp_path / "o.dss"
    with pytest.raises(DssUnsupportedRecordError, match="not a depth"):
        gridded.import_gridded(
            written(tmp_path, "x.grib2"), out, variable=V24, window_rule="accumulation:24H", units=label, **BOX
        )
    assert not out.exists()


def test_a_label_in_a_file_name_does_not_refuse_a_real_one_hour_file(tmp_path):
    variable = "MultiSensor_QPE_01H_Pass2_altitude_above_msl"
    source = written(tmp_path, "24H_RadarOnly_QPE_01H_x.grib2", 37)
    r = gridded.import_gridded(source, tmp_path / "o.dss", variable=variable, window_rule="accumulation:01H", **BOX)
    assert r.records[0]["accumulation_stated_by"] == "variable+file_name"


def test_the_default_refuses_a_recurrence_interval_and_vortex_keeps_it(tmp_path):
    out = tmp_path / "o.dss"
    source = flash(tmp_path, "MRMS_MultiSensor_QPE_ARI01H_00.00_20240615-000000.grib2", 15)
    variable = "FLASH_QPE_ARI01H_altitude_above_msl"
    for extra in ({}, {"data_type": "INST-VAL"}):
        with pytest.raises(DssUnsupportedRecordError, match="not a rain depth"):
            gridded.import_gridded(source, out, variable=variable, **extra, **BOX)
        assert not out.exists()
    r = gridded.import_gridded(source, out, variable=variable, window_rule="vortex", data_type="INST-VAL", **BOX)
    assert r.records[0]["units"] == "year"

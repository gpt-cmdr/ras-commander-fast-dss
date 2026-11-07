"""P25: pattern time-origin metadata (DSS_TASK DT5 follow-up); public/synthetic data only.

The first value of a regular pattern is at 01JAN3000 00:00 on both DSS versions; the stored
offset is reported raw and never applied. ``pattern_heclib_first_seconds`` is the start heclib
C ``ztsRetrievePattern`` reports. For ``pattern_offset7.dss`` the expected starts below were
verified against pydsstools 3.1.0, Monolith 3.3.27 ``startTime`` and hecdss 0.1.33
(``start_date`` + one interval); see evidence/P25/REPORT.md.
"""

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss

from conftest import fixture_path

KEYS = (
    "pattern_time_convention",
    "pattern_stored_offset_seconds",
    "pattern_stored_offset_source",
    "pattern_heclib_first_seconds",
)
CONVENTION = "first-value-at-3000-01-01T00:00"
EPOCH3000 = 34_712_755_200  # HEC seconds of 01JAN3000 00:00
T3000 = pd.Timestamp("3000-01-01 00:00")

# pathname, interval seconds, stored INT_HEAD_timeOffset, heclib first (seconds after 3000).
OFFSET7 = [
    ("//P25 SYNTH/FLOW-UNIT GRAPH/TS-PATTERN/15Minute/OFFSET180/", 900, 180, 180),
    ("//P25 SYNTH/FLOW-UNIT GRAPH/TS-PATTERN/1Hour/OFFSET1800/", 3600, 1800, 1800),
    ("//P25 SYNTH/FLOW-UNIT GRAPH/TS-PATTERN/15Minute/OFFSET0/", 900, 0, 900),
]
PATTERN = "/A/B/FLOW/TS-PATTERN/1HOUR/PAT/"
ORDINARY = "/A/B/FLOW/01JAN2000/1HOUR/TS-OBS/"


@pytest.mark.parametrize("path,step,stored,first", OFFSET7)
def test_dss7_stored_offset_is_reported_not_applied(path, step, stored, first):
    with dss.open(fixture_path("pattern_offset7.dss")) as f:
        for d in (f.read_record_arrays(path), f.read_arrays(path)):
            assert d["version"] == 7 and d["pattern"] is True
            t = np.asarray(d["times"])
            assert t[0] == EPOCH3000
            np.testing.assert_array_equal(np.diff(t), step)
            assert d["time_offset_seconds"] == 0
            assert d["pattern_time_convention"] == CONVENTION
            assert d["pattern_stored_offset_seconds"] == stored
            assert d["pattern_stored_offset_source"] == "dss7_int_head_time_offset"
            assert d["pattern_heclib_first_seconds"] == EPOCH3000 + first
        for df in (f.read_timeseries(path, extra_attrs=True), f.read_record(path, extra_attrs=True)):
            assert df.index[0] == T3000
            assert df.attrs["time_offset_seconds"] == 0
            assert df.attrs["pattern_time_convention"] == CONVENTION
            assert df.attrs["pattern_stored_offset_seconds"] == stored
            assert df.attrs["pattern_stored_offset_source"] == "dss7_int_head_time_offset"
            assert df.attrs["pattern_heclib_first_seconds"] == EPOCH3000 + first
        plain = f.read_timeseries(path)
        assert not any(k in plain.attrs for k in KEYS)


def test_windowed_pattern_read_keeps_origin_fields():
    path, step, stored, first = OFFSET7[0]
    with dss.open(fixture_path("pattern_offset7.dss")) as f:
        d = f.read_arrays(path, "3000-01-01 00:15", "3000-01-01 00:45")
        assert list(np.asarray(d["times"]) - EPOCH3000) == [900, 1800, 2700]
        assert d["pattern_stored_offset_seconds"] == stored
        assert d["pattern_heclib_first_seconds"] == EPOCH3000 + first


@pytest.mark.parametrize(
    "name,stored,source",
    [
        ("synthetic_python6.dss", 0, "dss6_internal_header_word0_minutes"),
        # The synthetic DSS7 pattern has no internal header, so no stored offset word.
        ("synthetic_python7.dss", None, "dss7_int_head_time_offset"),
    ],
)
def test_existing_synthetic_patterns(name, stored, source):
    with dss.open(fixture_path(name)) as f:
        for d in (f.read_record_arrays(PATTERN), f.read_arrays(PATTERN)):
            assert d["pattern"] is True
            assert np.asarray(d["times"])[0] == EPOCH3000
            assert d["time_offset_seconds"] == 0
            assert d["pattern_time_convention"] == CONVENTION
            assert d["pattern_stored_offset_seconds"] == stored
            assert d["pattern_stored_offset_source"] == source
            # 01JAN3000 00:00 + one 1HOUR interval (stored offset 0 / absent).
            assert d["pattern_heclib_first_seconds"] == EPOCH3000 + 3600


@pytest.mark.parametrize(
    "name,path",
    [
        ("synthetic_python6.dss", ORDINARY),
        ("sample7.dss", "/GREEN RIVER/GLENFIR/FLOW//1Hour/OBS/"),
    ],
)
def test_non_pattern_records_have_no_origin_fields(name, path):
    with dss.open(fixture_path(name)) as f:
        d = f.read_arrays(path)
        assert d["pattern"] is False
        assert all(d[k] is None for k in KEYS)
        df = f.read_timeseries(path, extra_attrs=True)
        assert not any(k in df.attrs for k in KEYS)

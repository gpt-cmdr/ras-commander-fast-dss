"""Regressions for nine Python binding defects fixed in P09a; public and synthetic data only."""

import errno
import os
from pathlib import Path

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss._file import _normalize, _slice_window

from conftest import fixture_path

SAMPLES = ["sample6.dss", "sample7.dss"]
PATTERNS = ["synthetic_python6.dss", "synthetic_python7.dss"]
HOURLY = "/GREEN RIVER/GLENFIR/FLOW//1Hour/OBS/"
PATTERN = "/A/B/FLOW/TS-PATTERN/1HOUR/PAT/"


@pytest.mark.parametrize("part", ["A", "F"])
def test_ts_prefix_outside_d_is_not_a_pattern(part):
    path = ("/TS-BASIN/B/FLOW/01JAN2000/1HOUR/F/" if part == "A" else
            "/A/B/FLOW/01JAN2000/1HOUR/TS-OBS/")
    with dss.open(fixture_path("synthetic_python6.dss")) as f:
        record = f.read_record_arrays(path)
        dataset = f.read_arrays(path)
        assert record["pattern"] is False and dataset["pattern"] is False
        np.testing.assert_array_equal(dataset["times"], record["times"])
        np.testing.assert_array_equal(dataset["values"], record["values"])
        assert f.read_timeseries(path).index[0] == pd.Timestamp("2000-01-01 01:00")


@pytest.mark.parametrize("name", SAMPLES)
@pytest.mark.parametrize("start,end,expected", [
    ("00:00:01", "01:00:01", ["01:00", "02:00"]),
    ("00:00:01", "00:00:01", ["01:00"]),
    ("00:00:00", "01:00:00", ["00:00", "01:00"]),
    ("00:00:59", "01:00:59", ["01:00", "02:00"]),
    ("00:30:00", "00:30:00", ["01:00"]),
])
def test_window_seconds_round_forward(name, start, end, expected):
    date = "1992-05-01 "
    with dss.open(fixture_path(name)) as f:
        whole = f.read_timeseries(HOURLY)
        result = f.read_timeseries(HOURLY, date + start, date + end)
        assert result.index.tolist() == [pd.Timestamp(date + t) for t in expected]
        pd.testing.assert_frame_equal(result, whole.loc[result.index])


@pytest.mark.parametrize("name", PATTERNS)
def test_pattern_windows_on_normalized_timeline(name):
    with dss.open(fixture_path(name)) as f:
        whole = f.read_timeseries(PATTERN)
        # No-window behavior stays unchanged: the stored markers are retained here.
        assert len(whole) == 5
        assert whole.index[0] == pd.Timestamp("3000-01-01").as_unit("s")
        for start, end, expected in [
            ("3000-01-01 01:00", "3000-01-01 01:00", [10.0]),
            ("3000-01-01 01:00", "3000-01-01 03:00", [10.0, 20.0, 30.0]),
            ("3000-01-01 01:00:01", "3000-01-01 03:00", [20.0, 30.0]),
            ("3000-01-01", None, [10.0, 20.0, 30.0]),
            (None, "3000-01-01 04:00", [10.0, 20.0, 30.0]),
        ]:
            result = f.read_timeseries(PATTERN, start, end)
            assert result.value.tolist() == expected
            assert not result.value.isna().any()
        raw = f.read_arrays(PATTERN, "3000-01-01", "3000-01-01 04:00", trim_missing=False)
        assert len(raw["times"]) == 5
        assert raw["values"].tolist() == [-901.0, 10.0, 20.0, 30.0, -902.0]
        for start, end in [("3001-01-01", "3001-01-02"), ("3001-01-01", None),
                           (None, "2999-01-01")]:
            with pytest.raises(dss.DssPathnameNotFoundError) as exc:
                f.read_timeseries(PATTERN, start, end)
            assert str(exc.value) == f"No data found for pathname: {PATTERN}"
        with pytest.raises(ValueError, match="end is before"):
            f.read_arrays(PATTERN, "3000-01-02", "3000-01-01")


@pytest.mark.parametrize("version", [6, 7])
@pytest.mark.parametrize("quality", [None, np.arange(5, dtype=np.int32)])
def test_pattern_slicing_preserves_quality_and_trims_after_selection(version, quality):
    epoch = dss.to_hec_seconds("3000-01-01")
    data = dict(version=version, pattern=True, regular=True, interval_seconds=3600,
                times=np.arange(5, dtype=np.int64) * 3600 + (3600 if version == 6 else epoch),
                values=np.array([-901, 10, 20, 30, -902], dtype=np.float32), quality=quality)
    normalized = _normalize(data)
    for start, end in [(epoch, epoch + 14400), (epoch, None), (None, epoch + 14400)]:
        result = _slice_window(normalized, start, end, True)
        assert result["values"].tolist() == [10, 20, 30]
        assert result["times"].tolist() == [epoch + 3600, epoch + 7200, epoch + 10800]
        if quality is None:
            assert result["quality"] is None
        else:
            assert result["quality"].tolist() == [1, 2, 3]
    empty = _slice_window(normalized, epoch + 36000, None, True)
    assert len(empty["times"]) == len(empty["values"]) == 0
    if quality is not None:
        assert len(empty["quality"]) == 0


@pytest.mark.parametrize("value", [
    pd.Timestamp("1992-05-01 00:00:00.5"),
    "1992-05-01T00:00:00.5",
    np.datetime64("1992-05-01T00:00:00.500000000"),
    pd.Timestamp("1992-05-01 00:00:00.000000001"),
])
@pytest.mark.parametrize("name", SAMPLES)
def test_fractional_seconds_rejected(value, name):
    with pytest.raises(ValueError, match="fractional seconds"):
        dss.to_hec_seconds(value)
    with dss.open(fixture_path(name)) as f:
        for bounds in [dict(start=value), dict(end=value), dict(start=value, end=value)]:
            with pytest.raises(ValueError, match="fractional seconds"):
                f.read_timeseries(HOURLY, **bounds)


@pytest.mark.parametrize("name", SAMPLES)
def test_source_path_is_captured_at_open(name, monkeypatch, tmp_path):
    # conftest's resolved fixture directory is UNC on Windows; keep the drive
    # spelling here to exercise the mapped-drive-preserving resolver as well.
    directory = Path(__file__).absolute().parents[2] / "fixtures" / "public"
    monkeypatch.chdir(directory)
    with dss.open(name) as f:
        assert str(f.path) == name
        monkeypatch.chdir(tmp_path)
        for frame in [f.read_timeseries(HOURLY),
                      f.read_record("/GREEN RIVER/GLENFIR/FLOW/01Apr1992/1Hour/OBS/")]:
            assert os.path.samefile(frame.attrs["dss_file"], fixture_path(name))
            if os.name == "nt" and len(directory.drive) == 2:
                assert frame.attrs["dss_file"].startswith(directory.drive)


@pytest.mark.parametrize("clock", ["1260", "12:99", "23:59:60", "2401", "24:00:01", "2500",
                                  "00:60:00", "24:59:00", "99:00:00"])
def test_invalid_hec_clock_fields(clock):
    with pytest.raises(ValueError, match="invalid time of day"):
        dss.to_hec_seconds("01JAN2020 " + clock)


@pytest.mark.parametrize("clock", ["2400", "24:00", "24:00:00", "240000"])
def test_hec_24_hour_rollover(clock):
    assert dss.to_hec_seconds("01JAN2020 " + clock) == dss.to_hec_seconds("02JAN2020")


def test_hec_clock_last_valid_second():
    assert dss.to_hec_seconds("01JAN2020 23:59:59") == dss.to_hec_seconds("02JAN2020") - 1


def test_generic_oserror_has_filename_and_errno():
    path = str(fixture_path("sample7.dss")) + "\0"
    with pytest.raises(OSError) as exc:
        dss.open(path)
    assert exc.value.filename == path
    assert exc.value.errno == errno.EINVAL
    assert exc.value.strerror


def test_windows_oserror_retains_winerror(tmp_path):
    if os.name != "nt":
        return  # Win32 codes do not exist on POSIX.
    path = str(tmp_path / "bad<name.dss")
    with pytest.raises(OSError) as exc:
        dss.open(path)
    assert exc.value.filename == path
    assert exc.value.winerror == 123  # ERROR_INVALID_NAME, mapped by Python to EINVAL
    assert exc.value.errno == errno.EINVAL


@pytest.mark.parametrize("name", SAMPLES)
def test_not_found_message_and_native_cause(name):
    absent = "/NO/SUCH/FLOW//1HOUR/RECORD/"
    with dss.open(fixture_path(name)) as f:
        for method in [f.read_timeseries, f.read_arrays, f.read_record, f.read_record_arrays]:
            with pytest.raises(dss.DssPathnameNotFoundError) as exc:
                method(absent)
            assert str(exc.value) == f"No data found for pathname: {absent}"
            assert isinstance(exc.value.__cause__, dss.DssPathnameNotFoundError)
            assert str(exc.value.__cause__) != str(exc.value)
        for start, end in [("1990-01-01", "1990-01-02"), ("1990-01-01", None),
                           (None, "1990-01-01")]:
            # The start-only case has later data, so use a late start for that case.
            if end is None:
                start = "2000-01-01"
            with pytest.raises(dss.DssPathnameNotFoundError) as exc:
                f.read_timeseries(HOURLY, start, end)
            assert str(exc.value) == f"No data found for pathname: {HOURLY}"

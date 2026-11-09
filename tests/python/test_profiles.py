"""Regular profile reads through ras_commander_fast_dss, on committed public fixtures.

``synthetic_profile7.dss`` is written by HEC Monolith 3.3.27
through its Java API; the expected
values are the values that were written. Monolith always stores profiles as doubles (107), so
the float (102) case uses ``synthetic_profile_f32_7.dss`` from
``crates/dss-core/tests/dss7_profile.rs`` (no HEC software).
"""

import hashlib

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssPathnameNotFoundError, DssUnsupportedRecordError, Profile

from conftest import fixture_path

PROFILE7 = "synthetic_profile7.dss"
F32 = "synthetic_profile_f32_7.dss"
DBL = "/PUBLIC/LAKE/DEPTH-TEMP//1HOUR/SYNTH-DBL/"
DBL_JAN = "/PUBLIC/LAKE/DEPTH-TEMP/01Jan2020/1Hour/SYNTH-DBL/"
DAY = "/PUBLIC/LAKE/DEPTH-TEMP//1DAY/SYNTH-DAY/"
GAP = "/PUBLIC/LAKE/DEPTH-DO//1DAY/SYNTH-GAP/"
IRR = "/PUBLIC/LAKE/DEPTH-TEMP//IR-MONTH/SYNTH-IRR/"
TS = "/PUBLIC/LAKE/FLOW//1HOUR/SYNTH-TS/"
F32_PATH = "/PUBLIC/LAKE/DEPTH-TEMP//1DAY/SYNTH-F32/"
UNDEF = float(-np.finfo(np.float32).max)
T0 = np.datetime64("2020-01-31T20:00:00", "s")


def expected_dbl():
    """ProfileMonolith dataset A, row-major (10 x 4), markers kept."""
    v = np.empty((10, 4))
    for i in range(10):
        for k in range(4):
            v[i, k] = 20.0 - 0.75 * k + 0.125 * i + 1.0e-9 * (i * 7 + k)
    v[2, 1] = UNDEF
    v[3, 3] = -901.0
    v[5, :] = UNDEF
    return v


def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()


@pytest.fixture
def prof7():
    p = fixture_path(PROFILE7)
    before = sha(p)
    with dss.open(p) as f:
        yield f
    assert sha(p) == before, "reader modified the fixture"


def test_double_profile_values_bitwise(prof7):
    pr = prof7.read_profile(DBL, raw=True)
    assert isinstance(pr, Profile)
    assert pr.record_type == 107
    assert pr.stored_dtype == np.float64 and pr.values.dtype == np.float64
    assert pr.shape == (10, 4) and pr.n_times == 10 and pr.n_depths == 4
    np.testing.assert_array_equal(pr.depths, [0.0, 1.5, 3.0, 4.5])
    assert np.array_equal(pr.values.view(np.uint64), expected_dbl().view(np.uint64))
    assert pr.times.dtype == np.dtype("datetime64[s]")
    np.testing.assert_array_equal(pr.times, T0 + np.arange(10) * np.timedelta64(3600, "s"))
    np.testing.assert_array_equal(pr.hec_seconds, (pr.times - dss.HEC_EPOCH).astype(np.int64))
    assert (pr.depth_units, pr.value_units, pr.value_type) == ("m", "deg C", "INST-VAL")
    assert pr.depth_type is None
    assert pr.time_zone == "UTC"
    assert pr.interval_seconds == 3600
    assert pr.precision == -1
    assert pr.quality is None
    assert pr.dss_version == 7
    md = pr.metadata
    assert md["blocks_found"] == 2 and md["blocks_missing"] == 0
    assert len(md["block_pathnames"]) == 2
    assert md["internal_header"][3] == 4  # profileDepthsNumber
    assert md["time_granularity_seconds"] == 60
    assert md["interval_name"]


def test_missing_markers_become_nan_unless_raw(prof7):
    pr = prof7.read_profile(DBL)
    assert not pr.raw
    want = expected_dbl()
    miss = (want == UNDEF) | (want == -901.0)
    assert np.array_equal(np.isnan(pr.values), miss)
    np.testing.assert_array_equal(pr.values[~miss], want[~miss])
    assert np.array_equal(pr.missing_mask(), miss)
    raw = prof7.read_profile(DBL, raw=True)
    assert not np.isnan(raw.values).any()
    assert np.array_equal(raw.missing_mask(), miss)


def test_to_dataframe_is_wide(prof7):
    pr = prof7.read_profile(DBL)
    df = pr.to_dataframe()
    assert df.shape == (10, 4)
    assert isinstance(df.index, pd.DatetimeIndex) and df.index.name == "datetime"
    assert df.index.dtype == np.dtype("datetime64[s]")
    assert df.columns.name == "depth"
    np.testing.assert_array_equal(df.columns.to_numpy(), pr.depths)
    assert all(df.dtypes == np.float64)
    np.testing.assert_array_equal(df.to_numpy(), pr.values)
    assert df.attrs["depth_units"] == "m" and df.attrs["value_units"] == "deg C"
    assert df.attrs["type"] == "INST-VAL" and df.attrs["pathname"] == DBL
    np.testing.assert_array_equal(pr.at_depth(-1), pr.values[:, 3])
    with pytest.raises(IndexError):
        pr.at_depth(4)


def test_trim_and_windows(prof7):
    full = prof7.read_profile(DBL, trim_missing=False, raw=True)
    assert full.shape == (48, 4)  # 31Jan2020 01:00 .. 01Feb2020 24:00
    trimmed = prof7.read_profile(DBL, raw=True)
    i0 = int(np.flatnonzero(full.times == trimmed.times[0])[0])
    assert np.array_equal(full.values[i0 : i0 + 10], trimmed.values)
    # Two-sided window, HEC string bounds.
    w = prof7.read_profile(DBL, "31JAN2020 2100", "01FEB2020 0400", raw=True)
    np.testing.assert_array_equal(w.times, trimmed.times[1:9])
    assert np.array_equal(w.values, trimmed.values[1:9])
    # One-sided windows cut the whole dataset, then trim.
    s = prof7.read_profile(DBL, start=trimmed.times[3], raw=True)
    np.testing.assert_array_equal(s.times, trimmed.times[3:])
    e = prof7.read_profile(DBL, end=trimmed.times[6], raw=True)
    np.testing.assert_array_equal(e.times, trimmed.times[:7])
    e2 = prof7.read_profile(DBL, end=trimmed.times[6], raw=True, trim_missing=False)
    assert e2.times[0] == full.times[0]
    with pytest.raises(ValueError):
        prof7.read_profile(DBL, "01FEB2020 0400", "31JAN2020 2100")
    with pytest.raises(DssPathnameNotFoundError):
        prof7.read_profile(DBL, "01JAN2030 0100", "02JAN2030 0100")


def test_record_read_and_missing_block(prof7):
    rec = prof7.read_profile_record(DBL_JAN, raw=True)
    assert rec.shape == (744, 4)  # every hour of January 2020
    assert rec.metadata["stored_pathname"] == DBL_JAN
    gap = prof7.read_profile(GAP)
    assert gap.shape == (370, 2)  # 30Dec2018 .. 02Jan2020; no 2019 block
    assert gap.metadata["blocks_found"] == 2 and gap.metadata["blocks_missing"] == 1
    assert np.isnan(gap.values[2:-2]).all()
    np.testing.assert_array_equal(gap.values[:2], [[5.5, 4.25], [5.75, 4.5]])
    np.testing.assert_array_equal(gap.values[-2:], [[6.0, 4.75], [6.25, 5.0]])
    day = prof7.read_profile(DAY)
    assert day.shape == (6, 3) and day.time_zone is None
    assert (day.depth_units, day.value_units, day.value_type) == ("ft", "deg F", "PER-AVER")


def test_typed_errors(prof7):
    for call in (lambda: prof7.read_timeseries(DBL), lambda: prof7.read_record(DBL_JAN)):
        with pytest.raises(DssUnsupportedRecordError, match="read_profile"):
            call()
    with pytest.raises(DssUnsupportedRecordError, match="read_timeseries"):
        prof7.read_profile(TS)
    with pytest.raises(DssUnsupportedRecordError, match="irregular"):
        prof7.read_profile(IRR)
    with pytest.raises(DssPathnameNotFoundError):
        prof7.read_profile("/PUBLIC/LAKE/NOPE//1DAY/X/")
    with pytest.raises(ValueError):  # DssPathnameNotFoundError is a ValueError
        prof7.read_profile_record("/PUBLIC/LAKE/NOPE/01Jan2020/1Day/X/")
    # read_timeseries still reads the scalar series in the same file.
    assert len(prof7.read_timeseries(TS)) == 5


def test_non_time_series_record_is_unsupported():
    with dss.open(fixture_path("sample7.dss")) as f:
        cat = f.catalog()
        pd_path = cat.pathname[cat.record_type == 200].iloc[0]
        with pytest.raises(DssUnsupportedRecordError):
            f.read_profile(pd_path)
        with pytest.raises(DssUnsupportedRecordError):
            f.read_profile_record(pd_path)


@pytest.mark.parametrize("name", ["castro.dss", "synth6.dss"])
def test_dss6_has_no_profiles(name):
    with dss.open(fixture_path(name)) as f:
        assert f.version == 6
        cat = f.catalog()
        ts = cat.pathname[(cat.record_type >= 100) & (cat.record_type < 200)].iloc[0]
        with pytest.raises(DssUnsupportedRecordError, match="DSS6"):
            f.read_profile_record(ts)
        parts = ts.split("/")
        parts[4] = ""
        with pytest.raises(DssUnsupportedRecordError, match="DSS6"):
            f.read_profile("/".join(parts))
        with pytest.raises(DssPathnameNotFoundError):
            f.read_profile("/NO/SUCH/FLOW//1HOUR/X/")


def test_float_profile_keeps_float32():
    p = fixture_path(F32)
    pr = dss.read_profile(p, F32_PATH)
    assert pr.record_type == 102
    assert pr.stored_dtype == np.float32
    assert pr.values.dtype == np.float32 and pr.depths.dtype == np.float32
    np.testing.assert_array_equal(pr.depths, np.array([0.5, 2.25, 10.0], dtype=np.float32))
    want = np.array(
        [[np.float32(60.1) + np.float32(1.3) * np.float32(i) - np.float32(2.7) * np.float32(k) for k in range(3)]
         for i in range(6)],
        dtype=np.float32,
    )
    raw = dss.read_profile(p, F32_PATH, raw=True).values
    assert raw[4, 0] == np.float32(UNDEF) and raw[2, 2] == -901.0 and raw[3, 1] == -902.0
    keep = ~np.isnan(pr.values)
    assert keep.sum() == 15
    assert np.array_equal(raw[keep].view(np.uint32), want[keep].view(np.uint32))
    assert pr.precision == 2 and pr.time_zone == "UTC"
    assert pr.to_dataframe().dtypes.unique().tolist() == [np.dtype(np.float32)]


def test_module_level_and_exports():
    assert "read_profile" in dss.__all__ and "Profile" in dss.__all__
    pr = dss.read_profile(fixture_path(PROFILE7), DBL, "31JAN2020 2000", "31JAN2020 2300")
    assert pr.shape == (4, 4)
    assert pr.dss_file.endswith(PROFILE7)


# -- edge cases -----------------------------------------------------------------------

QUAL = "/PUBLIC/LAKE/DEPTH-TEMP//1HOUR/SYNTH-QUAL/"
QUAL_REC = "/PUBLIC/LAKE/DEPTH-TEMP/01Jun2020/1Hour/SYNTH-QUAL/"
DBL_TS = "/PUBLIC/LAKE/TEMP-SURF//1HOUR/SYNTH-DBL-TS/"
MB = "/PUBLIC/LAKE/DEPTH-TEMP//1DAY/SYNTH-MB/"


def test_quality_from_record_and_dataset_reads(prof7):
    ds = prof7.read_profile(QUAL)
    np.testing.assert_array_equal(ds.quality, [3, 5, 9, 17, 33])
    assert ds.quality.dtype == np.int32
    assert ds.metadata["quality_element_size"] == 1
    rec = prof7.read_profile_record(QUAL_REC)
    assert rec.quality is not None and len(rec.quality) == rec.n_times
    assert rec.metadata["quality_element_size"] == 1
    i = int(np.flatnonzero(rec.times == ds.times[0])[0])
    np.testing.assert_array_equal(rec.quality[i : i + 5], ds.quality)
    # One-sided windows keep quality aligned with the rows.
    cut = prof7.read_profile(QUAL, start=ds.times[1])
    np.testing.assert_array_equal(cut.quality, ds.quality[1:])


@pytest.mark.parametrize(
    "start,end",
    [
        ("31JAN2020 2030", "01FEB2020 0030"),
        ("31JAN2020 1959", "31JAN2020 2059"),
        ("31JAN2020 2301", "01FEB2020 0459"),
    ],
)
@pytest.mark.parametrize("trim", [False, True])
def test_windows_between_stamps_match_read_timeseries(prof7, start, end, trim):
    pr = prof7.read_profile(DBL, start, end, raw=True, trim_missing=trim)
    ts = prof7.read_timeseries(DBL_TS, start, end, raw=True, trim_missing=trim)
    np.testing.assert_array_equal(pr.times, ts.index.to_numpy())
    if not trim:
        assert np.array_equal(pr.values[:, 0].view(np.uint64), ts["value"].to_numpy().view(np.uint64))
    # One-sided bounds between stamps agree too.
    pr1 = prof7.read_profile(DBL, start=start, raw=True, trim_missing=trim)
    ts1 = prof7.read_timeseries(DBL_TS, start=start, raw=True, trim_missing=trim)
    np.testing.assert_array_equal(pr1.times, ts1.index.to_numpy())


def test_float_profile_across_two_blocks_bit_exact():
    pr = dss.read_profile(fixture_path(F32), MB, raw=True)
    assert pr.record_type == 102 and pr.stored_dtype == np.float32
    assert pr.metadata["blocks_found"] == 2
    np.testing.assert_array_equal(pr.depths, np.array([0.5, 1.0, 2.5], dtype=np.float32))
    a = [np.float32(11.0) + np.float32(0.5) * np.float32(i) + np.float32(0.001) for i in range(6)]
    b = [np.float32(21.0) + np.float32(0.5) * np.float32(i) + np.float32(0.001) for i in range(6)]
    b[1] = np.float32(-901.0)
    want = np.array(a + b, dtype=np.float32).reshape(4, 3)
    assert np.array_equal(pr.values.view(np.uint32), want.view(np.uint32))
    np.testing.assert_array_equal(
        pr.times, np.datetime64("2020-12-31T00:00:00", "s") + np.arange(4) * np.timedelta64(86400, "s")
    )


def test_pattern_records_are_refused_with_a_pattern_hint():
    with dss.open(fixture_path("synthetic_python7.dss")) as f:
        with pytest.raises(DssUnsupportedRecordError, match="pattern"):
            f.read_profile("/A/B/FLOW/TS-PATTERN/1HOUR/PAT/")

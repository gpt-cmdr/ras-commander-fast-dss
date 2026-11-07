"""P29: opt-in HEC DSS6 "no data" refusal (``no_data="refuse"``, ``DssNoDataError``).

HEC DSS6 (``zrrtsi.f:ZRRTSI6`` lines 618-651, hec-dss fork tag ``7-IU-15``) reports
"no data" (``ISTAT = 4``; Monolith -2) when the blocks of a regular request exist but no
value read is other than -901/-902. The rule is for the request as a whole, not block by
block: an all-missing block inside a window that holds data is read as found
(docs/format/dss6.md §7.4). The default ``no_data="return"`` keeps the P28 behavior.

Fixtures: ``synthetic_all_missing6.dss`` (P28) and ``synthetic_no_data6.dss`` (P29), both
generated and byte-checked by the Rust tests (``dss6_all_missing.rs``, ``dss6_no_data.rs``).
"""

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from conftest import fixture_path

P28 = "synthetic_all_missing6.dss"
P29 = "synthetic_no_data6.dss"

P28_NO_DATA = [
    "/P28/GAGE M/FLOW/01JAN2000/1DAY/OBS/",  # RTS all -901
    "/P28/GAGE Q/FLOW/01JAN2000/1DAY/OBS/",  # RTS -901/-902
    "/P28/GAGE R/FLOW/01JAN2000/1DAY/OBS/",  # RTD all -901
    "/P28/GAGE W/FLOW/01JAN2000/1DAY/OBS/",  # RTS with quality, all -901
    "/P28/GAGE N/FLOW/01JAN2000/1DAY/OBS/",  # RTS all -902
]
P28_FOUND = [
    "/P28/GAGE M/FLOW/01JAN2001/1DAY/OBS/",  # data
    "/P28/GAGE X/FLOW/01JAN2000/1DAY/OBS/",  # all -FLT_MAX: data for HEC
]
U = "/P29/GAGE U/FLOW//1DAY/OBS/"
V = "/P29/GAGE V/FLOW//1DAY/OBS/"


@pytest.fixture
def f28():
    with dss.open(fixture_path(P28)) as fh:
        yield fh


@pytest.fixture
def f29():
    with dss.open(fixture_path(P29)) as fh:
        yield fh


def test_exception_class():
    assert issubclass(dss.DssNoDataError, dss.DssPathnameNotFoundError)
    assert issubclass(dss.DssNoDataError, ValueError)
    assert dss.NO_DATA_STATUS == 4
    assert "DssNoDataError" in dss.__all__ and "NO_DATA_STATUS" in dss.__all__
    from ras_commander_fast_dss import exceptions

    assert "DssNoDataError" in exceptions.__all__
    e = dss.DssNoDataError("msg")
    assert e.pathname is None and e.status == 4


@pytest.mark.parametrize("path", P28_NO_DATA)
def test_record_refused_with_option(f28, path):
    with pytest.raises(dss.DssNoDataError) as ei:
        f28.read_record(path, no_data="refuse")
    assert ei.value.pathname == path and ei.value.status == 4
    assert "status 4" in str(ei.value)
    with pytest.raises(dss.DssPathnameNotFoundError):  # existing handlers catch it
        f28.read_record_arrays(path, no_data="refuse")
    # Default and explicit "return": the block, as in P28.
    for kw in ({}, {"no_data": "return"}):
        df = f28.read_record(path, raw=True, **kw)
        assert len(df) == 366 and np.isin(df["value"].to_numpy(), (-901.0, -902.0)).all()


@pytest.mark.parametrize("path", P28_FOUND)
def test_found_blocks_never_raise(f28, path):
    a = f28.read_record_arrays(path, no_data="refuse")
    b = f28.read_record_arrays(path)
    assert np.array_equal(a["times"], b["times"])
    assert np.array_equal(a["values"], b["values"])


def test_bad_option_value(f28):
    for bad in ("Refuse", "raise", None, True):
        with pytest.raises(ValueError):
            f28.read_record(P28_NO_DATA[0], no_data=bad)
        with pytest.raises(ValueError):
            f28.read_timeseries("/P28/GAGE M/FLOW//1DAY/OBS/", no_data=bad)


@pytest.mark.parametrize(
    "path",
    ["/P28/GAGE Q/FLOW//1DAY/OBS/", "/P28/GAGE R/FLOW//1DAY/OBS/", "/P28/GAGE W/FLOW//1DAY/OBS/",
     "/P28/GAGE N/FLOW//1DAY/OBS/"],
)
def test_dataset_of_only_all_missing_blocks(f28, path):
    for trim in (False, True):
        with pytest.raises(dss.DssNoDataError) as ei:
            f28.read_timeseries(path, trim_missing=trim, no_data="refuse")
        assert ei.value.pathname == path
        with pytest.raises(dss.DssNoDataError):
            f28.read_arrays(path, trim_missing=trim, no_data="refuse")
    # Default modes unchanged (P28).
    assert len(f28.read_timeseries(path, trim_missing=False)) == 366
    assert len(f28.read_arrays(path)["values"]) == 0


def test_dataset_with_data_is_identical_in_both_modes(f28):
    path = "/P28/GAGE M/FLOW//1DAY/OBS/"
    for trim in (False, True):
        a = f28.read_timeseries(path, trim_missing=trim, no_data="refuse", extra_attrs=True)
        b = f28.read_timeseries(path, trim_missing=trim, extra_attrs=True)
        assert a.equals(b) and a.attrs == b.attrs
        assert a.attrs["blocks_found"] == 2
        assert len(a) == (365 if trim else 731)


def test_windows(f28):
    path = "/P28/GAGE M/FLOW//1DAY/OBS/"
    # Inside the all-missing year: refused; default returns NaN rows.
    with pytest.raises(dss.DssNoDataError):
        f28.read_timeseries(path, "2000-03-01", "2000-03-31", trim_missing=False, no_data="refuse")
    assert len(f28.read_timeseries(path, "2000-03-01", "2000-03-31", trim_missing=False)) == 31
    # Reaching into 2001: data, identical in both modes.
    a = f28.read_timeseries(path, "2000-12-01", "2001-01-10", trim_missing=False, no_data="refuse")
    b = f28.read_timeseries(path, "2000-12-01", "2001-01-10", trim_missing=False)
    assert a.equals(b)
    # One-sided windows apply the same rule to the cut.
    with pytest.raises(dss.DssNoDataError):
        f28.read_timeseries(path, None, "2000-06-01", no_data="refuse")
    assert len(f28.read_timeseries(path, "2000-12-31", None, no_data="refuse")) == 365


def test_user_header_from_last_block_found(f29):
    # The all-missing 2001 block is the last block found; HEC returns its header.
    for nd in ("return", "refuse"):
        for trim in (False, True):
            d = f29.read_arrays(U, trim_missing=trim, no_data=nd)
            assert d["user_header_text"] == "Gage: U missing 2001;"
            assert d["user_header_block"] == "/P29/GAGE U/FLOW/01JAN2001/1DAY/OBS/"
            assert d["user_header_conflict"] is True
            assert d["blocks_found"] == 2
            assert len(d["values"]) == (366 if trim else 731)
    # Record reads: the data block returns; the all-missing block is refused.
    d = f29.read_record_arrays("/P29/GAGE U/FLOW/01JAN2000/1DAY/OBS/", no_data="refuse")
    assert d["user_header_text"] == "Gage: U data 2000;"
    with pytest.raises(dss.DssNoDataError):
        f29.read_record_arrays("/P29/GAGE U/FLOW/01JAN2001/1DAY/OBS/", no_data="refuse")
    with pytest.raises(dss.DssNoDataError):
        f29.read_timeseries(U, "2001-02-01", "2001-02-28", no_data="refuse")


def test_absent_and_all_missing_blocks(f29):
    d = f29.read_arrays(V, trim_missing=False)
    assert d["blocks_found"] == 3 and len(d["values"]) == 366 + 3 * 365
    for trim in (False, True):
        with pytest.raises(dss.DssNoDataError):
            f29.read_timeseries(V, trim_missing=trim, no_data="refuse")
    # A window inside the absent year reads no block: plain not-found, both modes.
    for nd in ("return", "refuse"):
        with pytest.raises(dss.DssPathnameNotFoundError) as ei:
            f29.read_timeseries(V, "2002-03-01", "2002-03-10", trim_missing=False, no_data=nd)
        assert not isinstance(ei.value, dss.DssNoDataError)


def test_module_helper(f28):
    path = fixture_path(P28)
    with pytest.raises(dss.DssNoDataError):
        dss.read_timeseries(path, "/P28/GAGE N/FLOW//1DAY/OBS/", trim_missing=False, no_data="refuse")
    assert len(dss.read_timeseries(path, "/P28/GAGE N/FLOW//1DAY/OBS/", trim_missing=False)) == 366


@pytest.mark.parametrize("name", ["sample7.dss", "BaldEagleDamBrk.dss"])
def test_dss7_unaffected(name):
    with dss.open(fixture_path(name)) as f:
        cat = f.catalog()
        ts = cat[cat["record_type"].isin([100, 105, 110, 115])]["pathname"].tolist()[:40]
        assert ts
        for p in ts:
            try:
                a = f.read_record_arrays(p)
            except dss.DssError:
                with pytest.raises(dss.DssError) as ei:
                    f.read_record_arrays(p, no_data="refuse")
                assert not isinstance(ei.value, dss.DssNoDataError)
                continue
            b = f.read_record_arrays(p, no_data="refuse")
            assert np.array_equal(a["values"], b["values"], equal_nan=True)


@pytest.mark.parametrize("name", ["sample6.dss", "castro.dss", "synth6.dss"])
def test_public_dss6_irregular_and_pattern_unaffected(name):
    with dss.open(fixture_path(name)) as f:
        cat = f.catalog()
        for p, t in zip(cat["pathname"], cat["record_type"]):
            if t not in (101, 110, 111, 115):
                continue
            try:
                a = f.read_record_arrays(p)
            except dss.DssError:
                continue
            b = f.read_record_arrays(p, no_data="refuse")
            assert np.array_equal(a["values"], b["values"], equal_nan=True)


# -- P29 review ----------------------------------------------------------------------
# M1: records the option must not reach, each with every value -901/-902 (synthetic
# fixtures; the public DSS6 and DSS7 files above hold no such record).

P_PAT = "/P29/GAGE P/FLOW/TS-PATTERN/1DAY/OBS/"  # type 100, TS- pattern
I_2000 = "/P29/GAGE I/STAGE/01JAN2000/IR-MONTH/OBS/"  # type 110 irregular
D7 = "/P29/GAGE D7/FLOW/01JAN2000/1DAY/OBS/"  # DSS7 regular, all -901/-902


def _same(a, b):
    assert np.array_equal(a["times"], b["times"])
    assert np.array_equal(a["values"], b["values"], equal_nan=True)


def test_all_missing_pattern_and_irregular_are_unaffected(f29):
    for p in (P_PAT, I_2000):
        a = f29.read_record_arrays(p, no_data="refuse")
        b = f29.read_record_arrays(p)
        _same(a, b)
        assert len(a["values"]) and np.isin(a["values"], (-901.0, -902.0)).all()
    for p, n in ((P_PAT, 24), ("/P29/GAGE I/STAGE//IR-MONTH/OBS/", 4)):
        for trim in (False, True):
            _same(
                f29.read_arrays(p, trim_missing=trim, no_data="refuse"),
                f29.read_arrays(p, trim_missing=trim),
            )
        assert len(f29.read_timeseries(p, trim_missing=False, no_data="refuse")) == n


def test_dss7_all_missing_block_is_unaffected():
    with dss.open(fixture_path("synthetic_no_data7.dss")) as f:
        assert f.version == 7
        a = f.read_record_arrays(D7, no_data="refuse")
        b = f.read_record_arrays(D7)
        _same(a, b)
        assert len(a["values"]) == 366 and np.isin(a["values"], (-901.0, -902.0)).all()
        df = f.read_record(D7, raw=True, no_data="refuse")
        assert len(df) == 366


# m1: -FLT_MAX and NaN are values for HEC's rule.

F = "/P29/GAGE F/FLOW//1DAY/OBS/"  # -901 except one -FLT_MAX
Z = "/P29/GAGE Z/FLOW//1DAY/OBS/"  # -901 except one NaN


@pytest.mark.parametrize("path", [F, Z])
def test_flt_max_and_nan_are_not_refused(f29, path):
    rec = path.replace("//1DAY", "/01JAN2000/1DAY")
    assert len(f29.read_record_arrays(rec, no_data="refuse")["values"]) == 366
    assert len(f29.read_arrays(path, trim_missing=False, no_data="refuse")["values"]) == 366
    # P30: the default trim is ztsTrim's (-901/-902/-FLT_MAX). F has no values left, but
    # nothing is refused: read_arrays is empty, read_timeseries raises plain not-found.
    # Z keeps its stored NaN (a value for HEC; Monolith returns 1 value).
    for nd in ("refuse", "return"):
        d = f29.read_arrays(path, no_data=nd)
        if path == Z:
            assert len(d["values"]) == 1 and np.isnan(d["values"][0])
            assert len(f29.read_timeseries(path, no_data=nd)) == 1
        else:
            assert len(d["values"]) == 0
            with pytest.raises(dss.DssPathnameNotFoundError) as ei:
                f29.read_timeseries(path, no_data=nd)
            assert not isinstance(ei.value, dss.DssNoDataError)
    # One-sided windows go through the Python rule (_hec_no_data): not refused.
    d = f29.read_arrays(path, None, "2000-12-31", trim_missing=False, no_data="refuse")
    assert len(d["values"]) > 300
    d = f29.read_arrays(path, None, "2000-12-31", no_data="refuse")  # trimmed: no raise
    assert len(d["values"]) == (1 if path == Z else 0)


def test_python_rule_treats_nan_and_flt_max_as_values():
    from ras_commander_fast_dss._file import _hec_no_data

    base = {"version": 6, "regular": True, "pattern": False}
    fmax = float(-np.finfo(np.float32).max)
    assert _hec_no_data(dict(base, values=np.array([-901.0, -902.0], dtype=np.float32)))
    assert not _hec_no_data(dict(base, values=np.array([-901.0, np.nan])))
    assert not _hec_no_data(dict(base, values=np.array([-901.0, fmax], dtype=np.float32)))
    assert not _hec_no_data(dict(base, values=np.array([], dtype=np.float32)))
    assert not _hec_no_data(dict(base, version=7, values=np.array([-901.0])))
    assert not _hec_no_data(dict(base, regular=False, values=np.array([-901.0])))
    assert not _hec_no_data(dict(base, pattern=True, values=np.array([-901.0])))


# m2: start-only window on an all-missing tail (U: data in 2000, all -901 in 2001).


def test_start_only_window_on_all_missing_tail(f29):
    with pytest.raises(dss.DssNoDataError) as ei:
        f29.read_timeseries(U, "2001-02-01", None, no_data="refuse")
    assert ei.value.pathname == U and ei.value.status == 4
    with pytest.raises(dss.DssNoDataError):
        f29.read_arrays(U, "2001-02-01", None, trim_missing=False, no_data="refuse")
    # Default: the trim leaves nothing; read_arrays is empty, read_timeseries plain not-found.
    assert len(f29.read_arrays(U, "2001-02-01", None)["values"]) == 0
    with pytest.raises(dss.DssPathnameNotFoundError) as ei:
        f29.read_timeseries(U, "2001-02-01", None)
    assert not isinstance(ei.value, dss.DssNoDataError)
    # A start inside 2000 reaches data: identical in both modes.
    a = f29.read_timeseries(U, "2000-12-01", None, trim_missing=False, no_data="refuse")
    b = f29.read_timeseries(U, "2000-12-01", None, trim_missing=False)
    assert a.equals(b)


# n5: the module helper validates no_data before opening the file.


def test_module_helper_validates_before_open(tmp_path):
    with pytest.raises(ValueError):
        dss.read_timeseries(tmp_path / "absent.dss", "/A/B/FLOW//1DAY/F/", no_data="bad")
    with pytest.raises(FileNotFoundError):
        dss.read_timeseries(tmp_path / "absent.dss", "/A/B/FLOW//1DAY/F/", no_data="refuse")

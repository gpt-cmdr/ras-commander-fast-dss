"""ras_commander_fast_dss Python API on the committed public fixtures (DSS6 and DSS7).

Expected counts come from the Monolith comparisons in P02/P08a/P08b; the reference
values (castro, sample7) were read with ras-commander's Java ``RasDss`` (P09a).
"""

import hashlib
import os
import threading
import warnings
from concurrent.futures import Future, wait

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import (
    DssError,
    DssFormatError,
    DssLockedError,
    DssPathnameNotFoundError,
    DssUnsupportedRecordError,
)

from conftest import ALL, DSS6, DSS7, FIXTURES, fixture_path

# name: (version, version_string, live entries, {record_type: count}, TS datasets)
CATALOGS = {
    "sample7.dss": (7, "7-10", 594, {100: 590, 200: 4}, 32),
    "BaldEagleDamBrk.dss": (7, "7-HL", 102, {100: 102}, 15),
    "sample6.dss": (6, "6-SB", 595, {100: 590, 200: 5}, 32),
    "castro.dss": (6, "6-QF", 3, {100: 2, 200: 1}, 2),
    "green_river.dss": (6, "6-ND", 30, {100: 30}, 4),
    "Muncie.dss": (6, "6-UF", 1164, {100: 432, 105: 18, 200: 714}, 155),
    "sample_vb.dss": (6, "6-LA", 32, {100: 31, 200: 1}, 16),
    "synth6.dss": (6, "6-YO", 21, {100: 15, 110: 6}, 10),
}

ATTR_KEYS = {"pathname", "units", "type", "interval", "dss_file"}
FLT_MAX = float(np.finfo(np.float32).max)


def dataset_key(p: str) -> str:
    s = p.split("/")
    s[4] = ""
    return "/".join(s).upper()


def first_paths(cat: pd.DataFrame) -> dict:
    """First catalog pathname of every time-series dataset."""
    out = {}
    for p in cat.loc[cat.record_type.between(100, 119), "pathname"]:
        out.setdefault(dataset_key(p), p)
    return out


def fingerprint(path) -> tuple:
    st = os.stat(path)
    return hashlib.sha256(path.read_bytes()).hexdigest(), st.st_size, st.st_mtime_ns


# --- file facts and catalog ------------------------------------------------------------


@pytest.mark.parametrize("name", ALL)
def test_open_version_and_signature(name):
    version, vstring, nlive, _, _ = CATALOGS[name]
    with dss.open(fixture_path(name)) as f:
        assert f.version == version
        assert f.version_string == vstring
        sig = f.signature()
        assert sig["version"] == version and sig["version_string"] == vstring
        assert sig["byte_order"] == "little"
        assert not f.closed
    assert f.closed
    sig2 = dss.file_signature(fixture_path(name))
    assert sig2 == sig
    assert set(sig) == {
        "version", "version_string", "byte_order", "number_records",
        "file_size_bytes", "dead_words", "number_deletes",
    }


@pytest.mark.parametrize("name", ALL)
def test_catalog(name):
    _, _, nlive, by_type, ndatasets = CATALOGS[name]
    cat = dss.get_catalog(fixture_path(name))
    assert len(cat) == nlive
    assert cat.record_type.value_counts().to_dict() == by_type
    assert list(cat.columns[:1]) == ["pathname"]
    # Same column dtype as RasDss's pd.DataFrame({'pathname': [str, ...]}).
    assert cat["pathname"].dtype == pd.DataFrame({"pathname": ["x"]})["pathname"].dtype
    assert cat["record_type"].dtype == np.int32
    assert set(cat["status"]) == {"live"}
    assert not cat["duplicate"].any() and not cat["recovered"].any()
    assert cat.attrs["anomalies"] == []
    assert all(p.startswith("/") and p.endswith("/") and p.count("/") == 7 for p in cat.pathname)
    assert len(first_paths(cat)) == ndatasets
    with dss.open(fixture_path(name)) as f:
        assert f.pathnames() == cat.pathname.tolist()
        full = f.catalog(all_entries=True)
        assert {"raw_status", "stored_pathname"} <= set(full.columns)
        assert len(full) >= len(cat)


def test_catalog_names_and_last_write():
    cat = dss.get_catalog(fixture_path("sample7.dss"))
    assert set(cat.record_type_name) == {"RTS", "PD"}
    assert cat.last_write.notna().all()  # DSS7 bins carry write times
    cat6 = dss.get_catalog(fixture_path("castro.dss"))
    assert cat6.last_write.isna().all()
    assert cat6.pathname.tolist() == [
        "/CASTRO VALLEY/OUTLET/FLOW/16JAN1973/10MIN/OBS/",
        "/CASTRO VALLEY/FIRE DEPT./PRECIP-INC/16JAN1973/10MIN/OBS/",
        "//TABLE 1/STORAGE-FLOW///TABLE/",
    ]


def test_dss6_pseudo_regular_catalog_shows_tilde():
    cat = dss.get_catalog(fixture_path("synth6.dss"))
    assert any("/~1DAY/" in p for p in cat.pathname)
    full = dss.open(fixture_path("synth6.dss")).catalog(all_entries=True)
    row = full[full.pathname.str.contains("/~1DAY/")].iloc[0]
    assert "/IR-YEAR/" in row.stored_pathname


def test_recovery_mode_is_a_no_op_on_undamaged_dss7():
    for name in DSS7:
        normal = dss.get_catalog(fixture_path(name))
        with dss.open(fixture_path(name), recover=True) as f:
            assert f.recover
            rec = f.catalog()
        pd.testing.assert_frame_equal(normal, rec)


# --- dataset reads ---------------------------------------------------------------------


def check_frame(df, pathname, path):
    assert list(df.columns) == ["value"]
    assert df["value"].dtype == np.float64
    assert isinstance(df.index, pd.DatetimeIndex)
    assert df.index.name == "datetime"
    assert df.index.dtype == "datetime64[s]"
    assert set(df.attrs) == ATTR_KEYS
    assert df.attrs["pathname"] == pathname
    assert df.attrs["dss_file"] == str(path.resolve()) or os.path.samefile(df.attrs["dss_file"], path)
    assert isinstance(df.attrs["interval"], int)
    assert df.index.is_monotonic_increasing


@pytest.mark.parametrize("name", ALL)
def test_every_dataset_reads(name):
    path = fixture_path(name)
    with dss.open(path) as f:
        paths = first_paths(f.catalog())
        nodata = 0
        for p in paths.values():
            try:
                df = f.read_timeseries(p)
            except DssPathnameNotFoundError:
                nodata += 1  # all-missing dataset (Muncie storage-area stages)
                continue
            check_frame(df, p, path)
            assert len(df) > 0
            # Trimmed: regular data starts and ends with a valid value.
            if df.attrs["interval"] > 0:
                assert not np.isnan(df.value.iloc[0]) and not np.isnan(df.value.iloc[-1])
        assert nodata == (2 if name == "Muncie.dss" else 0)


def test_reference_values_castro_and_sample7():
    # Values read with RasDss (Java Monolith 3.3.27) on copies, P09a.
    df = dss.read_timeseries(fixture_path("castro.dss"), "/CASTRO VALLEY/OUTLET/FLOW/16JAN1973/10MIN/OBS/")
    assert len(df) == 61
    assert df.index[0] == pd.Timestamp("1973-01-16 03:00")
    assert df.index[1] - df.index[0] == pd.Timedelta(minutes=10)
    assert df.value.iloc[:3].tolist() == [3.0, 3.0, 3.0]
    assert df.attrs["units"] == "CFS" and df.attrs["type"] == "INST-VAL" and df.attrs["interval"] == 10

    df = dss.read_timeseries(fixture_path("sample7.dss"), "/GREEN RIVER/GLENFIR/FLOW/01Apr1992/1Hour/OBS/")
    assert len(df) == 720
    assert df.index[0] == pd.Timestamp("1992-04-22 01:00")
    assert df.index[-1] == pd.Timestamp("1992-05-22 00:00")  # 21MAY1992 2400
    assert df.value.iloc[:3].tolist() == [2411.0, 2400.0, 2380.0]
    assert df.attrs["interval"] == 60


def test_d_part_is_ignored_for_dataset_reads():
    path = fixture_path("sample7.dss")
    a = dss.read_timeseries(path, "/GREEN RIVER/GLENFIR/FLOW/01Apr1992/1Hour/OBS/")
    b = dss.read_timeseries(path, "/GREEN RIVER/GLENFIR/FLOW/01May1992/1Hour/OBS/")
    c = dss.read_timeseries(path, "/GREEN RIVER/GLENFIR/FLOW//1Hour/OBS/")
    d = dss.read_timeseries(path, "/green river/glenfir/flow//1HOUR/obs/")
    for x in (b, c, d):
        np.testing.assert_array_equal(a.index.asi8, x.index.asi8)
        np.testing.assert_array_equal(a.value.to_numpy(), x.value.to_numpy())


def test_dss6_and_dss7_copies_of_sample_agree():
    """sample7.dss is HEC's DSS7 version of sample.dss (DSS6): same datasets, stamps and
    metadata; the DSS7 copy has two edited values in one daily dataset."""
    changed = {}
    with dss.open(fixture_path("sample6.dss")) as f6, dss.open(fixture_path("sample7.dss")) as f7:
        p6 = first_paths(f6.catalog())
        p7 = first_paths(f7.catalog())
        assert set(p6) == set(p7)
        for k in p6:
            a, b = f6.read_timeseries(p6[k]), f7.read_timeseries(p7[k])
            np.testing.assert_array_equal(a.index.asi8, b.index.asi8)
            av, bv = a.value.to_numpy(), b.value.to_numpy()
            np.testing.assert_array_equal(np.isnan(av), np.isnan(bv))
            ndiff = int((av[~np.isnan(av)] != bv[~np.isnan(bv)]).sum())
            if ndiff:
                changed[k] = ndiff
            assert a.attrs["units"] == b.attrs["units"] and a.attrs["type"] == b.attrs["type"]
            assert a.attrs["interval"] == b.attrs["interval"]
    assert list(changed.values()) == [2]


@pytest.mark.parametrize("name", ["sample6.dss", "sample7.dss"])
def test_window_read(name):
    path = fixture_path(name)
    p = "/GREEN RIVER/GLENFIR/FLOW//1Hour/OBS/"
    whole = dss.read_timeseries(path, p)
    w = dss.read_timeseries(path, p, "01MAY1992 0000", "02MAY1992 2400")
    assert w.index[0] == pd.Timestamp("1992-05-01 00:00")
    assert w.index[-1] == pd.Timestamp("1992-05-03 00:00")
    assert len(w) == 49
    np.testing.assert_array_equal(w.value.to_numpy(), whole.loc[w.index[0] : w.index[-1], "value"].to_numpy())
    # datetime arguments and HEC strings agree; one-sided windows cut the whole dataset.
    w2 = dss.read_timeseries(path, p, pd.Timestamp("1992-05-01"), pd.Timestamp("1992-05-03"))
    pd.testing.assert_frame_equal(w, w2)
    tail = dss.read_timeseries(path, p, start="1992-05-10")
    assert tail.index[0] == pd.Timestamp("1992-05-10") and tail.index[-1] == whole.index[-1]
    head = dss.read_timeseries(path, p, end="1992-04-30T00:00")
    assert head.index[0] == whole.index[0] and head.index[-1] == pd.Timestamp("1992-04-30")
    with pytest.raises(ValueError):
        dss.read_timeseries(path, p, "1992-05-03", "1992-05-01")


def test_irregular_and_pseudo_regular_datasets():
    path = fixture_path("synth6.dss")
    df = dss.read_timeseries(path, "/SYN/IRR/STAGE//IR-MONTH/IRR/")
    assert df.attrs["interval"] == 0
    assert len(df) == 300
    with dss.open(path) as f:
        r = f.read_record_arrays("/SYN/IRR/STAGE/01JAN2001/IR-MONTH/IRR/")
        assert not r["regular"] and r["irregular_block"] == "IR-Month"
        assert r["record_type"] == 110
        pseudo = f.read_timeseries("/SYN/PSEUDO/FLOW//~1DAY/PSEUDO/")
        assert pseudo.attrs["interval"] == 0 and len(pseudo) == 500


def test_calendar_interval_attrs():
    path = fixture_path("synth6.dss")
    assert dss.read_timeseries(path, "/SYN/CAL/FLOW//1MON/MONTHLY/").attrs["interval"] == 43200
    assert dss.read_timeseries(path, "/SYN/CAL/FLOW//1YEAR/YEARLY/").attrs["interval"] == 525600


# --- missing values --------------------------------------------------------------------


def test_missing_markers_become_nan_and_raw_keeps_them():
    path = fixture_path("sample_vb.dss")
    p = "/MY BASIN/RIVERSIDE/FLOW//1HOUR/OBS/"
    df = dss.read_timeseries(path, p)
    raw = dss.read_timeseries(path, p, raw=True)
    nan = df.value.isna().to_numpy()
    assert nan.sum() == 51
    rv = raw.value.to_numpy()
    assert np.isin(rv[nan], [-901.0, -902.0, -FLT_MAX]).all()
    np.testing.assert_array_equal(rv[~nan], df.value.to_numpy()[~nan])
    pd.testing.assert_index_equal(df.index, raw.index)


def test_untrimmed_read_keeps_leading_missing():
    path = fixture_path("synth6.dss")
    p = "/SYN/CAL/FLOW//1MON/MONTHLY/"
    trimmed = dss.read_timeseries(path, p)
    with dss.open(path) as f:
        a = f.read_arrays(p, trim_missing=False)
    assert len(a["times"]) > len(trimmed)
    assert dss.missing_mask(a["values"])[0]


def test_missing_mask_widths():
    v32 = np.array([1.0, -901.0, -902.0, -FLT_MAX, np.nan, -900.0], dtype=np.float32)
    assert dss.missing_mask(v32).tolist() == [False, True, True, True, True, False]
    v64 = v32.astype(np.float64)
    assert dss.missing_mask(v64).tolist() == [False, True, True, True, True, False]
    assert not dss.missing_mask(np.array([-np.finfo(np.float64).max]))[0]


def test_record_read_keeps_stored_width_and_block_slots():
    with dss.open(fixture_path("sample7.dss")) as f:
        r = f.read_record_arrays("/GREEN RIVER/GLENFIR/FLOW/01Apr1992/1Hour/OBS/")
        assert r["values"].dtype == np.float32 and r["times"].dtype == np.int64
        assert len(r["values"]) == 720  # every hour of April
        block = f.read_record("/GREEN RIVER/GLENFIR/FLOW/01Apr1992/1Hour/OBS/")
        assert len(block) == 720 and block.index[0] == pd.Timestamp("1992-04-01 01:00")
        assert block.value.isna().sum() == int(dss.missing_mask(r["values"]).sum())
    with dss.open(fixture_path("Muncie.dss")) as f:
        rtd = f.catalog().query("record_type == 105").pathname.iloc[0]
        assert f.read_record_arrays(rtd)["values"].dtype == np.float64


def test_quality_and_extra_attrs():
    with dss.open(fixture_path("sample7.dss")) as f:
        df = f.read_timeseries("/GREEN RIVER/GLENFIR/FLOW//1Hour/OBS/", extra_attrs=True, include_quality=True)
        assert df.attrs["dss_version"] == 7 and df.attrs["record_type"] == 100
        assert {"time_zone", "time_offset_seconds", "blocks_found", "warnings"} <= set(df.attrs)


# --- exceptions ------------------------------------------------------------------------


def test_exception_hierarchy():
    for cls in (DssFormatError, DssUnsupportedRecordError, DssPathnameNotFoundError, DssLockedError):
        assert issubclass(cls, DssError)
    assert issubclass(DssError, RuntimeError)
    assert issubclass(DssPathnameNotFoundError, ValueError)
    assert dss.exceptions.DssError is DssError


@pytest.mark.parametrize("name", ALL)
def test_pathname_not_found(name):
    with dss.open(fixture_path(name)) as f:
        for p in ("/NO/SUCH/FLOW//1HOUR/RECORD/", "/NO/SUCH/FLOW/01JAN2000/1HOUR/RECORD/"):
            with pytest.raises(DssPathnameNotFoundError) as ei:
                f.read_timeseries(p)
            assert isinstance(ei.value, ValueError) and isinstance(ei.value, RuntimeError)
            with pytest.raises(DssPathnameNotFoundError):
                f.read_record(p)
        with pytest.raises(DssPathnameNotFoundError):
            f.read_timeseries("not a pathname")
        with pytest.raises(DssPathnameNotFoundError):
            f.read_timeseries("/A/B/C//NOT-AN-INTERVAL/F/")


@pytest.mark.parametrize("name", [n for n in ALL if 200 in CATALOGS[n][3]])
def test_paired_data_is_unsupported(name):
    with dss.open(fixture_path(name)) as f:
        pd_path = f.catalog().query("record_type == 200").pathname.iloc[0]
        with pytest.raises(DssUnsupportedRecordError) as ei:
            f.read_timeseries(pd_path)
        assert isinstance(ei.value, RuntimeError)
        with pytest.raises(DssUnsupportedRecordError):
            f.read_record(pd_path)


def test_missing_file_is_not_created(tmp_path):
    p = tmp_path / "nope.dss"
    with pytest.raises(FileNotFoundError):
        dss.open(p)
    with pytest.raises(FileNotFoundError):
        dss.get_catalog(p)
    with pytest.raises(FileNotFoundError):
        dss.file_signature(p)
    assert not p.exists()


@pytest.mark.parametrize("content", [b"", b"x" * 2000, b"\0" * 4096, b"ZDSS" + b"\0" * 1000])
def test_not_a_dss_file(tmp_path, content):
    p = tmp_path / "junk.dss"
    p.write_bytes(content)
    with pytest.raises(DssFormatError) as ei:
        dss.open(p)
    assert isinstance(ei.value, RuntimeError)
    assert p.read_bytes() == content


def test_truncated_dss7_raises_format_error(tmp_path):
    data = fixture_path("BaldEagleDamBrk.dss").read_bytes()
    p = tmp_path / "trunc.dss"
    p.write_bytes(data[: len(data) // 2])
    with pytest.raises(DssFormatError):
        with dss.open(p) as f:
            f.catalog()


def test_closed_file_raises_value_error():
    f = dss.open(fixture_path("castro.dss"))
    f.close()
    with pytest.raises(ValueError):
        f.catalog()
    with pytest.raises(ValueError):
        f.read_timeseries("/CASTRO VALLEY/OUTLET/FLOW//10MIN/OBS/")
    f.close()  # idempotent


# --- read-only and threads -------------------------------------------------------------


@pytest.mark.parametrize("name", ALL)
def test_reads_never_modify_the_file(name):
    path = fixture_path(name)
    before = fingerprint(path)
    listing = sorted(os.listdir(FIXTURES))
    for recover in (False, True):
        with dss.open(path, recover=recover) as f:
            cat = f.catalog(all_entries=True)
            for p in first_paths(cat).values():
                try:
                    f.read_timeseries(p)
                    f.read_record(p)
                    f.read_arrays(p, trim_missing=False)
                except DssPathnameNotFoundError:
                    pass
    assert fingerprint(path) == before
    assert sorted(os.listdir(FIXTURES)) == listing


def test_concurrent_reads_from_threads():
    path = fixture_path("sample7.dss")
    f = dss.open(path)
    try:
        paths = list(first_paths(f.catalog()).values())
        expected = {p: f.read_timeseries(p).value.to_numpy() for p in paths}

        def work():
            for p in paths:
                np.testing.assert_array_equal(f.read_timeseries(p).value.to_numpy(), expected[p])

        _thread_results(work, 4, timeout=30)
    except BaseException:
        # A native lock regression may also block close(). Daemon workers own the
        # handle until they finish; let the test fail without another lock wait.
        raise
    else:
        f.close()


def _thread_results(work, count, timeout):
    """Propagate even panic-style BaseExceptions, with one bounded wait for all work."""
    def run(future):
        try:
            future.set_result(work())
        except BaseException as exc:
            future.set_exception(exc)

    futures = [Future() for _ in range(count)]
    for future in futures:
        # Daemons cannot keep pytest alive when the native mutex is deadlocked.
        threading.Thread(target=run, args=(future,), daemon=True).start()
    _, pending = wait(futures, timeout=timeout)
    if pending:
        raise TimeoutError(f"{len(pending)} concurrent readers did not finish")
    return [future.result(timeout=0) for future in futures]


def test_thread_results_propagate_base_exception():
    class WorkerPanic(BaseException):
        pass

    def fail():
        raise WorkerPanic("worker failed")

    with pytest.raises(WorkerPanic, match="worker failed"):
        _thread_results(fail, 4, timeout=1)


def test_thread_results_timeout():
    release = threading.Event()
    try:
        with pytest.raises(TimeoutError, match="did not finish"):
            _thread_results(release.wait, 1, timeout=0.01)
    finally:
        release.set()


def test_unhandled_thread_warning_is_fatal():
    with pytest.raises(pytest.PytestUnhandledThreadExceptionWarning):
        warnings.warn("worker failed", pytest.PytestUnhandledThreadExceptionWarning)


# --- time helpers ----------------------------------------------------------------------


def test_to_hec_seconds():
    assert dss.to_hec_seconds("31DEC1899 0000") == 0
    assert dss.to_hec_seconds("01JAN1900") == 86400
    assert dss.to_hec_seconds("01JAN2020 2400") == dss.to_hec_seconds("02JAN2020 0000")
    assert dss.to_hec_seconds("1jan2020 13:30") == dss.to_hec_seconds(pd.Timestamp("2020-01-01 13:30"))
    assert dss.to_hec_seconds(np.datetime64("2020-01-01T00:00:15")) == dss.to_hec_seconds("2020-01-01") + 15
    with pytest.raises(ValueError):
        dss.to_hec_seconds("01JAN2020 2500")


# --- DSS6 blocks that disagree in units/type: the last block read wins (P09a B1, P09c) ----
# b1_multiblock6.dss was written by HEC Monolith 3.3.27 (oracle/synth_b1.py). Every dataset
# has three month blocks; for REG and IRR the (units, type) pairs are CFS/INST-VAL, cms/PER-AVER
# and ft3/s/INST-CUM, in time order. HEC re-reads the header for every block it reads
# (zrrtsi.f, zritsi6.f). The expected values are what Monolith returns for the same requests.

B1_REG = "/B1/REG/FLOW//1HOUR/MULTI/"
B1_UNI = "/B1/UNI/FLOW//1HOUR/UNIFORM/"
B1_IRR = "/B1/IRR/STAGE//IR-MONTH/MULTI/"


def _b1(pathname, start=None, end=None):
    with dss.open(fixture_path("b1_multiblock6.dss")) as f:
        df = f.read_timeseries(pathname, start, end, extra_attrs=True)
    return df, (df.attrs["units"], df.attrs["type"])


def _differs(df):
    return [w for w in df.attrs["warnings"] if "differs from the first block" in w]


def test_b1_regular_whole_dataset_reports_last_block_header():
    df, hdr = _b1(B1_REG)
    assert hdr == ("ft3/s", "INST-CUM")
    assert len(df) == 2160
    assert len(_differs(df)) == 2
    assert df["value"].iloc[0] == 100.0 and df["value"].iloc[744] == 200.0
    # The module-level one-call API gives the same answer.
    assert dss.read_timeseries(fixture_path("b1_multiblock6.dss"), B1_REG).attrs["units"] == "ft3/s"


def test_b1_window_reports_the_last_block_it_read():
    df, hdr = _b1(B1_REG, "2001-01-31T22:00", "2001-02-02T00:00")
    assert hdr == ("cms", "PER-AVER") and len(df) == 27
    df, hdr = _b1(B1_REG, "2001-01-05", "2001-01-06")
    assert hdr == ("CFS", "INST-VAL") and len(df) == 25 and not _differs(df)


def test_b1_irregular_reports_last_block_header():
    df, hdr = _b1(B1_IRR)
    assert hdr == ("ft3/s", "INST-CUM") and len(df) == 120
    assert df.index[0] == pd.Timestamp("2001-01-02 03:07")
    assert df.index[-1] == pd.Timestamp("2001-03-12 21:10")
    df, hdr = _b1(B1_IRR, "2001-01-20", "2001-02-10")
    assert hdr == ("cms", "PER-AVER") and len(df) == 29
    assert df.index[-1] == pd.Timestamp("2001-02-09 20:34")


def test_b1_uniform_headers_are_untouched():
    df, hdr = _b1(B1_UNI)
    assert hdr == ("CFS", "INST-VAL") and len(df) == 2160 and not _differs(df)

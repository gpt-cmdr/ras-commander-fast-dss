"""P76: calendar-interval time placement against HEC Monolith.

Fixtures, all written or read by HEC Monolith 3.3.27 (javaHeclib) through hms-commander's Java
writer and reader (provenance in ``fixtures/public/README.md``):

- ``p76_monolith_calendar6.dss`` / ``7.dss`` with ``p76_monolith_calendar.json``: 66 series per
  version (yearly across leap years, month, semi- and tri-month overflow days, weeks around
  decade blocks, day/hour/15-minute) and Monolith's own read of each.
- ``p76_hec_calendar_oracle.json.gz``: 7,994 series (every start day of 2019 and 2020 for every
  calendar interval, leap-year starts in 1904, 2000, 2016, 2020 and 2024, weeks around nine
  decade starts, fixed intervals), each written by the engine from its start and read back by
  Monolith (``hec_reads_engine_record``).

Checks:

1. The engine reads Monolith's records as Monolith reads them (DSS6 weekly: Monolith drops the
   first value; the engine returns it).
2. ``rts_value_times`` / ``rts6_value_times`` (the ``times=`` check) equal HEC's read-back stamps
   whenever HEC reads back ``n`` strictly increasing stamps, and refuse otherwise. So a series
   ``write_timeseries(times=...)`` accepts reads back in HEC at exactly the given times.
3. Reproductions of the two 0.1.2 calendar defects.
"""

from __future__ import annotations

import gzip
import json

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import _native

from conftest import fixture_path

EPOCH = np.datetime64("1899-12-31T00:00:00", "s")


def hs(t) -> int:
    return int((np.datetime64(pd.Timestamp(t).to_datetime64(), "s") - EPOCH).astype(np.int64))


def iso(s: int) -> str:
    return str(pd.Timestamp(EPOCH + np.timedelta64(int(s), "s")))


def value_times(v: int, pathname: str, start, n: int):
    fn = _native.rts_value_times if v == 7 else _native.rts6_value_times
    return [iso(x) for x in fn(pathname, hs(start), n)]


def _overflow_only(engine, monolith):
    """Every engine (time, value) Monolith does not return identically is a value the engine keeps
    at its own period's stamp after a shorter period (February) where HEC's chained read moves or
    drops it: the engine's stamp is on day 1-3 of a month, and the value is present once."""
    mono = dict((v, t) for t, v in monolith)
    for t, v in engine:
        if mono.get(v) != t:
            assert pd.Timestamp(t).day <= 3, (t, v, monolith)
    assert len({v for _, v in engine}) == len(engine)


@pytest.mark.parametrize("v", [6, 7])
def test_engine_reads_monolith_records_as_monolith(v):
    rows = [r for r in json.loads(fixture_path("p76_monolith_calendar.json").read_text())
            if r["version"] == v]
    assert len(rows) == 33
    with dss.open(fixture_path(f"p76_monolith_calendar{v}.dss")) as f:
        for r in rows:
            df = f.read_timeseries(r["pathname"])
            times = [str(t) for t in df.index]
            values = [float(x) for x in df.iloc[:, 0]]
            assert (times, values) == (r["engine_times"], r["engine_values"]), r
            eng = list(zip(times, values))
            mono = list(zip(r["monolith_times"], r["monolith_values"]))
            if v == 6 and r["e"] == "1WEEK" and eng[1:] == mono:
                continue  # Monolith's DSS6 weekly read drops the first value.
            if r["hec_overflow_move"]:
                assert v == 6, r
                _overflow_only(eng, mono)
            else:
                assert eng == mono, r


def _cases():
    raw = gzip.decompress(fixture_path("p76_hec_calendar_oracle.json.gz").read_bytes())
    return json.loads(raw)["cases"]


def test_times_check_matches_hec_read_back_for_every_case():
    """Accepted: HEC reads the engine's record at exactly the accepted stamps, and so does the
    engine. Refused: HEC's read is not n strictly increasing stamps, or HEC moves a value away
    from the period the engine keeps it in (February overflow, DSS6)."""
    cases = _cases()
    assert len(cases) == 7994
    seen = {"accepted": 0, "refused": 0}
    for c in cases:
        v, e, n = c["v"], c["e"], c["n"]
        hec = list(c["hec_reads_engine_record"])
        own = list(c["engine_reads_engine_record"])
        pn = f"/P76/HECREF/FLOW//{e}/K/"
        try:
            got = value_times(v, pn, c["start"], n)
        except ValueError:
            got = None
        drops_first = c.get("monolith_drops_first_weekly_value") and own[1:] == hec and own != hec
        if got is not None:
            seen["accepted"] += 1
            assert got == own, c
            assert got == hec or (drops_first and got[1:] == hec), c
        else:
            seen["refused"] += 1
            kept = len(hec) == n and all(a < b for a, b in zip(hec, hec[1:]))
            assert not kept or own != hec, c
    assert seen["refused"] >= 28


def test_engine_round_trip_equals_hec_read_back_sample(tmp_path):
    """Every 25th oracle case: the engine writes from the start and reads back HEC's stamps."""
    cases = _cases()[::25]
    files = {}
    for k, c in enumerate(cases):
        v, e, n = c["v"], c["e"], c["n"]
        f = files.get(v)
        if f is None:
            f = files[v] = tmp_path / f"r{v}.dss"
            dss.create(f, version=v, sync="none")
        pn = f"/P76/RT/FLOW//{e}/K{k}/"
        dss.write_timeseries(f, pn, np.arange(n, dtype=float) + 1.0,
                             start=pd.Timestamp(c["start"]).to_pydatetime(), units="CFS",
                             data_type="INST-VAL", version=v, sync="none")
        df = dss.read_timeseries(f, pn)
        got = [str(t) for t in df.index]
        assert got == c["engine_reads_engine_record"], c


@pytest.mark.parametrize("v", [7, 6])
def test_calendar_defect_repros(tmp_path, v):
    f = tmp_path / f"r{v}.dss"
    dss.create(f, version=v)
    ts = dict(units="CFS", data_type="INST-VAL", version=v)
    # Month-end start: a day of the month a later month lacks would read back with two
    # values at one time, so it is refused.
    with pytest.raises(ValueError):
        dss.write_timeseries(f, "/A/G/FLOW//1MON/OBS/", [1.0, 2.0, 3.0, 4.0],
                             times=["2019-01-30", "2019-03-02", "2019-03-30", "2019-04-30"], **ts)
    assert dss.get_catalog(f).empty
    # Leap year: yearly stamps HEC keeps across a leap year are accepted and read back exactly.
    keep = pd.DatetimeIndex(["2019-06-15", "2020-06-15", "2021-06-15"])
    dss.write_timeseries(f, "/A/G/FLOW//1YEAR/OBS/", [1.0, 2.0, 3.0], times=keep, **ts)
    assert list(dss.read_timeseries(f, "/A/G/FLOW//1YEAR/OBS/").index) == list(keep)


def test_weekly_decade_start_is_refused_on_dss7(tmp_path):
    f = tmp_path / "w.dss"
    dss.create(f)
    with pytest.raises(ValueError):
        dss.write_timeseries(f, "//G/FLOW//1WEEK/OBS/", [1.0, 2.0, 3.0],
                             times=pd.date_range("2019-12-30", periods=3, freq="7D"),
                             units="CFS", data_type="INST-VAL")


def test_dss6_leap_year_yearly_start_after_2_march_is_refused(tmp_path):
    """HEC reads a DSS6 yearly series from 2020-06-15 at 2020-06-16 (its stored offset is a day
    count from 1 January, which C reads past 29 February), so ``times=`` refuses it."""
    f = tmp_path / "y6.dss"
    dss.create(f, version=6)
    with pytest.raises(ValueError):
        dss.write_timeseries(f, "/A/G/FLOW//1YEAR/OBS/", [1.0, 2.0],
                             times=["2020-06-15", "2021-06-15"], units="CFS",
                             data_type="INST-VAL", version=6)
    assert value_times(6, "/A/G/FLOW//1YEAR/OBS/", "2020-06-15", 2)[0] == "2020-06-16 00:00:00"


def _missing_cases():
    raw = gzip.decompress(fixture_path("p76_hec_missing_oracle.json.gz").read_bytes())
    return json.loads(raw)["cases"]


def test_missing_values_times_check_against_hec():
    """Leading (1-3), trailing, interior and leading+trailing missing values,
    every calendar interval, day and hour, DSS6 and DSS7 (14,138 HEC Monolith cases). Whenever
    ``times=`` accepts a series, HEC Monolith and the engine read every finite value at its
    accepted time; a series whose value HEC moves away from its own period is refused."""
    cases = _missing_cases()
    assert len(cases) == 14138
    accepted = 0
    for c in cases:
        v, e, n = c["v"], c["e"], c["n"]
        miss = set(c["missing"])
        fv = next((i for i in range(n) if i not in miss), n)
        fn = _native.rts_value_times if v == 7 else _native.rts6_value_times
        try:
            kept = [iso(x) for x in fn(f"/P76/MISSING/FLOW//{e}/K/", hs(c["start"]), n, first_valid=fv)]
        except ValueError:
            kept = None
        assert (kept is not None) == c["accepted"], c
        if c["hec_overflow_move"]:
            assert kept is None, c
        if kept is None:
            continue
        accepted += 1
        given = [[kept[i - fv], float(i + 1)] for i in range(fv, n) if i not in miss]
        assert given == c["engine_read"], c
        hec = c["hec_reads_engine_record"]
        drops_first = v == 6 and e == "1WEEK" and given[1:] == hec and given != hec
        assert given == hec or drops_first, c
    assert accepted > 14000


def test_missing_values_round_trip_sample(tmp_path):
    """Every 20th missing-value case: the engine writes it from its start, and its trimmed and
    untrimmed reads give the recorded finite (time, value) pairs."""
    cases = _missing_cases()[::20]
    files = {}
    for k, c in enumerate(cases):
        v, e, n = c["v"], c["e"], c["n"]
        f = files.get(v)
        if f is None:
            f = files[v] = tmp_path / f"m{v}.dss"
            dss.create(f, version=v, sync="none")
        vals = np.array([np.nan if i in set(c["missing"]) else float(i + 1) for i in range(n)])
        pn = f"/P76/RTM/FLOW//{e}/K{k}/"
        dss.write_timeseries(f, pn, vals, start=pd.Timestamp(c["start"]).to_pydatetime(),
                             units="CFS", data_type="INST-VAL", version=v, sync="none")
        with dss.open(f) as h:
            for trim in (True, False):
                df = h.read_timeseries(pn, trim_missing=trim)
                got = [[str(t), float(x)] for t, x in zip(df.index, df.iloc[:, 0]) if x == x]
                assert got == c["engine_read"], (c, trim)


@pytest.mark.parametrize("v", [6, 7])
def test_monthly_leading_missing_from_month_end(tmp_path, v):
    """A monthly series from 2019-01-31 with a leading missing value. The
    ``times=`` write is refused (HEC moves its values); a ``start=`` write reads the same finite
    (time, value) pairs trimmed and untrimmed."""
    pn = "/MONTHEND/MISSING/FLOW//1MON/K0/"
    f = tmp_path / f"r{v}.dss"
    dss.create(f, version=v)
    with pytest.raises(ValueError):
        dss.write_timeseries(f, pn, [np.nan, 2.0, 3.0, 4.0],
                             times=["2019-01-31", "2019-03-03", "2019-03-31", "2019-05-01"],
                             units="CFS", data_type="INST-VAL", version=v)
    dss.write_timeseries(f, pn, [np.nan, 2.0, 3.0, 4.0], start="2019-01-31", units="CFS",
                         data_type="INST-VAL", version=v)
    with dss.open(f) as h:
        a = h.read_timeseries(pn)
        b = h.read_timeseries(pn, trim_missing=False)
    fin = lambda df: [(str(t)[:10], float(x)) for t, x in zip(df.index, df.iloc[:, 0]) if x == x]  # noqa: E731
    assert fin(a) == fin(b)
    if v == 6:
        assert fin(a) == [("2019-03-03", 2.0), ("2019-03-31", 3.0), ("2019-05-01", 4.0)]


# -- Raw NaNs are kept by reads, so ``times=`` must keep them too ---------------------------


def _raw_write(f, pn, values, times, v):
    dss.write_timeseries(f, pn, values, times=times, units="CFS", data_type="INST-VAL",
                         version=v, raw=True)


def _read_pairs(f, pn):
    with dss.open(f) as h:
        df = h.read_timeseries(pn)
    return [(str(t), float(x)) for t, x in zip(df.index, df.iloc[:, 0])]


def _same_pairs(got, want):
    """(time, value) pairs equal, NaN equal to NaN."""
    assert [t for t, _ in got] == [t for t, _ in want], (got, want)
    for (_, a), (_, b) in zip(got, want):
        assert (a != a and b != b) or a == b, (got, want)


@pytest.mark.parametrize("v", [6, 7])
def test_raw_all_nan_hourly_times_are_checked(tmp_path, v):
    """Two raw NaNs requested a day apart read back an hour apart, so the write is refused (it
    was accepted: every value counted as missing and no stamp was compared). Correct hourly
    stamps are accepted and read back as written."""
    f = tmp_path / f"a{v}.dss"
    dss.create(f, version=v)
    pn = "/RAW/ALLNAN/FLOW//1HOUR/T/"
    with pytest.raises(ValueError, match="not regular"):
        _raw_write(f, pn, [np.nan, np.nan], ["2019-01-01 01:00", "2019-01-02 01:00"], v)
    assert dss.get_catalog(f).empty
    _raw_write(f, pn, [np.nan, np.nan], ["2019-01-01 01:00", "2019-01-01 02:00"], v)
    _same_pairs(_read_pairs(f, pn), [("2019-01-01 01:00:00", np.nan),
                                     ("2019-01-01 02:00:00", np.nan)])


@pytest.mark.parametrize("v", [6, 7])
@pytest.mark.parametrize("nan_at", [(0,), (0, 1), (2,), (1, 3), (4,), (3, 4), (0, 2, 4)],
                         ids=["lead1", "lead2", "interior1", "interior2", "trail1", "trail2",
                              "mixed"])
@pytest.mark.parametrize("e,start", [("1HOUR", "2019-01-01 01:00"), ("1MON", "2019-01-15")])
def test_raw_nan_keeps_every_stamp(tmp_path, v, nan_at, e, start):
    """Leading, interior and trailing raw NaNs with finite values after them: the accepted
    ``times=`` are exactly the stamps a read returns for every value, NaN included, and a set of
    times shifted at the first finite value after a leading NaN is refused."""
    n = 5
    pn = f"/RAW/KEEP/FLOW//{e}/T/"
    stamps = value_times(v, pn, start, n)
    vals = [np.nan if i in nan_at else float(i + 1) for i in range(n)]
    f = tmp_path / f"k{v}.dss"
    dss.create(f, version=v)
    _raw_write(f, pn, vals, stamps, v)
    _same_pairs(_read_pairs(f, pn), list(zip(stamps, vals)))
    if nan_at[0] == 0:
        # The stamps the old check predicted from the first finite value: they move that value.
        fv = next(i for i in range(n) if i not in nan_at)
        fn = _native.rts_value_times if v == 7 else _native.rts6_value_times
        moved = stamps[:fv] + [iso(x) for x in fn(pn, hs(stamps[0]), n, first_valid=fv)]
        if moved != stamps:
            g = tmp_path / f"m{v}.dss"
            dss.create(g, version=v)
            with pytest.raises(ValueError, match="not regular"):
                _raw_write(g, pn, vals, moved, v)
            assert dss.get_catalog(g).empty


@pytest.mark.parametrize("v", [6, 7])
def test_raw_leading_nan_calendar_does_not_move_finite_value(tmp_path, v):
    """Reproduction: Tri-Month, raw ``[NaN, 1, 2, 3]`` at 2019-01-30, 02-10, 02-20,
    03-02. DSS7 accepted it and read the finite 3 back at 03-10 (eight days later). It is
    refused on both versions; on DSS7 the stamps a read returns are accepted instead and every
    value reads back at its own stamp."""
    pn = "/TEST/NAN/FLOW//Tri-Month/T/"
    vals = [np.nan, 1.0, 2.0, 3.0]
    f = tmp_path / f"c{v}.dss"
    dss.create(f, version=v)
    with pytest.raises(ValueError):
        _raw_write(f, pn, vals, ["2019-01-30", "2019-02-10", "2019-02-20", "2019-03-02"], v)
    assert dss.get_catalog(f).empty
    if v == 7:
        stamps = value_times(v, pn, "2019-01-30", 4)
        assert stamps[-1] == "2019-03-10 00:00:00"
        _raw_write(f, pn, vals, stamps, v)
        _same_pairs(_read_pairs(f, pn), list(zip(stamps, vals)))


@pytest.mark.parametrize("v", [6, 7])
def test_marker_leading_missing_still_trimmed(tmp_path, v):
    """Unchanged: without ``raw`` a leading NaN becomes a HEC marker, reads trim it, and the
    ``times=`` check compares only the kept stamps (from the first finite value)."""
    pn = "/RAW/MARKER/FLOW//1HOUR/T/"
    f = tmp_path / f"t{v}.dss"
    dss.create(f, version=v)
    stamps = value_times(v, pn, "2019-01-01 01:00", 3)
    dss.write_timeseries(f, pn, [np.nan, 2.0, 3.0], times=stamps, units="CFS",
                         data_type="INST-VAL", version=v)
    assert _read_pairs(f, pn) == [(stamps[1], 2.0), (stamps[2], 3.0)]


def test_raw_nan_times_check_matches_hec_read_back_for_every_case():
    """Oracle tie: a raw NaN is a kept value, so a series whose first value is a raw NaN gets
    the same ``times=`` verdict as the all-finite series, for every HEC Monolith calendar case
    (7,994 series). Where it is accepted, HEC Monolith reads the engine's record back at exactly
    those stamps (DSS6 weekly: after the first value, D8). The stamps the old check predicted
    from the first finite value are refused wherever they differ."""
    from ras_commander_fast_dss._write import _check_regular_times

    def verdict(fn, pn, t, vals):
        try:
            return _check_regular_times(fn, pn, t, vals)
        except ValueError:
            return None

    seen = {"accepted": 0, "refused": 0, "old_refused": 0}
    for c in _cases():
        v, e, n = c["v"], c["e"], c["n"]
        if n < 2:
            continue
        fn = _native.rts_value_times if v == 7 else _native.rts6_value_times
        pn = f"/P76/ORACLE/FLOW//{e}/K/"
        try:
            own = [hs(x) for x in value_times(v, pn, c["start"], n)]
        except ValueError:
            continue
        t = np.array(own, dtype=np.int64)
        finite = np.arange(1, n + 1, dtype=np.float64)
        raw = finite.copy()
        raw[0] = np.nan
        got = verdict(fn, pn, t, raw)
        assert got == verdict(fn, pn, t, finite), c
        if got is None:
            seen["refused"] += 1
            continue
        seen["accepted"] += 1
        hec = c["hec_reads_engine_record"]
        stamps = [iso(x) for x in own]
        d8 = c.get("monolith_drops_first_weekly_value")
        assert stamps == hec or (d8 and stamps[1:] == hec), c
        try:
            old = list(fn(pn, own[0], n, first_valid=1))
        except ValueError:
            continue  # the old prediction was itself refused: nothing it could accept
        shifted = np.array([own[0]] + old, dtype=np.int64)
        if not np.array_equal(shifted, t):
            assert verdict(fn, pn, shifted, raw) is None, c
            seen["old_refused"] += 1
    assert seen["accepted"] > 7000, seen
    assert seen["old_refused"] > 0, seen

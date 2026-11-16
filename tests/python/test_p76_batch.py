"""P76: ``write_batch`` / ``DssFile.write_batch`` and ``__version__``.

A batch must store what the same records stored by separate ``write_timeseries`` /
``write_paired_data`` calls store: with a fixed write clock the files are compared byte for
byte (DSS7 and DSS6, every ``sync`` mode, module-level and through a ``DssFile`` session),
and each report equals the separate call's return value. The batch takes one flush in the
default ``sync="end"``. Errors: an invalid or unsupported record raises before anything is
written; a record refused while the batch is written raises with ``batch_index`` and
``batch_reports``, after the records before it. Process death during a batch is covered by
the ``batch7`` / ``batch6`` cases of ``crates/dss-core/tests/p64_kill.rs``.

Synthetic data in ``tmp_path`` only.
"""

from __future__ import annotations

import importlib.metadata
import re

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import _native

TS = dict(units="CFS", data_type="INST-VAL")
PD = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")
CLOCK = 1_789_000_000_000


@pytest.fixture(autouse=True)
def fixed_clock():
    _native._set_write_clock(CLOCK)
    yield
    _native._set_write_clock(None)


def records():
    """Mixed records: new series over several blocks, a merge into a stored series, an
    unchanged rewrite, quality, irregular series (replace and new), paired data (replace
    with a width change, and new), NaN values, and one pathname written twice."""
    vals = np.arange(24 * 45, dtype=np.float32) * 0.5
    vals[5] = np.nan
    return [
        dict(kind="timeseries", pathname="/T/NEW/FLOW//1HOUR/S/", values=vals,
             start="20FEB2021 01:00", **TS),
        dict(kind="timeseries", pathname="/T/MULTI/FLOW//1HOUR/S/",
             values=np.arange(72, dtype=np.float32) + 7, start="21MAR2021 01:00", **TS),
        dict(kind="timeseries", pathname="/K/KEEP/FLOW//1HOUR/S/",
             values=np.arange(72, dtype=np.float32) + 7, start="01JAN2021 01:00", **TS),
        dict(kind="timeseries", pathname="/T/QUAL/FLOW//15MIN/S/",
             values=np.linspace(1000, 1010, 30), start="28FEB2021 22:00",
             quality=np.arange(30) % 4, units="CFS", data_type="PER-AVER"),
        dict(kind="paired_data", pathname="/T/PD/STAGE-FLOW///S/",
             x=np.array([0.0, 2.0, 5.0]), y=np.array([10.0, 20.0, 30.5]), **PD),
        dict(kind="paired_data", pathname="/T/NEWPD/STAGE-FLOW///S/",
             x=np.array([0.0, 1.0, 2.5, 4.0], dtype=np.float32),
             y=np.arange(8, dtype=np.float32).reshape(4, 2) * 1.25, **PD),
        dict(kind="timeseries", pathname="/T/IRR/STAGE//IR-MONTH/S/",
             values=np.array([7.5, 8.5], dtype=np.float32),
             times=["04APR2021 00:00", "06APR2021 01:00"], units="FT", data_type="INST-VAL"),
        dict(kind="timeseries", pathname="/T/NEWIRR/STAGE//IR-MONTH/S/",
             values=np.array([1.5, 2.5, 3.5]),
             times=["01MAY2021 00:01", "02MAY2021 00:00", "03JUN2021 01:00"],
             units="FT", data_type="INST-VAL"),
        dict(kind="timeseries", pathname="/T/NEW/FLOW//1HOUR/S/",
             values=np.array([9.0, 8.0, 7.0], dtype=np.float32), start="10MAR2021 00:00", **TS),
    ]


def base(tmp_path, v):
    f = tmp_path / f"base{v}.dss"
    dss.create(f, version=v, sync="none")
    k = dict(version=v, sync="none")
    seven = np.arange(72, dtype=np.float32) + 7
    dss.write_timeseries(f, "/K/KEEP/FLOW//1HOUR/S/", seven, start="01JAN2021 01:00", **TS, **k)
    dss.write_timeseries(f, "/T/MULTI/FLOW//1HOUR/S/", seven, start="20MAR2021 01:00", **TS, **k)
    dss.write_paired_data(f, "/T/PD/STAGE-FLOW///S/", np.array([0.0, 1.0], dtype=np.float32),
                          np.array([1.0, 2.0], dtype=np.float32), **PD, **k)
    dss.write_timeseries(f, "/T/IRR/STAGE//IR-MONTH/S/", np.array([1.0, 2.0], dtype=np.float32),
                         times=["02APR2021 00:00", "05APR2021 00:00"], units="FT",
                         data_type="INST-VAL", **k)
    return f


def separately(f, recs, **kw):
    out = []
    for r in recs:
        r = dict(r)
        kind = r.pop("kind")
        fn = dss.write_timeseries if kind == "timeseries" else dss.write_paired_data
        out.append(fn(f, **r, **kw))
    return out


@pytest.mark.parametrize("v", [7, 6])
@pytest.mark.parametrize("sync", ["end", "step", "none"])
def test_batch_equals_separate_calls(tmp_path, v, sync):
    b0 = base(tmp_path, v)
    a, b, c = (tmp_path / f"{x}.dss" for x in "abc")
    for x in (a, b, c):
        x.write_bytes(b0.read_bytes())
    sep = separately(a, records(), version=v, sync=sync)
    f0 = _native._flush_count()
    rb = dss.write_batch(b, records(), version=v, sync=sync)
    flushes = _native._flush_count() - f0
    with dss.DssFile(c) as s:
        rc = s.write_batch(records(), sync=sync)
        # Reads through the session see the batch.
        assert s.read_timeseries("/T/NEWIRR/STAGE//IR-MONTH/S/")["value"].tolist() == [1.5, 2.5, 3.5]
    assert a.read_bytes() != b0.read_bytes()
    assert b.read_bytes() == a.read_bytes()
    assert c.read_bytes() == a.read_bytes()
    assert rb == sep
    assert rc == sep
    outcomes = {blk["outcome"] for r in sep for blk in r["blocks"]}
    assert {"added", "replaced", "unchanged"} <= outcomes
    if sync == "end":
        assert flushes == 1
    elif sync == "none":
        assert flushes == 0


def test_session_batch_uses_the_held_handle(tmp_path):
    f = base(tmp_path, 7)
    with dss.DssFile(f) as s:
        s.write_timeseries("/Z/FIRST/FLOW//1HOUR/S/", np.ones(3), start="01JAN2021 01:00", **TS)
        opens = s._native._held_opens
        s.write_batch(records())
        assert s._native._held_opens == opens
        assert len(s.catalog()) > 0


@pytest.mark.parametrize("v", [7, 6])
def test_record_refused_while_writing_stops_the_batch(tmp_path, v):
    b0 = base(tmp_path, v)
    recs = records()
    # Paired data onto a stored time series is refused (the writer never deletes another
    # record type), after the records before it are written.
    recs.insert(3, dict(kind="paired_data", pathname="/K/KEEP/FLOW/01JAN2021/1HOUR/S/",
                        x=[0.0, 1.0], y=[1.0, 2.0], **PD))
    a = tmp_path / "a.dss"
    a.write_bytes(b0.read_bytes())
    sep = separately(a, recs[:3], version=v)
    with pytest.raises(dss.DssUnsupportedRecordError) as single:
        separately(a, recs[3:4], version=v)
    b = tmp_path / "b.dss"
    b.write_bytes(b0.read_bytes())
    with pytest.raises(dss.DssUnsupportedRecordError) as err:
        dss.write_batch(b, recs, version=v)
    assert err.value.batch_index == 3
    assert err.value.batch_reports == sep
    assert str(err.value).replace(str(b), "F") == str(single.value).replace(str(a), "F")
    assert b.read_bytes() == a.read_bytes()


@pytest.mark.parametrize(
    "bad, exc, text",
    [
        (dict(kind="timeseries", pathname="/A/B/FLOW//1HOUR/S/", values=[1.0], start="01JAN2021",
              notes=["x"], **TS), ValueError, "write_batch stores"),
        (dict(kind="timeseries", pathname="/A/B/FLOW/TS-PATTERN/1MON/S/", values=[1.0] * 12,
              start="01JAN3000", **TS), ValueError, "write_batch stores"),
        (dict(kind="timeseries", pathname="/A/B/FLOW//1HOUR/S/", values=[1.0],
              start="01JAN2021 01:00", quality=np.zeros((1, 2), dtype=np.int32), **TS),
         ValueError, "write_batch stores"),
        (dict(kind="timeseries", pathname="/A/B/FLOW//1HOUR/S/", values=[1.0],
              start="01JAN2021 01:00", sync="none", **TS), TypeError, "whole batch"),
        (dict(kind="grid", pathname="/A/B/C////"), ValueError, '"kind"'),
        (["not", "a", "mapping"], TypeError, "mapping"),
        (dict(kind="timeseries", pathname="/A/B/FLOW//1HOUR/S/", values=[], start="01JAN2021",
              **TS), ValueError, "no values"),
    ],
)
def test_invalid_records_write_nothing(tmp_path, bad, exc, text):
    f = base(tmp_path, 7)
    before = f.read_bytes()
    recs = records()[:2] + [bad] + records()[2:]
    with pytest.raises(exc, match=re.escape(text)) as err:
        dss.write_batch(f, recs)
    assert err.value.batch_index == 2
    assert err.value.batch_reports == []
    assert f.read_bytes() == before


def test_empty_batch_touches_nothing(tmp_path):
    missing = tmp_path / "missing.dss"
    assert dss.write_batch(missing, []) == []
    assert not missing.exists()


def test_batch_validation_errors_match_single_writes(tmp_path):
    f = base(tmp_path, 7)
    rec = dict(pathname="/A/B/FLOW//1HOUR/S/", values=[1.0, np.inf], start="01JAN2021 01:00", **TS)
    with pytest.raises(ValueError) as single:
        dss.write_timeseries(f, **rec)
    with pytest.raises(ValueError) as batch:
        dss.write_batch(f, [dict(kind="timeseries", **rec)])
    assert str(batch.value) == str(single.value)


def test_feature_flag():
    assert "write_batch" in dss.FEATURES
    assert "write_batch" in dss.__all__


def test_version_matches_the_distribution():
    assert isinstance(dss.__version__, str)
    assert re.fullmatch(r"\d+\.\d+\.\d+([.-]?\w+)*", dss.__version__)
    assert dss.__version__ == _native.__version__
    assert "__version__" in dss.__all__
    assert dss.__version__ == importlib.metadata.version("ras-commander-fast-dss")


def test_wrong_kind_reads(tmp_path, caplog):
    """A record of another kind raises an error that is
    also a ValueError, and the compat paired-data reader logs it at WARNING, not ERROR."""
    import logging

    f = tmp_path / "k.dss"
    dss.create(f)
    dss.write_paired_data(f, "//G/SF///T/", [0.0, 1.0], [1.0, 2.0], **PD)
    dss.write_timeseries(f, "/A/B/FLOW//1HOUR/T/", [1.0, 2.0], start="01JAN2021 01:00", **TS)
    with pytest.raises(dss.DssUnsupportedRecordError) as e:
        dss.read_timeseries(f, "//G/SF///T/")
    assert isinstance(e.value, ValueError)
    assert isinstance(e.value, RuntimeError) and isinstance(e.value, dss.DssError)
    with caplog.at_level(logging.DEBUG, logger="ras_commander_fast_dss"):
        assert dss.read_paired_data_compat(f, "/A/B/FLOW/01JAN2021/1HOUR/T/") is None
    levels = [r.levelname for r in caplog.records if r.name.startswith("ras_commander_fast_dss")]
    assert levels == ["WARNING"]

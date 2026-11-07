"""P38: writer variants through the Python API: multi-word quality, integer and character
notes, patterns, pseudo-regular data, on both DSS versions where the format has them, and
HEC-written fixtures read back and rewritten. Synthetic data only."""
from pathlib import Path

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss.exceptions import DssUnsupportedRecordError

FIX = Path(__file__).resolve().parents[2] / "fixtures" / "public"


@pytest.fixture
def new7(tmp_path):
    return dss.create(tmp_path / "v7.dss")


@pytest.fixture
def new6(tmp_path):
    return dss.create(tmp_path / "v6.dss", version=6)


def rec(path, pathname):
    with dss.open(path) as f:
        return f.read_record_arrays(pathname)


def test_features():
    for name in ("write_ts_quality_words", "write_ts_notes", "write_ts_patterns", "write_ts_pseudo_regular"):
        assert name in dss.FEATURES


def test_quality_words_regular(new7):
    q = np.arange(48, dtype=np.int32).reshape(24, 2)
    r = dss.write_timeseries(new7, "/P/Q/FLOW//1Hour/A/", np.arange(24.0, dtype=np.float32), start="01JAN2001 0100",
                             units="CFS", data_type="INST-VAL", quality=q)
    assert r["blocks"][0]["outcome"] == "added"
    d = rec(new7, "/P/Q/FLOW/01Jan2001/1Hour/A/")
    assert d["quality_element_size"] == 2
    assert np.array_equal(np.asarray(d["quality_words"])[:48], q.reshape(-1))


def test_quality_words_irregular_merge(new7):
    t = pd.date_range("2001-01-02", periods=6, freq="D")
    q = np.arange(18).reshape(6, 3)
    p = "/P/Q/FLOW//IR-Month/B/"
    dss.write_timeseries(new7, p, np.arange(6.0), times=t, units="CFS", data_type="INST-VAL", quality=q)
    dss.write_timeseries(new7, p, [50.0, 51.0], times=t[1:3], units="CFS", data_type="INST-VAL",
                         quality=np.array([[1, 2, 3], [4, 5, 6]]))
    d = rec(new7, "/P/Q/FLOW/01Jan2001/IR-Month/B/")
    assert np.asarray(d["values"]).tolist() == [0.0, 50.0, 51.0, 3.0, 4.0, 5.0]
    w = np.asarray(d["quality_words"]).reshape(6, 3)
    assert w[0].tolist() == [0, 1, 2]
    assert w[1].tolist() == [1, 2, 3]
    assert w[5].tolist() == [15, 16, 17]


def test_character_notes_round_trip_and_keep(new7):
    n = [f"note {i}" if i % 3 else "" for i in range(6)]
    p = "/P/N/FLOW//1Hour/C/"
    dss.write_timeseries(new7, p, np.arange(6.0, dtype=np.float32), start="01JAN2001 0100", units="CFS",
                         data_type="INST-VAL", notes=n)
    d = rec(new7, "/P/N/FLOW/01Jan2001/1Hour/C/")
    assert d["cnotes"][:6] == n
    # A write without notes keeps stored character notes, as heclib does.
    dss.write_timeseries(new7, p, [9.0], start="01JAN2001 0300", units="CFS", data_type="INST-VAL",
                         value_type="float")
    d = rec(new7, "/P/N/FLOW/01Jan2001/1Hour/C/")
    assert d["cnotes"][:6] == n
    assert float(np.asarray(d["values"])[2]) == 9.0


def test_integer_notes_and_quality(new7):
    ine = np.arange(40).reshape(10, 4) - 7
    t = pd.date_range("2001-03-03 01:00", periods=10, freq="h")
    dss.write_timeseries(new7, "/P/N/FLOW//IR-Month/I/", np.arange(10.0), times=t, units="CFS", data_type="INST-VAL",
                         notes=ine, quality=np.ones(10, dtype=np.int32))
    d = rec(new7, "/P/N/FLOW/01Mar2001/IR-Month/I/")
    assert d["inote_element_size"] == 4
    assert np.array_equal(np.asarray(d["inotes"]), ine.reshape(-1))
    assert np.asarray(d["quality"]).tolist() == [1] * 10


@pytest.mark.parametrize("kwargs, match", [
    (dict(notes=["a"]), "1 notes for 2"),
    (dict(notes=["a€", "b"]), "Latin-1"),
    (dict(notes=["a\0b", "c"]), "NUL"),
    (dict(quality=np.ones((3, 2))), "shape"),
    (dict(quality=np.full((2, 2), 0.5)), "integers"),
    (dict(notes=np.full((2, 2), 2**40)), "32 bits"),
    (dict(quality=np.ones((2, 65), dtype=np.int32)), "element size"),
])
def test_bad_arrays_refused(new7, kwargs, match):
    before = Path(new7).read_bytes()
    with pytest.raises(ValueError, match=match):
        dss.write_timeseries(new7, "/P/N/FLOW//1Hour/X/", np.ones(2), start="01JAN2001 0100", units="CFS",
                             data_type="INST-VAL", **kwargs)
    assert Path(new7).read_bytes() == before


def test_dss6_refuses_notes_and_quality_words(new6):
    for kw in (dict(notes=["a", "b"]), dict(quality=np.ones((2, 2), dtype=np.int32))):
        with pytest.raises(DssUnsupportedRecordError, match="DSS6"):
            dss.write_timeseries(new6, "/P/N/FLOW//1HOUR/X/", np.ones(2), start="01JAN2001 0100", units="CFS",
                                 data_type="INST-VAL", version=6, **kw)


def test_regular_pattern_both_versions(new7, new6):
    v = np.arange(12.0, dtype=np.float32)
    for f, ver, stored in ((new7, 7, "/P/P/FLOW/TS-PATTERN/1Month/R/"), (new6, 6, "/P/P/FLOW/TS-PATTERN/1MON/R/")):
        r = dss.write_timeseries(f, "/P/P/FLOW/TS-PATTERN/1MON/R/", v, units="CFS", data_type="PER-AVER", version=ver)
        assert r["blocks"][0]["record_type"] == 101
        d = rec(f, stored)
        assert d["pattern"] and np.array_equal(np.asarray(d["values"]), v)
        assert d["pattern_stored_offset_seconds"] == 0
        assert d["pattern_time_convention"] == "first-value-at-3000-01-01T00:00"
        # The reader's frame goes straight back: the P25 convention is the only time taken.
        with dss.open(f) as h:
            frame = h.read_timeseries(stored)
        r = dss.write_timeseries(f, stored, frame["value"].to_numpy(dtype=np.float32), times=frame.index,
                                 units="CFS", data_type="PER-AVER", version=ver)
        assert r["blocks"][0]["outcome"] == "unchanged"


def test_pattern_refusals(new7, new6):
    p = "/P/P/FLOW/TS-PATTERN/1Day/R/"
    kw = dict(units="CFS", data_type="PER-AVER")
    with pytest.raises(ValueError, match="positional"):
        dss.write_timeseries(new7, p, np.ones(2), start="01JAN2001 0100", **kw)
    with pytest.raises(ValueError, match="no quality or notes"):
        dss.write_timeseries(new7, p, np.ones(2), quality=[1, 2], **kw)
    with pytest.raises(ValueError, match="no quality or notes"):
        dss.write_timeseries(new7, p, np.ones(2), notes=["a", "b"], **kw)
    with pytest.raises(ValueError, match="needs times"):
        dss.write_timeseries(new7, "/P/P/FLOW/TS-PATTERN/IR-Day/I/", np.ones(2), **kw)
    with pytest.raises(ValueError, match="pseudo-regular"):
        dss.write_timeseries(new7, "/P/P/FLOW/TS-PATTERN/~1Day/I/", np.ones(2), times=["01JAN2001 0100", "01JAN2001 0200"], **kw)
    with pytest.raises(DssUnsupportedRecordError, match="floats"):
        dss.write_timeseries(new6, "/P/P/FLOW/TS-PATTERN/1DAY/R/", np.ones(2), version=6, value_type="double", **kw)


def test_hec_written_patterns_rewrite_word_for_word(tmp_path):
    src = FIX / "synthetic_irregular_variants7.dss"
    out = tmp_path / "copy.dss"
    dss.create(out)
    for path in ("/P32/PATTERN/FLOW/TS-PATTERN/IR-Year/IRR PATTERN/",
                 "/P32/PATTERN/FLOW/TS-PATTERN/IR-Day/IRR PATTERN DBL/",
                 "/P32/PATTERN/FLOW/TS-PATTERN/1Month/REG PATTERN/"):
        a = rec(src, path)
        t = None
        if not a["regular"]:
            t = np.datetime64("1899-12-31") + np.asarray(a["times"]).astype("timedelta64[s]")
        dss.write_timeseries(out, path, a["values"], times=t, units=a["units"], data_type=a["data_type"])
        b = rec(out, path)
        assert b["record_type"] == a["record_type"]
        assert np.array_equal(np.asarray(a["values"]), np.asarray(b["values"]))
        assert np.array_equal(np.asarray(a["times"]), np.asarray(b["times"]))


def test_hec_written_notes_read_and_rewrite(tmp_path):
    src = FIX / "synthetic_notes_quality7.dss"
    out = tmp_path / "n.dss"
    dss.create(out)
    path = "/P32/QUALITY/FLOW/01JAN2001/IR-Month/Q2 IRR/"
    a = rec(src, path)
    assert a["quality_element_size"] == 2 and a["cnotes"]
    t = np.datetime64("1899-12-31") + np.asarray(a["times"]).astype("timedelta64[s]")
    q = np.asarray(a["quality_words"]).reshape(-1, 2)
    dss.write_timeseries(out, path, a["values"], times=t, units=a["units"], data_type=a["data_type"], quality=q,
                         notes=a["cnotes"], time_granularity=60)
    b = rec(out, path)
    assert b["cnotes"] == a["cnotes"]
    assert np.array_equal(np.asarray(a["quality_words"]), np.asarray(b["quality_words"]))
    assert np.array_equal(np.asarray(a["values"]), np.asarray(b["values"]))


def test_pseudo_regular_both_versions(new7, new6):
    t = pd.date_range("2001-12-26", periods=10, freq="D")
    v = np.arange(10.0, dtype=np.float32)
    r = dss.write_timeseries(new7, "/P/S/FLOW//~1Day/A/", v, times=t, units="CFS", data_type="INST-VAL")
    assert [b["pathname"] for b in r["blocks"]] == ["/P/S/FLOW/01Jan2001/~1Day/A/", "/P/S/FLOW/01Jan2002/~1Day/A/"]
    with dss.open(new7) as f:
        a = f.read_arrays("/P/S/FLOW//~1Day/A/")
    assert np.array_equal(np.asarray(a["values"]), v)
    r = dss.write_timeseries(new6, "/P/S/FLOW//~1DAY/A/", v, times=t, units="CFS", data_type="INST-VAL", version=6)
    assert r["blocks"][0]["pathname"] == "/P/S/FLOW/01JAN2001/IR-YEAR/A/"
    with dss.open(new6) as f:
        a = f.read_arrays("/P/S/FLOW//~1DAY/A/")
        cat = f.catalog()
    assert np.array_equal(np.asarray(a["values"]), v)
    assert any("~1DAY" in p for p in cat["pathname"])


def test_hec_written_dss6_pseudo_matches_reference(new6):
    with dss.open(FIX / "synthetic_hec_variants6.dss") as f:
        a = f.read_arrays("/P33/PSEUDO/FLOW//~1HOUR/PS/")
    t = np.datetime64("1899-12-31") + np.asarray(a["times"]).astype("timedelta64[s]")
    dss.write_timeseries(new6, "/P33/PSEUDO/FLOW//~1HOUR/PS/", a["values"], times=t, units="CFS", data_type="INST-VAL",
                         version=6)
    with dss.open(new6) as f:
        b = f.read_arrays("/P33/PSEUDO/FLOW//~1HOUR/PS/")
    assert np.array_equal(np.asarray(a["values"]), np.asarray(b["values"]))
    assert np.array_equal(np.asarray(a["times"]), np.asarray(b["times"]))

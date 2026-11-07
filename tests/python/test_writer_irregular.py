"""P21: DSS7 irregular time-series writer, synthetic data only.

Every file is created in ``tmp_path``. Round trips read back through the native reader;
the HEC oracle comparisons (hecdss, Monolith, heclib C ``zcheckFile`` and word-level
comparisons with the heclib C writer) are local-only, see evidence/P21/REPORT.md.
"""

from __future__ import annotations

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss

MISSING = np.float32(-np.finfo(np.float32).max)
EPOCH = np.datetime64("1899-12-31T00:00:00", "s")


@pytest.fixture
def new_file(tmp_path):
    p = tmp_path / "irr.dss"
    dss.create(p)
    return p


def stamps(d):
    return EPOCH + d["times"].astype("timedelta64[s]")


def read(p, path):
    with dss.open(p) as f:
        return f.read_record_arrays(path)


def test_series_round_trip_across_blocks(new_file):
    idx = pd.date_range("2021-01-30 05:00", periods=200, freq="317min")
    s = pd.Series(np.arange(200, dtype=np.float32) * 0.5, index=idx)
    rep = dss.write_timeseries(new_file, "/P21/A/FLOW//IR-Month/SYN/", s, units="cfs", data_type="INST-VAL")
    assert [b["outcome"] for b in rep["blocks"]] == ["added"] * 3
    assert {b["record_type"] for b in rep["blocks"]} == {110}
    with dss.open(new_file) as f:
        df = f.read_timeseries("/P21/A/FLOW//IR-Month/SYN/")
    assert np.array_equal(df.index.values.astype("datetime64[s]"), idx.values.astype("datetime64[s]"))
    assert np.array_equal(df.iloc[:, 0].to_numpy(dtype=np.float32), s.to_numpy())


def test_midnight_and_seconds(new_file):
    times = ["01MAR2021 1200", "01MAR2021 2400", "02MAR2021 0001"]
    dss.write_timeseries(new_file, "/P21/M/FLOW//IR-Day/SYN/", np.array([1, 2, 3], np.float32), times=times,
                         units="cfs", data_type="INST-VAL")
    d = read(new_file, "/P21/M/FLOW/01Mar2021/IR-Day/SYN/")
    assert len(d["values"]) == 2  # 24:00 stays in its day's block
    t = pd.DatetimeIndex(["2025-06-01 23:59:31", "2025-06-01 23:59:38", "2025-06-02 00:00:05"])
    dss.write_timeseries(new_file, "/P21/S/FLOW//IR-Day/SYN/", np.ones(3), times=t, units="cfs", data_type="INST-VAL")
    with dss.open(new_file) as f:
        df = f.read_timeseries("/P21/S/FLOW//IR-Day/SYN/")
    assert list(df.index) == list(t)


def test_double_quality_missing_nan(new_file):
    times = pd.date_range("2021-05-01 01:00", periods=5, freq="h")
    v = np.array([1 / 3, -901.0, np.nan, 4.0, 5.0])
    dss.write_timeseries(new_file, "/P21/D/FLOW//IR-Day/SYN/", v, times=times, units="cfs", data_type="INST-VAL",
                         quality=[1, 2, 3, 4, 5])
    d = read(new_file, "/P21/D/FLOW/01May2021/IR-Day/SYN/")
    assert d["record_type"] == 115
    assert list(d["quality"]) == [1, 2, 3, 4, 5]
    assert d["values"][2] == float(MISSING)  # NaN is stored as HEC missing (-FLT_MAX)
    dss.write_timeseries(new_file, "/P21/R/FLOW//IR-Day/SYN/", np.array([np.nan], np.float32),
                         times=["01MAY2021 0100"], units="cfs", data_type="INST-VAL", raw=True)
    with dss.open(new_file) as f:
        raw = f.read_record_arrays("/P21/R/FLOW/01May2021/IR-Day/SYN/")
    assert np.isnan(raw["values"][0])


def test_replace_window_keeps_rest_and_deletes_empty_blocks(new_file):
    p = "/P21/W/FLOW//IR-Day/SYN/"
    times = pd.date_range("2021-03-01 01:00", periods=5 * 24 - 2, freq="h")
    dss.write_timeseries(new_file, p, np.arange(len(times), dtype=np.float32), times=times, units="cfs",
                         data_type="INST-VAL")
    rep = dss.write_timeseries(new_file, p, np.array([1.0, 2.0], np.float32),
                               times=["01MAR2021 2000", "05MAR2021 0300"], units="cfs", data_type="INST-VAL")
    assert [b["outcome"] for b in rep["blocks"]] == ["replaced", "deleted", "deleted", "deleted", "replaced"]
    with dss.open(new_file) as f:
        df = f.read_timeseries(p)
        assert "/P21/W/FLOW/02Mar2021/IR-Day/SYN/" not in set(f.pathnames())
    kept = times[(times < "2021-03-01 20:00") | (times > "2021-03-05 03:00")]
    assert len(df) == len(kept) + 2
    # A single value in the middle of a block replaces only its own time.
    rep = dss.write_timeseries(new_file, p, np.array([99.0], np.float32), times=["01MAR2021 1200"],
                               units="cfs", data_type="INST-VAL")
    assert rep["blocks"][0]["outcome"] == "replaced"
    with dss.open(new_file) as f:
        df2 = f.read_timeseries(p)
    assert len(df2) == len(df)
    assert df2.loc["2021-03-01 12:00"].iloc[0] == 99.0


def test_equal_rewrite_unchanged_and_granularity(new_file):
    p = "/P21/E/FLOW//IR-Year/SYN/"
    times = pd.date_range("2021-01-02", periods=10, freq="3D")
    v = np.arange(10, dtype=np.float32)
    dss.write_timeseries(new_file, p, v, times=times, units="cfs", data_type="INST-VAL", time_granularity=86400)
    rep = dss.write_timeseries(new_file, p, v, times=times, units="cfs", data_type="INST-VAL", time_granularity=86400)
    assert rep["blocks"][0]["outcome"] == "unchanged"
    with pytest.raises(ValueError, match="multiple"):
        dss.write_timeseries(new_file, p, v[:1], times=["05JAN2021 0130"], units="cfs", data_type="INST-VAL",
                             time_granularity=86400)
    with pytest.raises(ValueError, match="granularity"):
        dss.write_timeseries(new_file, p, v, times=times, units="cfs", data_type="INST-VAL", time_granularity=7)


@pytest.mark.parametrize(
    "kwargs, match",
    [
        (dict(times=["01MAR2021 0100", "01MAR2021 0100"]), "duplicate"),
        (dict(times=["01MAR2021 0200", "01MAR2021 0100"]), "ascending"),
        (dict(start="01MAR2021 0100"), "times="),
        (dict(), "times="),
        (dict(times=["01MAR2021 0100"]), "times for"),
    ],
)
def test_refusals(new_file, kwargs, match):
    with pytest.raises(ValueError, match=match):
        dss.write_timeseries(new_file, "/P21/F/FLOW//IR-Day/SYN/", np.ones(2), units="cfs", data_type="INST-VAL",
                             **kwargs)


def test_refused_pathnames_and_options(new_file):
    for p in ("/P21/F/FLOW//IR-Dy/SYN/", "/P21/F/FLOW/TS-PAT/IR-Dy/SYN/"):
        with pytest.raises(ValueError):
            dss.write_timeseries(new_file, p, np.ones(1), times=["01MAR2021 0100"], units="cfs",
                                 data_type="INST-VAL")
    with pytest.raises(ValueError, match="whole minute"):
        dss.write_timeseries(new_file, "/P21/F/FLOW//IR-Century/SYN/", np.ones(1),
                             times=[pd.Timestamp("2021-03-01 01:00:07")], units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError, match="irregular"):
        dss.write_timeseries(new_file, "/P21/F/FLOW//1Hour/SYN/", np.ones(1), start="01MAR2021 0100",
                             units="cfs", data_type="INST-VAL", time_granularity=60)
    with pytest.raises(ValueError, match="time-zone"):
        dss.write_timeseries(new_file, "/P21/F/FLOW//IR-Day/SYN/", np.ones(1),
                             times=pd.DatetimeIndex(["2021-03-01 01:00"], tz="UTC"), units="cfs",
                             data_type="INST-VAL")
    # P22: version=6 routes to the DSS6 writer, which refuses this DSS7 file.
    with pytest.raises(dss.DssUnsupportedRecordError):
        dss.write_timeseries(new_file, "/P21/F/FLOW//IR-Day/SYN/", np.ones(1), times=["01MAR2021 0100"],
                             units="cfs", data_type="INST-VAL", version=6)


def test_float_into_double_record(new_file):
    p = "/P21/FD/FLOW//IR-Day/SYN/"
    times = pd.date_range("2021-03-01 01:00", periods=10, freq="h")
    v = 1 / 3 + np.arange(10.0)
    dss.write_timeseries(new_file, p, v, times=times, units="cfs", data_type="INST-VAL")
    dss.write_timeseries(new_file, p, np.array([2.5], np.float32), times=["01MAR2021 1230"], units="cfs",
                         data_type="INST-VAL")
    d = read(new_file, "/P21/FD/FLOW/01Mar2021/IR-Day/SYN/")
    assert d["record_type"] == 115
    assert np.array_equal(d["values"][:10], v)  # stored doubles are kept exactly


def test_dssfile_method_reopens(new_file):
    with dss.open(new_file) as f:
        f.write_timeseries("/P21/O/FLOW//IR-Day/SYN/", np.ones(2), times=["01MAR2021 0100", "01MAR2021 0500"],
                           units="cfs", data_type="INST-VAL")
        assert "/P21/O/FLOW/01Mar2021/IR-Day/SYN/" in set(f.pathnames())


# -- HEC-written public fixture (heclib C 7-JA via evidence/P21/scripts/p21hec.c) ---------

FIXTURE = "p21_hec_its7.dss"
FIXTURE_SHA256 = "57829b397ce42bb3fea976cb01a76214c3647c362bab2f9ebccd9bce30c7660b"
SPEC = "p21_hec_its7_spec.txt"
SPEC_SHA256 = "f895faa6635f6f27d6f031cb317ba97b9cc8692da01038de2440da7f4681a9aa"


def parse_irreg_spec(text):
    """IRREG lines of the p21hec spec: (kind, path, units, type, granularity, times, values, quality)."""
    import struct

    t = text.split()
    i, out = 0, []
    while i < len(t):
        assert t[i] == "IRREG"
        kind, path, units, typ, n, gran, _flag, hq = t[i + 1:i + 9]
        n = int(n)
        i += 9
        times = np.array([int(x) for x in t[i:i + n]], np.int64)
        i += n
        fmt, dt = ("<f", np.float32) if kind == "float" else ("<d", np.float64)
        vals = np.array([struct.unpack(fmt, bytes.fromhex(h)[::-1])[0] for h in t[i:i + n]], dt)
        i += n
        q = None
        if hq == "1":
            q = np.array([int(x) for x in t[i:i + n]], np.int32)
            i += n
        out.append((kind, path, units.replace("~", ""), typ.replace("~", ""), int(gran), times, vals, q))
    return out


def masked_words(path):
    """File words with write/creation times and program names set to 0: header 12/13,
    bin +5, info 6/7/8/11 (dss7.md §2.2, §3.4, §5.1)."""
    import struct

    b = path.read_bytes()
    w = list(struct.unpack("<%dq" % (len(b) // 8), b))
    w[12] = w[13] = 0
    ht, mh, bs = w[19], w[15], w[23]
    for s in range(mh):
        a = w[ht + s]
        while a:
            ic = 0
            while ic + 7 < bs and w[a + ic] != 0:
                info = w[a + ic + 3]
                w[a + ic + 5] = 0
                for k in (6, 7, 8, 11):
                    w[info + k] = 0
                ic += 7 + (w[a + ic + 2] >> 32)
                if ic >= bs - 2:
                    break
            a = w[a + bs - 1]
    return w


def test_hec_irregular_fixture_hash_and_native_read():
    import hashlib

    from conftest import fixture_path

    fx, sp = fixture_path(FIXTURE), fixture_path(SPEC)
    assert hashlib.sha256(fx.read_bytes()).hexdigest() == FIXTURE_SHA256
    spec_bytes = sp.read_bytes().replace(bytes([13, 10]), bytes([10]))
    assert hashlib.sha256(spec_bytes).hexdigest() == SPEC_SHA256
    spec = parse_irreg_spec(sp.read_text())
    assert len(spec) == 18
    with dss.open(fx) as f:
        assert f.version_string == "7-JA"
        for kind, path, units, typ, _g, times, vals, q in spec:
            d = f.read_arrays(path, trim_missing=False)
            np.testing.assert_array_equal(d["times"], times)
            assert d["values"].dtype == vals.dtype
            # Stored bits, missing markers and NaN included (irregular blocks are never trimmed).
            np.testing.assert_array_equal(d["values"].view(np.uint8), vals.view(np.uint8))
            if q is not None:
                np.testing.assert_array_equal(d["quality"], q)
            assert d["units"] == units and d["data_type"] == typ


def test_native_irregular_write_equals_hec_write(tmp_path):
    """The same spec written natively into a new file is word-identical to the HEC-written
    fixture, except write/creation times and the program name."""
    from conftest import fixture_path

    out = tmp_path / "native.dss"
    dss.create(out)
    for kind, path, units, typ, g, times, vals, q in parse_irreg_spec(fixture_path(SPEC).read_text()):
        dss.write_timeseries(out, path, vals, times=EPOCH + times.astype("timedelta64[s]"), units=units,
                             data_type=typ, quality=q, raw=True, time_granularity=g)
    a, b = masked_words(fixture_path(FIXTURE)), masked_words(out)
    assert len(a) == len(b)
    assert [i for i, (x, y) in enumerate(zip(a, b)) if x != y] == []


# -- P21 review follow-ups ------------------------------------------------------------


def test_refused_multi_block_write_leaves_file_unchanged(tmp_path):
    """M1, m4: refusals of later blocks (notes, deletes of note blocks, multi-word
    quality on the regular path) are raised before any block is written."""
    import shutil

    from conftest import fixture_path

    # P38: stored notes and multi-word quality are merged now; a record with aliases is
    # still refused, here in the second block of the write.
    p = tmp_path / "al.dss"
    shutil.copyfile(fixture_path("synthetic_catalog_ops7.dss"), p)
    before = p.read_bytes()
    with pytest.raises(dss.DssUnsupportedRecordError):
        dss.write_timeseries(p, "/P32/ALIAS PRIMARY/FLOW//1Day/OBS/", np.ones(5, np.float32),
                             start="30DEC2000 2400", units="cfs", data_type="INST-VAL")
    assert p.read_bytes() == before
    # Explicit granularity refused by kept stored seconds in the second block.
    q = tmp_path / "g.dss"
    dss.create(q)
    dss.write_timeseries(q, "/G/G/FLOW//IR-Day/S/", np.ones(2, np.float32),
                         times=["02MAR2021 20:00:05", "02MAR2021 21:00:05"], units="cfs", data_type="INST-VAL")
    before = q.read_bytes()
    with pytest.raises(ValueError, match="multiples"):
        dss.write_timeseries(q, "/G/G/FLOW//IR-Day/S/", np.ones(3, np.float32),
                             times=["01MAR2021 0500", "01MAR2021 2300", "02MAR2021 0200"], units="cfs",
                             data_type="INST-VAL", time_granularity=60)
    assert q.read_bytes() == before


def test_input_type_refusals(new_file):
    p = "/P21/T/FLOW//IR-Day/SYN/"
    kw = dict(units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError, match="precision must be an integer"):
        dss.write_timeseries(new_file, p, np.ones(1), times=["01MAR2021 0100"], precision=2.7, **kw)
    with pytest.raises(ValueError, match="precision must be an integer"):
        dss.write_timeseries(new_file, "/P21/T/FLOW//1Hour/SYN/", np.ones(1), start="01MAR2021 0100",
                             precision=2.0, **kw)
    with pytest.raises(TypeError, match="pathname must be str"):
        dss.write_timeseries(new_file, b"/P21/T/FLOW//IR-Day/SYN/", np.ones(1), times=["01MAR2021 0100"], **kw)
    for bad in ([3823635600], np.array([3823635600]), np.array([1.5])):
        with pytest.raises(ValueError, match="not numbers"):
            dss.write_timeseries(new_file, p, np.ones(1), times=bad, **kw)
    with pytest.raises(ValueError, match="NaT"):
        dss.write_timeseries(new_file, p, np.ones(1), times=pd.DatetimeIndex([pd.NaT]), **kw)
    with pytest.raises(ValueError, match="not a time"):
        dss.write_timeseries(new_file, p, np.ones(1), times=[None], **kw)
    assert dss.open(new_file).catalog().empty

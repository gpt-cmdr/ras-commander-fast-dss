"""P66: DSS6 regular time-series merge/extend through the public Python API, and the
explicit opt-in for DSS6 irregular patterns. Synthetic data only.

- ``fixtures/public/p66_hec_ts6_merge_ops.tsv`` replayed with ``write_timeseries(...,
  version=6)`` into an empty DSS6 file reads back exactly as ``p66_hec_ts6_merge.dss``, which
  HEC Monolith 3.3.27 wrote from the same ops file (``zsrtsx``/``zsrtsxd``, IPLAN 0), apart from
  the one block HEC re-quantizes (``/P66/COMP3/``; see ``crates/dss-core/tests/dss6_merge.rs``).
- ``NaN`` maps to -901 on DSS6 unless ``raw=True``, also when merging.
"""
import datetime as _dt
from pathlib import Path

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss.exceptions import DssUnsupportedRecordError

FIX = Path(__file__).resolve().parents[2] / "fixtures" / "public"
REQUANTIZED = "/P66/COMP3/FLOW/01JAN2020/1HOUR/M/"


def _vals(s, double):
    out = []
    for t in s.split(","):
        t = t.strip()
        if t.startswith("0x"):
            bits = int(t[2:], 16)
            if double:
                out.append(np.array([bits], dtype=np.uint64).view(np.float64)[0])
            else:
                out.append(np.array([bits], dtype=np.uint32).view(np.float32)[0])
        else:
            out.append(float(t))
    return np.array(out, dtype=np.float64 if double else np.float32)


def _start(date, hhmm):
    d = _dt.datetime.strptime(date, "%d%b%Y")
    h = int(hhmm)
    return np.datetime64(d + _dt.timedelta(minutes=(h // 100) * 60 + h % 100), "s")


def _replay(out):
    lines = (FIX / "p66_hec_ts6_merge_ops.tsv").read_text(encoding="utf-8").splitlines()
    n = 0
    for line in lines:
        if not line or line.startswith("#"):
            continue
        c = line.split("\t")
        double = c[0] == "rtd"
        q = None if c[7] == "-" else [int(x) for x in c[7].split(",")]
        dss.write_timeseries(
            out, c[1], _vals(c[6], double), start=_start(c[2], c[3]), units=c[4],
            data_type=c[5], quality=q, version=6, raw=True,
            value_type="double" if double else "float",
        )
        n += 1
    return n


def _records(path):
    out = {}
    with dss.open(path) as f:
        for p in f.catalog()["pathname"]:
            a = f.read_record_arrays(p)
            out[p] = a
    return out


def test_public_api_merges_match_hec(tmp_path):
    out = dss.create(tmp_path / "py6.dss", version=6)
    assert _replay(out) == 60
    hec, ours = _records(FIX / "p66_hec_ts6_merge.dss"), _records(out)
    assert sorted(hec) == sorted(ours) and len(hec) == 44
    for p, a in hec.items():
        b = ours[p]
        assert a["record_type"] == b["record_type"], p
        assert np.array_equal(np.asarray(a["times"]), np.asarray(b["times"])), p
        assert (a["units"], a["data_type"]) == (b["units"], b["data_type"]), p
        qa, qb = a.get("quality"), b.get("quality")
        assert (qa is None) == (qb is None), p
        if qa is not None:
            assert np.array_equal(np.asarray(qa), np.asarray(qb)), p
        va, vb = np.asarray(a["values"]), np.asarray(b["values"])
        if p == REQUANTIZED:
            # HEC rounds the whole merged delta-compressed block to the caller's precision.
            assert not np.array_equal(va, vb)
            assert np.array_equal(va[:31], np.round(vb[:31].astype(np.float32)).astype(va.dtype))
            continue
        assert va.dtype == vb.dtype, p
        assert va.tobytes() == vb.tobytes(), p


def test_nan_maps_to_missing_on_merge(tmp_path):
    f = dss.create(tmp_path / "m6.dss", version=6)
    p = "/P66/NAN/FLOW//1HOUR/PY/"
    kw = dict(units="CFS", data_type="INST-VAL", version=6)
    dss.write_timeseries(f, p, np.arange(1.0, 25.0), start="31JAN2020 0100", **kw)
    # Overlaps the stored tail and extends into the February block.
    dss.write_timeseries(f, p, [np.nan, 5.5, np.nan, 7.5], start="31JAN2020 2300", **kw)
    with dss.open(f) as h:
        jan = h.read_record_arrays("/P66/NAN/FLOW/01JAN2020/1HOUR/PY/")
        feb = h.read_record_arrays("/P66/NAN/FLOW/01FEB2020/1HOUR/PY/")
    jv, fv = np.asarray(jan["values"]), np.asarray(feb["values"])
    # 31 January 01:00 is position 721 of 744.
    assert list(jv[720:742]) == [float(x) for x in range(1, 23)]
    assert jv[742] == -901.0 and jv[743] == 5.5
    assert fv[0] == -901.0 and fv[1] == 7.5 and (fv[2:] == -901.0).all()
    df = dss.read_timeseries(f, p, "31JAN2020 2000", "01FEB2020 0200", trim_missing=False)
    assert list(df["value"].isna()) == [False, False, False, True, False, True, False]


IRR_PATTERN = "/P66/IPAT/FLOW/TS-PATTERN/IR-YEAR/PY/"


def test_dss6_irregular_pattern_needs_opt_in(tmp_path):
    f = dss.create(tmp_path / "p6.dss", version=6)
    t = np.datetime64("3000-01-01T01:00") + np.arange(3).astype("timedelta64[h]")
    kw = dict(times=t, units="CFS", data_type="PER-AVER", version=6)
    before = Path(f).read_bytes()
    with pytest.raises(DssUnsupportedRecordError, match="opt-in"):
        dss.write_timeseries(f, IRR_PATTERN, [1.0, 2.0, 3.0], **kw)
    assert Path(f).read_bytes() == before
    with pytest.raises(TypeError):
        dss.write_timeseries(f, IRR_PATTERN, [1.0, 2.0, 3.0],
                             allow_unconfirmed_dss6_irregular_pattern=1, **kw)
    r = dss.write_timeseries(f, IRR_PATTERN, [1.0, 2.0, 3.0],
                             allow_unconfirmed_dss6_irregular_pattern=True, **kw)
    assert r["blocks"][0]["record_type"] == 111
    with dss.open(f) as h:
        # DssFile.write_timeseries passes the keyword through.
        with pytest.raises(DssUnsupportedRecordError):
            h.write_timeseries("/P66/IPAT2/FLOW/TS-PATTERN/IR-YEAR/PY/", [4.0], times=t[:1],
                               units="CFS", data_type="PER-AVER")
        h.write_timeseries("/P66/IPAT2/FLOW/TS-PATTERN/IR-YEAR/PY/", [4.0], times=t[:1],
                           units="CFS", data_type="PER-AVER",
                           allow_unconfirmed_dss6_irregular_pattern=True)
    assert "write_ts_dss6_irregular_pattern_opt_in" in dss.FEATURES


def test_opt_in_leaves_other_patterns_alone(tmp_path):
    f6 = dss.create(tmp_path / "r6.dss", version=6)
    f7 = dss.create(tmp_path / "r7.dss")
    kw = dict(units="CFS", data_type="PER-AVER")
    # Regular DSS6 patterns and DSS7 irregular patterns need no opt-in.
    dss.write_timeseries(f6, "/P66/R/FLOW/TS-PATTERN/1DAY/PY/", np.ones(3), version=6, **kw)
    t = np.datetime64("3000-01-01T01:00") + np.arange(2).astype("timedelta64[h]")
    dss.write_timeseries(f7, "/P66/I/FLOW/TS-PATTERN/IR-Day/PY/", np.ones(2), times=t, **kw)


@pytest.mark.parametrize("sync", ["end", "step", "none", True, False])
def test_irregular_pattern_opt_in_through_the_held_handle(tmp_path, sync):
    # P66: the opt-in reaches the held native handle of a DssFile session,
    # in every sync mode, and the default refuses there without writing.
    t = np.datetime64("3000-01-01T01:00") + np.arange(3).astype("timedelta64[h]")
    kw = dict(times=t, units="CFS", data_type="PER-AVER", sync=sync)
    a = dss.create(tmp_path / "path.dss", version=6)
    b = dss.create(tmp_path / "held.dss", version=6)
    dss._native._set_write_clock(1_600_000_000_000)
    try:
        dss.write_timeseries(a, IRR_PATTERN, [1.0, 2.0, 3.0], version=6,
                             allow_unconfirmed_dss6_irregular_pattern=True, **kw)
        with dss.open(b) as h:
            # A regular write first opens the held handle.
            h.write_timeseries("/P66/R/FLOW/TS-PATTERN/1DAY/PY/", np.ones(3), units="CFS",
                               data_type="PER-AVER", sync=sync)
            assert h._native._held_opens == 1
            before = Path(b).read_bytes()
            with pytest.raises(DssUnsupportedRecordError, match="opt-in"):
                h.write_timeseries(IRR_PATTERN, [1.0, 2.0, 3.0], **kw)
            assert Path(b).read_bytes() == before
            r = h.write_timeseries(IRR_PATTERN, [1.0, 2.0, 3.0],
                                   allow_unconfirmed_dss6_irregular_pattern=True, **kw)
            assert r["blocks"][0]["record_type"] == 111
            assert h._native._held_opens == 1
        dss.write_timeseries(a, "/P66/R/FLOW/TS-PATTERN/1DAY/PY/", np.ones(3), version=6,
                             units="CFS", data_type="PER-AVER", sync=sync)
    finally:
        dss._native._set_write_clock(None)
    with dss.open(a) as ha, dss.open(b) as hb:
        assert sorted(ha.catalog()["pathname"]) == sorted(hb.catalog()["pathname"])
        ra = ha.read_record_arrays(IRR_PATTERN)
        rb = hb.read_record_arrays(IRR_PATTERN)
        assert np.array_equal(np.asarray(ra["values"]), np.asarray(rb["values"]))

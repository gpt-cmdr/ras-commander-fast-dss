"""P22: DSS6 time-series writer (create, regular and irregular add, replace, merge).

Synthetic data only; every file is created in ``tmp_path`` or is a copy of a public
fixture. Round trips read back through the native reader; the Monolith oracle
comparisons are local-only (evidence/P22/REPORT.md).
"""

from __future__ import annotations

import os
import shutil
import struct
import subprocess
import sys

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssLockedError, DssUnsupportedRecordError

from conftest import fixture_path

EPOCH = np.datetime64("1899-12-31T00:00:00", "s")


def hec(s):
    return dss.to_hec_seconds(s)


def words6(b: bytes):
    """Logical DSS6 words (127 per 512-byte record; the 4-byte pad skipped)."""
    out = []
    for r in range(0, len(b), 512):
        rec = b[r:r + 508]
        out.extend(struct.unpack("<%di" % (len(rec) // 4), rec[: len(rec) // 4 * 4]))
    return out


@pytest.fixture
def new6(tmp_path):
    p = tmp_path / "w6.dss"
    assert dss.create(p, version=6) == p
    return p


def test_create_header_matches_monolith_layout(tmp_path):
    p = tmp_path / "e6.dss"
    dss.create(p, version=6)
    b = p.read_bytes()
    assert len(b) == 26_112
    w = words6(b)
    assert b[:4] == b"ZDSS" and b[16:20] == b"6-YO"
    # Non-zero words of an empty file created by Monolith 3.3.27 (6-YO), dates and times
    # (7..12) aside; 1-based address = index + 1.
    nonzero = {i: x for i, x in enumerate(w) if x and i not in (0, 4, 7, 8, 9, 10, 11, 12)}
    assert nonzero == {
        3: 6, 5: 6351, 14: 2048, 15: 1, 16: 32, 17: 32, 18: 127, 19: 2287, 20: 2287,
        44: 100, 45: 200, 46: 200, 47: 200, 48: 200, 49: 200, 2160: -9630, 6350: -9630,
    }
    with dss.open(p) as f:
        assert f.version == 6 and f.version_string == "6-YO" and f.catalog().empty
    with pytest.raises(FileExistsError):
        dss.create(p, version=6)


REGULAR = [
    ("1MIN", "01MAR2021 2301", 1500, 2),
    ("5MIN", "01MAR2021 2330", 30, 2),
    ("15MIN", "30JAN2021 0015", 400, 2),
    ("1HOUR", "30DEC2020 0100", 120, 2),
    ("6HOUR", "28DEC8999 0600", 30, 2),
    ("1DAY", "15DEC2019 2400", 800, 4),
    ("1WEEK", "26DEC2009 2400", 30, 2),
    ("1MON", "31JAN2018 2400", 40, 2),
    ("1YEAR", "31DEC1990 2400", 15, 2),
    ("SEMI-MONTH", "15JAN2029 2400", 30, 2),
    ("TRI-MONTH", "10JAN2029 2400", 40, 2),
    ("1Hour", "01JAN2020 0030", 50, 1),  # DSS7 name, 30-minute offset
]


@pytest.mark.parametrize("epart,start,n,blocks", REGULAR)
@pytest.mark.parametrize("dtype", [np.float32, np.float64])
def test_regular_round_trip(new6, epart, start, n, blocks, dtype):
    path = f"/P22/LOC/FLOW//{epart}/SYN/"
    vals = (100 + 50 * np.sin(np.arange(n) / 7.0)).astype(dtype)
    r = dss.write_timeseries(new6, path, vals, start=start, units="CFS", data_type="INST-VAL",
                             version=6)
    assert r["version"] == 6
    assert [b["outcome"] for b in r["blocks"]] == ["added"] * blocks
    assert {b["record_type"] for b in r["blocks"]} == {100 if dtype == np.float32 else 105}
    stored = path.upper().replace("1HOUR", "1HOUR")
    with dss.open(new6) as f:
        d = f.read_arrays(stored)
    np.testing.assert_array_equal(d["values"].astype(dtype), vals)
    expected = dss._native.rts6_value_times(path, hec(start), n)
    np.testing.assert_array_equal(d["times"], expected)
    assert d["times"][0] == hec(start)
    assert d["units"] == "CFS" and d["data_type"] == "INST-VAL"


def test_block_pathnames_full_blocks_and_upper_case(new6):
    r = dss.write_timeseries(new6, "/a/b/flow/01JAN1999/1Hour/f/", np.ones(30, np.float32),
                             start="31JAN2021 2200", units="cfs", data_type="INST-VAL",
                             version=6)
    assert [b["pathname"] for b in r["blocks"]] == [
        "/A/B/FLOW/01JAN2021/1HOUR/F/", "/A/B/FLOW/01FEB2021/1HOUR/F/",
    ]
    # Full blocks: KINDAT words and KILNDA values (every slot stored, -901 fill).
    assert [(b["number_data"], b["logical_number"]) for b in r["blocks"]] == [(744, 744), (672, 672)]
    with dss.open(new6) as f:
        rec = f.read_record_arrays("/A/B/FLOW/01JAN2021/1HOUR/F/")
    assert len(rec["values"]) == 744
    assert (rec["values"][:741] == -901.0).all() and (rec["values"][741:] == 1.0).all()


def test_nan_is_stored_as_minus_901(new6):
    path = "/A/B/FLOW//1HOUR/N/"
    dss.write_timeseries(new6, path, np.array([1.0, np.nan, 3.0], np.float32),
                         start="01JAN2021 0100", units="CFS", data_type="INST-VAL", version=6)
    with dss.open(new6) as f:
        rec = f.read_record_arrays("/A/B/FLOW/01JAN2021/1HOUR/N/")
        assert rec["values"][1] == -901.0
        df = f.read_timeseries(path)
    assert np.isnan(df["value"].iloc[1])
    # raw=True keeps the NaN bits.
    dss.write_timeseries(new6, path, np.array([np.nan], np.float32), start="01JAN2021 0400",
                         units="CFS", data_type="INST-VAL", raw=True, version=6)
    with dss.open(new6) as f:
        rec = f.read_record_arrays("/A/B/FLOW/01JAN2021/1HOUR/N/")
    assert np.isnan(rec["values"][3])


def test_merge_replace_and_unchanged(new6):
    path = "/A/B/FLOW//1DAY/M/"
    kw = dict(units="CFS", data_type="PER-AVER", version=6)
    dss.write_timeseries(new6, path, np.arange(10, dtype=np.float64), start="01JAN2021 2400", **kw)
    r = dss.write_timeseries(new6, path, np.array([50.0, 51.0]), start="04JAN2021 2400", **kw)
    assert [b["outcome"] for b in r["blocks"]] == ["replaced"]
    r = dss.write_timeseries(new6, path, np.array([50.0, 51.0]), start="04JAN2021 2400", **kw)
    assert [b["outcome"] for b in r["blocks"]] == ["unchanged"]
    with dss.open(new6) as f:
        d = f.read_arrays(path)
    np.testing.assert_array_equal(d["values"], [0, 1, 2, 50, 51, 5, 6, 7, 8, 9])


def test_quality_rules(new6):
    kw = dict(units="CFS", data_type="INST-VAL", version=6)
    p = "/A/B/FLOW//1HOUR/Q/"
    q = np.array([0, 3, -1, 0x7FFFFFFF, -(2**31)], dtype=np.int64)
    dss.write_timeseries(new6, p, np.arange(5, dtype=np.float32), start="01JAN2021 0100",
                         quality=q, **kw)
    # Without quality into a block with quality: the quality is kept, written slots get 0.
    dss.write_timeseries(new6, p, np.array([9.0], np.float32), start="01JAN2021 0200", **kw)
    with dss.open(new6) as f:
        rec = f.read_record_arrays("/A/B/FLOW/01JAN2021/1HOUR/Q/")
    np.testing.assert_array_equal(rec["quality"][:5], [0, 0, -1, 0x7FFFFFFF, -(2**31)])
    # Quality can be added to a float block.
    p2 = "/A/B/FLOW//1HOUR/Q2/"
    dss.write_timeseries(new6, p2, np.arange(5, dtype=np.float32), start="01JAN2021 0100", **kw)
    dss.write_timeseries(new6, p2, np.array([7.0], np.float32), start="01JAN2021 0300",
                         quality=[5], **kw)
    with dss.open(new6) as f:
        rec = f.read_record_arrays("/A/B/FLOW/01JAN2021/1HOUR/Q2/")
    np.testing.assert_array_equal(rec["quality"][:5], [0, 0, 5, 0, 0])
    # HEC refuses (status 511) quality with a width change; so does the writer.
    p3 = "/A/B/FLOW//1HOUR/Q3/"
    dss.write_timeseries(new6, p3, np.arange(5, dtype=np.float64), start="01JAN2021 0100", **kw)
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_timeseries(new6, p3, np.ones(2), start="01JAN2021 0100", quality=[1, 2], **kw)


def test_width_rules(new6):
    kw = dict(units="CFS", data_type="INST-VAL", version=6)
    # Double written into a float block: the block becomes double (zsrtsi6 case 3).
    p = "/A/B/FLOW//1DAY/W1/"
    dss.write_timeseries(new6, p, np.array([1.5, 2.5, 3.5], np.float32), start="01JAN2021 2400", **kw)
    r = dss.write_timeseries(new6, p, np.array([1.0 / 3]), start="02JAN2021 2400", **kw)
    assert r["blocks"][0]["record_type"] == 105
    with dss.open(new6) as f:
        d = f.read_arrays(p)
    assert d["values"].dtype == np.float64
    np.testing.assert_array_equal(d["values"], [1.5, 1.0 / 3, 3.5])
    # Float written into a double block: the block stays double (case 4).
    p = "/A/B/FLOW//1DAY/W2/"
    dss.write_timeseries(new6, p, np.array([1.0 / 3, 2.0 / 3]), start="01JAN2021 2400", **kw)
    r = dss.write_timeseries(new6, p, np.array([0.5], np.float32), start="02JAN2021 2400", **kw)
    assert r["blocks"][0]["record_type"] == 105
    with dss.open(new6) as f:
        d = f.read_arrays(p)
    np.testing.assert_array_equal(d["values"], [1.0 / 3, 0.5])


def test_irregular_round_trip_and_modes(new6):
    kw = dict(units="FT", data_type="INST-VAL", version=6)
    p = "/A/B/STAGE//IR-MONTH/I/"
    t = pd.to_datetime(["2021-01-30 23:00", "2021-01-31 00:00", "2021-02-01 06:07",
                        "2021-02-03 00:00"])
    r = dss.write_timeseries(new6, p, np.array([1.0, 2.0, 3.0, 4.0], np.float32), times=t, **kw)
    assert [b["pathname"] for b in r["blocks"]] == [
        "/A/B/STAGE/01JAN2021/IR-MONTH/I/", "/A/B/STAGE/01FEB2021/IR-MONTH/I/"]
    assert {b["record_type"] for b in r["blocks"]} == {110}
    with dss.open(new6) as f:
        d = f.read_arrays(p)
    np.testing.assert_array_equal(EPOCH + d["times"].astype("timedelta64[s]"),
                                  t.values.astype("datetime64[s]"))
    # Replace: values inside the window are replaced, the rest kept.
    dss.write_timeseries(new6, p, np.array([9.0], np.float32),
                         times=pd.to_datetime(["2021-02-02 00:00"]), **kw)
    with dss.open(new6) as f:
        d = f.read_arrays(p)
    np.testing.assert_array_equal(d["values"], [1, 2, 3, 9, 4])
    # Merge (native API): insert and replace at an equal time.
    tt = np.array([hec("01FEB2021 0607"), hec("02FEB2021 1200")], dtype=np.int64)
    dss._native.write_its6(str(new6), p, tt, np.array([7.0, 8.0], np.float32), "FT",
                           "INST-VAL", None, "merge")
    with dss.open(new6) as f:
        d = f.read_arrays(p)
    np.testing.assert_array_equal(d["values"], [1, 2, 7, 9, 8, 4])
    # Double irregular with quality, IR-CENTURY far dates.
    p2 = "/A/B/FLOW//IR-CENTURY/F/"
    t2 = pd.to_datetime(["1750-03-01 01:00", "1890-01-01 00:01", "1999-12-31 23:59"])
    dss.write_timeseries(new6, p2, np.array([1.0 / 3, 2.0, 3.0]), times=t2, quality=[1, 2, 3],
                         **kw)
    with dss.open(new6) as f:
        d = f.read_arrays(p2)
    np.testing.assert_array_equal(d["values"], [1.0 / 3, 2.0, 3.0])
    np.testing.assert_array_equal(d["quality"], [1, 2, 3])


def test_irregular_capacity_grows(new6):
    p = "/A/B/FLOW//IR-YEAR/BIG/"
    t = hec("01JAN2021 0001") + 3600 * np.arange(450, dtype=np.int64)
    dss.write_timeseries(new6, p, np.arange(450, dtype=np.float32),
                         times=(EPOCH + t.astype("timedelta64[s]")), units="CFS",
                         data_type="INST-VAL", version=6)
    with dss.open(new6) as f:
        rec = f.read_record_arrays("/A/B/FLOW/01JAN2021/IR-YEAR/BIG/")
    assert len(rec["values"]) == 450
    np.testing.assert_array_equal(rec["values"], np.arange(450, dtype=np.float32))


def test_dssfile_write_uses_the_file_version(new6):
    with dss.open(new6) as f:
        f.write_timeseries("/A/B/C//1DAY/F/", np.arange(3.0), start="01JAN2021 2400",
                           units="FT", data_type="INST-VAL")
        df = f.read_timeseries("/A/B/C//1DAY/F/")
        assert list(df["value"]) == [0.0, 1.0, 2.0]


@pytest.mark.parametrize(
    "path",
    [
        "/A/B/C//1HOUR/F",
        "/A/B/C//1HOU/F/",
        "/A/B/C//15Second/F/",
        "/A/B/C//~1DAY/F/",
        "/A/B/C/TS-PATTERN/1HOUR/F/",
        "/A/B/C/NOTADATE/1HOUR/F/",
        "/A/B/C//1HOUR/C:000001|X/",
        "/A/B\x01/C//1HOUR/F/",
        "/" + "A" * 129 + "/B/C//1HOUR/F/",
    ],
)
def test_pathname_refused(new6, path):
    with pytest.raises(ValueError):
        dss.write_timeseries(new6, path, np.ones(3), start="01JAN2021 0100", units="CFS",
                             data_type="INST-VAL", version=6)
    with dss.open(new6) as f:
        assert f.catalog().empty


def test_input_refused(new6):
    p = "/A/B/C//1HOUR/F/"
    kw = dict(units="CFS", data_type="INST-VAL", version=6)
    with pytest.raises(ValueError):  # not a whole minute
        dss.write_timeseries(new6, p, np.ones(3), start=hec("01JAN2021 0100") + 1, **kw)
    with pytest.raises(ValueError):  # units over 8 characters
        dss.write_timeseries(new6, p, np.ones(3), start="01JAN2021 0100", units="123456789",
                             data_type="INST-VAL", version=6)
    with pytest.raises(ValueError):
        dss.write_timeseries(new6, p, np.ones(3), start="01JAN2021 0100", time_zone="UTC", **kw)
    with pytest.raises(ValueError):  # irregular needs times=
        dss.write_timeseries(new6, "/A/B/C//IR-DAY/F/", np.ones(3), start="01JAN2021 0100", **kw)
    t = pd.to_datetime(["2021-01-01 01:00", "2021-01-01 02:00"])
    with pytest.raises(ValueError):  # -902 first: HEC's window marker
        dss.write_timeseries(new6, "/A/B/C//IR-DAY/F/", np.array([-902.0, 1.0]), times=t, **kw)
    with pytest.raises(ValueError):  # times decrease
        dss.write_timeseries(new6, "/A/B/C//IR-DAY/F/", np.ones(2), times=t[::-1], **kw)
    with pytest.raises(ValueError):
        dss.write_timeseries(new6, p, np.ones(3), start="01JAN2021 0100", version=5,
                             units="CFS", data_type="INST-VAL")
    with dss.open(new6) as f:
        assert f.catalog().empty


def test_version_mismatch_refused(tmp_path, new6):
    p7 = tmp_path / "w7.dss"
    dss.create(p7)
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_timeseries(p7, "/A/B/C//1HOUR/F/", np.ones(3), start="01JAN2021 0100",
                             units="CFS", data_type="INST-VAL", version=6)
    with pytest.raises(dss.DssError):
        dss.write_timeseries(new6, "/A/B/C//1HOUR/F/", np.ones(3), start="01JAN2021 0100",
                             units="CFS", data_type="INST-VAL", version=7)


HOLD6 = r"""
import os, sys
path, byte = sys.argv[1], int(sys.argv[2])
if os.name == "nt":
    import msvcrt
    fd = os.open(path, os.O_RDWR | os.O_BINARY)
    os.lseek(fd, byte, 0)
    msvcrt.locking(fd, msvcrt.LK_NBLCK, 4)
else:
    import fcntl
    fd = os.open(path, os.O_RDWR)
    fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 4, byte, 0)
print("READY", flush=True)
sys.stdin.read()
"""


# KLOCKB of a new file: lock record 18, byte 8704 (zopen6Int.f lines 410-427).
@pytest.mark.parametrize("byte,reason", [(8704, "primary_write"), (8704 + 16, "read_array"),
                                         (8704 + 8, "exclusive")])
def test_refused_while_hec_holds_a_lock_word(new6, byte, reason):
    before = new6.read_bytes()
    child = subprocess.Popen([sys.executable, "-I", "-c", HOLD6, str(new6), str(byte)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == "READY"
        with pytest.raises(DssLockedError) as e:
            dss.write_timeseries(new6, "/A/B/C//1HOUR/F/", np.ones(3), start="01JAN2021 0100",
                                 units="CFS", data_type="INST-VAL", version=6)
        assert reason in e.value.reasons
    finally:
        child.stdin.close()
        child.wait(timeout=30)
    assert new6.read_bytes() == before


def _records(path):
    """pathname -> (type, values bytes, quality) for every record."""
    out = {}
    with dss.open(path) as f:
        for p in f.catalog()["pathname"]:
            try:
                r = f.read_record_arrays(p)
            except dss.DssError:
                continue
            q = None if r["quality"] is None else r["quality"].tobytes()
            out[p] = (r["record_type"], r["values"].tobytes(), q, r["units"], r["data_type"])
    return out


@pytest.mark.parametrize("fixture", ["sample6.dss", "synthetic_hec_compress6.dss",
                                     "synthetic_hec_variants6.dss"])
def test_other_records_unchanged_on_fixture_copies(tmp_path, fixture):
    p = tmp_path / fixture
    shutil.copyfile(fixture_path(fixture), p)
    before = _records(p)
    with dss.open(p) as f:
        cat = f.catalog()
    target = None
    for row in cat.itertuples():
        if row.record_type in (100, 105) and "~" not in row.pathname:
            target = row.pathname
            break
    assert target is not None
    with dss.open(p) as f:
        rec = f.read_record_arrays(target)
    vals = rec["values"].copy()
    vals[0] = 12345.0
    start = pd.Timestamp(EPOCH + np.timedelta64(int(rec["times"][0]), "s"))
    r = dss.write_timeseries(p, target, vals[:1], start=start, units=rec["units"][:8],
                             data_type=rec["data_type"][:8], version=6, raw=True)
    assert r["blocks"][0]["outcome"] == "replaced"
    after = _records(p)
    assert set(after) == set(before)
    changed = [k for k in before if before[k] != after[k]]
    assert changed == [target]
    t, vb, q, *_ = after[target]
    a = np.frombuffer(vb, dtype=np.float32 if t == 100 else np.float64)
    b = np.frombuffer(before[target][1], dtype=a.dtype)
    assert a[0] == 12345.0
    np.testing.assert_array_equal(a[1:], b[1:])


def test_refused_multi_block_write_changes_nothing(new6):
    """Review M1: a February block stored as double with quality, then float values with
    no quality written from 31 January: HEC refuses the February block (zsrtsi6 status
    511), and the January block is not written either."""
    kw = dict(units="CFS", data_type="INST-VAL", version=6)
    dss.write_timeseries(new6, "/A/B/C//1HOUR/F/", np.array([1.0, 2.0]), start="01FEB2020 0100",
                         quality=[3, 4], **kw)
    before = new6.read_bytes()
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_timeseries(new6, "/A/B/C//1HOUR/F/", np.full(30, 9.0, np.float32),
                             start="31JAN2020 0100", **kw)
    assert new6.read_bytes() == before
    # Irregular: a replace whose window spans a stored block with no new value in it is
    # refused (HEC would delete the block); the first block is not written either.
    p = "/A/B/S//IR-DAY/G/"
    t = pd.to_datetime(["2020-01-01 01:00", "2020-01-02 01:00", "2020-01-03 01:00"])
    dss.write_timeseries(new6, p, np.array([1.0, 2.0, 3.0], np.float32), times=t, **kw)
    before = new6.read_bytes()
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_timeseries(new6, p, np.array([5.0, 6.0], np.float32),
                             times=pd.to_datetime(["2020-01-01 02:00", "2020-01-03 02:00"]), **kw)
    assert new6.read_bytes() == before


def test_precision_must_be_an_integer(new6):
    with pytest.raises(ValueError):
        dss.write_timeseries(new6, "/A/B/C//1HOUR/F/", np.ones(2), start="01JAN2020 0100",
                             units="CFS", data_type="INST-VAL", precision=2.5, version=6)

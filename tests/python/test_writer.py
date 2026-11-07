"""P20: DSS7 regular time-series writer (create, add, replace), synthetic data only.

Every file is created in ``tmp_path``; nothing is written to the public fixtures.
Round trips read back through the native reader; the HEC oracle comparisons (hecdss,
Monolith, heclib C ``zcheckFile``) are local-only, see evidence/P20/REPORT.md.
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

MISSING = np.float32(-np.finfo(np.float32).max)
EPOCH = np.datetime64("1899-12-31T00:00:00", "s")


def hec(s):
    return dss.to_hec_seconds(s)


@pytest.fixture
def new_file(tmp_path):
    p = tmp_path / "w.dss"
    assert dss.create(p) == p
    return p


def stamps(d):
    return EPOCH + d["times"].astype("timedelta64[s]")


def test_create_header(tmp_path):
    p = tmp_path / "empty.dss"
    dss.create(p)
    b = p.read_bytes()
    assert len(b) == 126_192
    words = struct.unpack("<15774q", b)
    assert b[:4] == b"ZDSS" and b[16:20] == b"7-JA"
    nonzero = {i: w for i, w in enumerate(words) if w and i not in (0, 2, 12, 13)}
    assert nonzero == {
        1: 100, 5: 15773, 14: 100, 15: 8192, 19: 178, 21: 32, 22: 32, 23: 200, 24: 9372,
        25: 9372, 39: 100, 42: 8370, 43: 8370, 45: 20, 47: 500, 48: 1002, 90: 25,
        91: 103, 92: 128, 93: 153, 99: -97531, 15773: -97532,
    }
    with dss.open(p) as f:
        assert f.version == 7 and f.catalog().empty
    with pytest.raises(FileExistsError):
        dss.create(p)
    # P22: version=6 creates a DSS6 file (tests/python/test_writer6.py).
    assert dss.create(tmp_path / "x6.dss", version=6).stat().st_size == 26_112
    with pytest.raises(ValueError):
        dss.create(tmp_path / "x5.dss", version=5)


@pytest.mark.parametrize(
    "epart,start,n,blocks",
    [
        ("1Minute", "01MAR2021 2301", 1500, 2),  # daily blocks
        ("15Minute", "30JAN2021 0015", 400, 2),  # monthly blocks
        ("1Hour", "30DEC2020 0100", 120, 2),  # monthly, across a year
        ("1Day", "15DEC2019 2400", 800, 4),  # yearly blocks
        ("1Month", "31JAN2018 2400", 40, 2),  # decade blocks
        ("15Second", "01JUN2025 2358", 30, 2),  # second granularity, daily blocks
        ("1Day", "01JAN2199 2400", 30, 1),  # far future
        ("6Hour", "28DEC8999 0600", 30, 2),
    ],
)
@pytest.mark.parametrize("dtype", [np.float32, np.float64])
def test_round_trip_blocks(new_file, epart, start, n, blocks, dtype):
    path = f"/P20/LOC/FLOW//{epart}/SYN/"
    vals = (100 + 50 * np.sin(np.arange(n) / 7.0)).astype(dtype)
    r = dss.write_timeseries(new_file, path, vals, start=start, units="cfs", data_type="INST-VAL")
    assert [b["outcome"] for b in r["blocks"]] == ["added"] * blocks
    assert {b["record_type"] for b in r["blocks"]} == {100 if dtype == np.float32 else 105}
    with dss.open(new_file) as f:
        d = f.read_arrays(path)
    assert d["values"].dtype == dtype
    np.testing.assert_array_equal(d["values"], vals)
    expected = dss._native.rts_value_times(path, hec(start), n)
    np.testing.assert_array_equal(d["times"], expected)
    assert d["times"][0] == hec(start)
    assert d["units"] == "cfs" and d["data_type"] == "INST-VAL"


def test_block_pathnames_and_canonical_epart(new_file):
    r = dss.write_timeseries(
        new_file, "/A/B/FLOW/01JAN1999/1HOUR/F/", np.ones(30, np.float32),
        start="31JAN2021 2200", units="cfs", data_type="INST-VAL",
    )
    assert [b["pathname"] for b in r["blocks"]] == [
        "/A/B/FLOW/01Jan2021/1Hour/F/", "/A/B/FLOW/01Feb2021/1Hour/F/",
    ]
    assert [(b["number_data"], b["logical_number"]) for b in r["blocks"]] == [(3, 744), (27, 672)]


def test_offset_and_times_argument(new_file):
    path = "/A/B/FLOW//1Hour/OFF/"
    times = pd.date_range("2021-02-01 00:30", periods=30, freq="h")
    vals = np.arange(30, dtype=np.float64)
    dss.write_timeseries(new_file, path, vals, times=times, units="cfs", data_type="INST-VAL")
    with dss.open(new_file) as f:
        d = f.read_arrays(path)
    np.testing.assert_array_equal(stamps(d), times.values.astype("datetime64[s]"))
    assert d["time_offset_seconds"] == 1800
    # A Series with a DatetimeIndex supplies the times.
    s = pd.Series(np.arange(5.0), index=pd.date_range("2022-01-01", periods=5, freq="D"))
    dss.write_timeseries(new_file, "/A/B/FLOW//1Day/SER/", s, units="cfs", data_type="PER-AVER")
    with dss.open(new_file) as f:
        d = f.read_arrays("/A/B/FLOW//1Day/SER/")
    np.testing.assert_array_equal(stamps(d), s.index.values.astype("datetime64[s]"))


def test_irregular_times_refused(new_file):
    t = list(pd.date_range("2021-01-01 01:00", periods=5, freq="h"))
    t[3] = t[3] + pd.Timedelta(minutes=1)
    with pytest.raises(ValueError, match="not regular"):
        dss.write_timeseries(new_file, "/A/B/C//1Hour/F/", np.ones(5), times=t,
                             units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError, match="exactly one"):
        dss.write_timeseries(new_file, "/A/B/C//1Hour/F/", np.ones(5), units="cfs",
                             data_type="INST-VAL")


def test_quality_and_missing(new_file):
    path = "/A/B/FLOW//1Hour/Q/"
    vals = np.array([1.0, -901.0, np.nan, 4.0, MISSING], np.float32)
    q = np.array([3, 5, 0, 3, 7], np.int32)
    dss.write_timeseries(new_file, path, vals, start="01FEB2021 0100", units="cfs",
                         data_type="INST-VAL", quality=q)
    with dss.open(new_file) as f:
        d = f.read_record_arrays("/A/B/FLOW/01Feb2021/1Hour/Q/")
    # Quality stores the whole block (672 positions); -901 kept, NaN -> -FLT_MAX.
    assert len(d["values"]) == 672
    np.testing.assert_array_equal(d["values"][:5], [1.0, -901.0, MISSING, 4.0, MISSING])
    np.testing.assert_array_equal(d["quality"][:5], q)
    assert (d["quality"][5:] == 0).all() and (d["values"][5:] == MISSING).all()
    # raw=True keeps NaN as a value.
    dss.write_timeseries(new_file, "/A/B/FLOW//1Hour/RAW/", np.array([1.0, np.nan, 3.0]),
                         start="01FEB2021 0100", units="cfs", data_type="INST-VAL", raw=True)
    with dss.open(new_file) as f:
        d = f.read_arrays("/A/B/FLOW//1Hour/RAW/")
    assert np.isnan(d["values"][1]) and len(d["values"]) == 3


def test_trim_and_all_missing_block(new_file):
    path = "/A/B/FLOW//1Hour/T/"
    vals = np.array([MISSING, -902.0, 1.0, 2.0, -901.0], np.float32)
    r = dss.write_timeseries(new_file, path, vals, start="01FEB2021 0100", units="cfs",
                             data_type="INST-VAL")
    assert r["blocks"][0]["number_data"] == 2
    # HEC's default (copyEmptyRecords = 1) stores an all-missing block as one value.
    r = dss.write_timeseries(new_file, "/A/B/FLOW//1Hour/E/", np.full(4, MISSING),
                             start="01FEB2021 0100", units="cfs", data_type="INST-VAL")
    assert r["blocks"][0]["outcome"] == "added" and r["blocks"][0]["number_data"] == 1


def test_replace_merge_and_idempotent(new_file):
    path = "/A/B/FLOW//1Hour/M/"
    base = np.arange(100, dtype=np.float32)
    dss.write_timeseries(new_file, path, base, start="01MAR2021 0100", units="cfs",
                         data_type="INST-VAL")
    size0 = os.path.getsize(new_file)
    r = dss.write_timeseries(new_file, path, base, start="01MAR2021 0100", units="cfs",
                             data_type="INST-VAL")
    assert [b["outcome"] for b in r["blocks"]] == ["unchanged"]
    assert os.path.getsize(new_file) == size0
    r = dss.write_timeseries(new_file, path, np.array([9.0, 8.0, 7.0], np.float32),
                             start="02MAR2021 0500", units="cfs", data_type="INST-VAL")
    assert [b["outcome"] for b in r["blocks"]] == ["replaced"]
    expect = base.copy()
    expect[28:31] = [9.0, 8.0, 7.0]
    with dss.open(new_file) as f:
        d = f.read_arrays(path)
        assert f.signature()["dead_words"] > 0
    np.testing.assert_array_equal(d["values"], expect)
    # Extending past the stored range grows the record.
    dss.write_timeseries(new_file, path, np.full(10, 5.0, np.float32), start="05MAR2021 0500",
                         units="cfs", data_type="INST-VAL")
    with dss.open(new_file) as f:
        d = f.read_arrays(path)
    assert len(d["values"]) == 110
    np.testing.assert_array_equal(d["values"][-10:], np.full(10, 5.0))


def test_quality_dropped_without_quality(new_file):
    path = "/A/B/FLOW//1Hour/QD/"
    dss.write_timeseries(new_file, path, np.arange(30, dtype=np.float32), start="01MAR2021 0100",
                         units="cfs", data_type="INST-VAL", quality=np.full(30, 7))
    dss.write_timeseries(new_file, path, np.array([1.5, 2.5], np.float32), start="01MAR2021 0300",
                         units="cfs", data_type="INST-VAL")
    with dss.open(new_file) as f:
        d = f.read_arrays(path)
    assert d["quality"] is None  # HEC drops stored quality when the write has none
    assert d["values"][2] == 1.5 and d["values"][29] == 29.0


def test_float_and_double_mixing(new_file):
    p1 = "/A/B/FLOW//1Hour/FD/"
    third = np.array([1 / 3 + i for i in range(30)])
    dss.write_timeseries(new_file, p1, third, start="01MAR2021 0100", units="cfs", data_type="INST-VAL")
    r = dss.write_timeseries(new_file, p1, np.array([2.5, 3.5], np.float32), start="01MAR2021 0500",
                             units="cfs", data_type="INST-VAL")
    assert r["blocks"][0]["record_type"] == 105
    with dss.open(new_file) as f:
        d = f.read_arrays(p1)
    expect = third.copy()
    expect[4:6] = [2.5, 3.5]
    np.testing.assert_array_equal(d["values"], expect)  # stored doubles keep their bits
    p2 = "/A/B/FLOW//1Hour/DF/"
    dss.write_timeseries(new_file, p2, np.arange(30, dtype=np.float32), start="01MAR2021 0100",
                         units="cfs", data_type="INST-VAL")
    r = dss.write_timeseries(new_file, p2, np.array([1 / 3]), start="01MAR2021 0500", units="cfs",
                             data_type="INST-VAL")
    assert r["blocks"][0]["record_type"] == 105
    with dss.open(new_file) as f:
        cat = f.catalog()
        d = f.read_arrays(p2)
    assert d["values"].dtype == np.float64 and d["values"][4] == 1 / 3
    assert set(cat["record_type"]) == {105}


def test_many_pathnames_new_bin_blocks(new_file):
    for i in range(70):
        dss.write_timeseries(new_file, f"/P20/MANY{i:03d}/FLOW//1Day/SYN/", np.arange(5.0) + i,
                             start="01JAN2021 2400", units="cfs", data_type="INST-VAL")
    with dss.open(new_file) as f:
        cat = f.catalog()
        assert len(cat) == 70
        for i in (0, 33, 69):
            d = f.read_arrays(f"/P20/MANY{i:03d}/FLOW//1Day/SYN/")
            np.testing.assert_array_equal(d["values"], np.arange(5.0) + i)


def test_dssfile_write_refreshes_reader(new_file):
    with dss.open(new_file) as f:
        assert f.catalog().empty
        f.write_timeseries("/A/B/C//1Day/F/", np.arange(3.0), start="01JAN2021 2400",
                           units="ft", data_type="INST-VAL")
        df = f.read_timeseries("/A/B/C//1Day/F/")
        assert list(df["value"]) == [0.0, 1.0, 2.0]


@pytest.mark.parametrize(
    "path",
    [
        "/A/B/C//1Hour/F",
        "/A/B/C/D//1Hour/F/",
        "/A/B/C//1HOU/F/",
        "/A/B/C//IR-DAY/F/",
        "/A/B/C//~1Day/F/",
        "/A/B/C/TS-PATTERN/1Hour/F/",
        "/A/B/C/NOTADATE/1Hour/F/",
        "/A/B/C//1Hour/C:000001|X/",
        "/A/Bé/C//1Hour/F/",
        "/" + "A" * 129 + "/B/C//1Hour/F/",
    ],
)
def test_pathname_refused(new_file, path):
    with pytest.raises(ValueError):
        dss.write_timeseries(new_file, path, np.ones(3), start="01JAN2021 0100", units="cfs",
                             data_type="INST-VAL")
    with dss.open(new_file) as f:
        assert f.catalog().empty


def test_input_refused(new_file):
    p = "/A/B/C//1Hour/F/"
    kw = dict(start="01JAN2021 0100", units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError):
        dss.write_timeseries(new_file, p, np.ones(3), quality=[1, 2], **kw)
    with pytest.raises(ValueError):
        dss.write_timeseries(new_file, p, np.array([]), **kw)
    with pytest.raises(ValueError):
        dss.write_timeseries(new_file, p, np.ones(3), start="01JAN2021 0100", units="cé",
                             data_type="INST-VAL")
    with pytest.raises(ValueError):
        dss.write_timeseries(new_file, p, np.ones(3), start="01JAN2021 0100", units="u" * 190,
                             data_type="INST-VAL")
    # P22: the DSS6 writer refuses a DSS7 file.
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_timeseries(new_file, p, np.ones(3), version=6, **kw)


def test_dss6_and_old_dss7_refused(tmp_path):
    p6 = tmp_path / "s6.dss"
    shutil.copyfile(fixture_path("sample6.dss"), p6)
    with pytest.raises(dss.DssError):
        dss.write_timeseries(p6, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                             units="cfs", data_type="INST-VAL")
    # sample7.dss is 7-10: old lock scheme (readers undetectable). The default policy
    # (maintainer 2026-10-28) passes the lock guard; the 7-10 aggregate layout is then
    # refused. A strict caller refuses at the guard.
    p7 = tmp_path / "s7.dss"
    shutil.copyfile(fixture_path("sample7.dss"), p7)
    before = p7.read_bytes()
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_timeseries(p7, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                             units="cfs", data_type="INST-VAL")
    with pytest.raises(DssLockedError) as e:
        dss.write_timeseries(p7, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                             units="cfs", data_type="INST-VAL", allow_undetectable_readers=False)
    assert e.value.readers_undetectable
    assert p7.read_bytes() == before


HOLD = r"""
import os, sys
path, word = sys.argv[1], int(sys.argv[2])
if os.name == "nt":
    import msvcrt
    fd = os.open(path, os.O_RDWR | os.O_BINARY)
    os.lseek(fd, word * 8, 0)
    msvcrt.locking(fd, msvcrt.LK_NBLCK, 8)
else:
    import fcntl
    fd = os.open(path, os.O_RDWR)
    fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 8, word * 8, 0)
print("READY", flush=True)
sys.stdin.read()
"""


@pytest.mark.parametrize("word,reason", [(128, "read_array"), (100, "primary_write"), (103, "write_array")])
def test_refused_while_hec_holds_a_lock_word(new_file, word, reason):
    before = new_file.read_bytes()
    child = subprocess.Popen([sys.executable, "-I", "-c", HOLD, str(new_file), str(word)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == "READY"
        with pytest.raises(DssLockedError) as e:
            dss.write_timeseries(new_file, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                                 units="cfs", data_type="INST-VAL")
        assert reason in e.value.reasons
        assert e.value.path == str(new_file)
    finally:
        child.stdin.close()
        child.wait(timeout=30)
    assert new_file.read_bytes() == before
    dss.write_timeseries(new_file, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                         units="cfs", data_type="INST-VAL")


def test_other_records_byte_identical(tmp_path):
    """G4 on a public RasExamples file copy: outside the appended space only the header,
    one table slot, the new bin entry (7 + 5 words) and the old EOF word may change, and
    every existing record reads back identically (word-level G4: tests/dss7_write.rs)."""
    p = tmp_path / "be.dss"
    shutil.copyfile(fixture_path("BaldEagleDamBrk.dss"), p)
    before = p.read_bytes()
    with dss.open(p) as f:
        paths = list(f.catalog()["pathname"])
        recs0 = [f.read_record_arrays(x) for x in paths]
    dss.write_timeseries(p, "/P20/NEW/FLOW//1Hour/SYN/", np.arange(48.0), start="01JAN2021 0100",
                         units="cfs", data_type="INST-VAL")
    after = p.read_bytes()
    assert len(after) > len(before)
    words = len(before) // 8
    changed = [i for i in range(words) if before[8 * i:8 * i + 8] != after[8 * i:8 * i + 8]]
    outside_header = [i for i in changed if i >= 100]
    assert len(outside_header) <= 1 + 12 + 1, outside_header
    with dss.open(p) as f:
        assert len(f.catalog()) == len(paths) + 1
        for x, r0 in zip(paths, recs0):
            r1 = f.read_record_arrays(x)
            np.testing.assert_array_equal(r0["values"], r1["values"])
            np.testing.assert_array_equal(r0["times"], r1["times"])


# -- HEC-written public fixture (heclib C 7-JA via evidence/P20/scripts/p20hec.c) ---------

FIXTURE = "p20_hec_rts7.dss"
FIXTURE_SHA256 = "a3c89f155e9b938b14f22913a491bccafda0724abcc52f228e5df8bdecb68c5f"
SPEC = "p20_hec_rts7_spec.txt"
SPEC_SHA256 = "b3ff966e66a781c6535e8b35a69b6d0690971859633aa309965360afa3ba7e0d"


def parse_spec(text):
    """STORE lines of the p20hec spec: (kind, path, date, time, units, type, values, quality)."""
    t = text.split()
    i = 0
    out = []
    while i < len(t):
        assert t[i] == "STORE"
        kind, path, date, time, units, typ, n, _flag, hq = t[i + 1:i + 10]
        n = int(n)
        i += 10
        hexes = t[i:i + n]
        i += n
        if kind == "float":
            vals = np.array([struct.unpack("<f", bytes.fromhex(h)[::-1])[0] for h in hexes], np.float32)
        else:
            vals = np.array([struct.unpack("<d", bytes.fromhex(h)[::-1])[0] for h in hexes], np.float64)
        q = None
        if hq == "1":
            q = np.array([int(x) for x in t[i:i + n]], np.int32)
            i += n
        out.append((kind, path, date, time, units.replace("~", ""), typ.replace("~", ""), vals, q))
    return out


def masked_words(path):
    """File words with write times, creation times and program names set to 0: header
    12/13, bin +5, info 6/7/8/11 (dss7.md §2.2, §3.4, §5.1)."""
    b = path.read_bytes()
    w = list(struct.unpack("<%dq" % (len(b) // 8), b))
    w[12] = w[13] = 0
    ht, mh, bs = w[19], w[15], w[23]
    for t in range(mh):
        a = w[ht + t]
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


def hec_time(date, time):
    if len(time) > 4:
        return hec(f"{date} {time[:2]}:{time[2:4]}:{time[4:]}")
    return hec(f"{date} {time}")


def test_hec_fixture_hash_and_native_read():
    import hashlib

    fx, sp = fixture_path(FIXTURE), fixture_path(SPEC)
    assert hashlib.sha256(fx.read_bytes()).hexdigest() == FIXTURE_SHA256
    # The spec is text: a Windows checkout may convert it to CRLF.
    spec_bytes = sp.read_bytes().replace(bytes([13, 10]), bytes([10]))
    assert hashlib.sha256(spec_bytes).hexdigest() == SPEC_SHA256
    spec = parse_spec(sp.read_text())
    assert len(spec) == 20
    with dss.open(fx) as f:
        assert f.version_string == "7-JA" and len(f.catalog()) == 31
        for kind, path, date, time, units, typ, vals, q in spec:
            d = f.read_arrays(path, trim_missing=False)
            n = len(vals)
            start = dss._native.rts_value_times(path, hec_time(date, time), n)
            i0 = int(np.searchsorted(d["times"], start[0]))
            np.testing.assert_array_equal(d["times"][i0:i0 + n], start)
            assert d["values"].dtype == (np.float32 if kind == "float" else np.float64)
            expect = vals.copy()
            if q is None:
                # HEC stores first..last valid only: the leading and trailing HEC missing
                # values read back as -FLT_MAX (ztsRegStoreBlock.c lines 816-826).
                ok = ~np.isin(vals, [-901.0, -902.0]) & (vals != vals.dtype.type(MISSING))
                first, last = np.argmax(ok), n - 1 - np.argmax(ok[::-1])
                expect[:first] = MISSING
                expect[last + 1:] = MISSING
            np.testing.assert_array_equal(d["values"][i0:i0 + n].view(np.uint8), expect.view(np.uint8))
            if q is not None:
                np.testing.assert_array_equal(d["quality"][i0:i0 + n], q)
            assert d["units"] == units and d["data_type"] == typ


def test_native_write_equals_hec_write(tmp_path):
    """The same spec written natively into a new file is word-identical to the HEC-written
    fixture, except write/creation times and the program name."""
    out = tmp_path / "native.dss"
    dss.create(out)
    for kind, path, date, time, units, typ, vals, q in parse_spec(fixture_path(SPEC).read_text()):
        start = EPOCH + np.timedelta64(hec_time(date, time), "s")
        dss.write_timeseries(out, path, vals, start=start, units=units, data_type=typ,
                             quality=q, raw=True)
    a, b = masked_words(fixture_path(FIXTURE)), masked_words(out)
    assert len(a) == len(b)
    diff = [i for i, (x, y) in enumerate(zip(a, b)) if x != y]
    assert diff == []
    raw = out.read_bytes()
    with dss.open(out) as f:
        first = f.catalog()["pathname"].iloc[0]
        assert f.read_record_arrays(first) is not None
    assert b"fast-dss" in raw and b"not specified" in fixture_path(FIXTURE).read_bytes()


def test_old_lock_scheme_written_with_warning(tmp_path):
    """A 7-HA+ file with the old lock scheme (read-array pointer 0, synthetic): the
    default policy writes it while holding word 100 and warns; a strict policy refuses."""
    p = tmp_path / "old.dss"
    dss.create(p)
    b = bytearray(p.read_bytes())
    b[92 * 8:93 * 8] = (0).to_bytes(8, "little")
    p.write_bytes(bytes(b))
    assert dss.lock_status(p)["readers_detectable"] is False
    with pytest.raises(DssLockedError) as e:
        dss.write_timeseries(p, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                             units="cfs", data_type="INST-VAL", allow_undetectable_readers=False)
    assert e.value.readers_undetectable and e.value.reasons == ()
    assert p.read_bytes() == bytes(b)
    with pytest.warns(RuntimeWarning, match="old lock scheme"):
        r = dss.write_timeseries(p, "/A/B/C//1Hour/F/", np.arange(3.0), start="01JAN2021 0100",
                                 units="cfs", data_type="INST-VAL")
    assert r["readers_undetected"] and r["blocks"][0]["outcome"] == "added"
    with dss.open(p) as f:
        np.testing.assert_array_equal(f.read_arrays("/A/B/C//1Hour/F/")["values"], np.arange(3.0))


def test_locked_message_names_the_remedy(new_file):
    child = subprocess.Popen([sys.executable, "-I", "-c", HOLD, str(new_file), "128"],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == "READY"
        with pytest.raises(DssLockedError, match="close the HEC program"):
            dss.write_timeseries(new_file, "/A/B/C//1Hour/F/", np.ones(3), start="01JAN2021 0100",
                                 units="cfs", data_type="INST-VAL")
    finally:
        child.stdin.close()
        child.wait(timeout=30)


def test_silent_conversions_refused(new_file):
    p = "/A/B/C//1Hour/CONV/"
    kw = dict(start="01JAN2021 0100", units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError, match="overflows"):
        dss.write_timeseries(new_file, p, np.array([1.0, 1e39]), value_type="float", **kw)
    with pytest.raises(ValueError, match="infinite"):
        dss.write_timeseries(new_file, p, np.array([1.0, np.inf]), **kw)
    with pytest.raises(ValueError, match="exactly"):
        dss.write_timeseries(new_file, p, np.array([1, 2**53 + 1], dtype=np.int64), **kw)
    with pytest.raises(ValueError, match="exactly"):
        dss.write_timeseries(new_file, p, np.array([16777217], dtype=np.int64), value_type="float", **kw)
    with pytest.raises(ValueError, match="integers"):
        dss.write_timeseries(new_file, p, np.ones(2), quality=[1.0, 1.5], **kw)
    with pytest.raises(ValueError, match="integers"):
        dss.write_timeseries(new_file, p, np.ones(2), quality=[1.0, np.nan], **kw)
    with pytest.raises(ValueError, match="32 bits"):
        dss.write_timeseries(new_file, p, np.ones(2), quality=np.array([1, 2**64 - 1], np.uint64), **kw)
    with pytest.raises(ValueError, match="exactly"):
        dss.write_timeseries(new_file, p, np.array([2**63 - 1], dtype=np.int64), **kw)
    tz = pd.date_range("2021-01-01 01:00", periods=3, freq="h", tz="UTC")
    with pytest.raises(ValueError, match="time-zone aware"):
        dss.write_timeseries(new_file, p, np.ones(3), times=tz, units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError, match="time-zone aware"):
        dss.write_timeseries(new_file, p, np.ones(3), start=pd.Timestamp("2021-01-01 01:00", tz="UTC"),
                             units="cfs", data_type="INST-VAL")
    with pytest.raises(ValueError, match="time-zone aware"):
        dss.write_timeseries(new_file, p, np.ones(3), start="2021-01-01T01:00+05:00",
                             units="cfs", data_type="INST-VAL")
    # Integral float quality and exact integers are accepted; raw keeps inf.
    r = dss.write_timeseries(new_file, p, np.array([1, 2, 3]), quality=[1.0, 2.0, 3.0], **kw)
    assert r["blocks"][0]["outcome"] == "added"
    assert r["lock_method"] in ("whole_file_lockfileex", "fcntl_setlk")
    assert r["reconciled_interrupted_write"] is False
    dss.write_timeseries(new_file, "/A/B/C//1Hour/INF/", np.array([np.inf]), raw=True, **kw)
    with dss.open(new_file) as f:
        assert np.isinf(f.read_arrays("/A/B/C//1Hour/INF/")["values"][0])


@pytest.mark.xfail(
    strict=True,
    raises=AssertionError,
    reason="deliberate deviation (P20 REPORT §3.4): heclib 7-JA reads a double record as "
    "floats before merging float data into it, so untouched values lose precision; the "
    "native writer keeps the stored doubles (oracle case float_into_double)",
)
def test_float_into_double_matches_hec(new_file):
    p = "/A/B/C//1Hour/FD/"
    third = np.array([1 / 3 + i for i in range(30)])
    dss.write_timeseries(new_file, p, third, start="01MAR2021 0100", units="cfs", data_type="INST-VAL")
    dss.write_timeseries(new_file, p, np.array([2.5, 3.5], np.float32), start="01MAR2021 0500",
                         units="cfs", data_type="INST-VAL")
    with dss.open(new_file) as f:
        got = f.read_arrays(p)["values"]
    # What the native writer stores: the untouched doubles keep every bit.
    assert got.dtype == np.float64 and got[0] == third[0] and got[29] == third[29]
    hec = third.astype(np.float32).astype(np.float64)  # what heclib 7-JA stores
    hec[4:6] = [2.5, 3.5]
    np.testing.assert_array_equal(got, hec)

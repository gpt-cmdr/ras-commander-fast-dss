"""P19a: ras_commander_fast_dss.lock_status and DssLockedError attributes (public fixtures only).

A child Python process takes HEC-style byte-range locks on a temporary copy of a public
fixture, exactly as heclib does: ``msvcrt.locking`` (the CRT ``_locking`` call of
``heclib_c/src/Dss6/lockdss.c`` lines 28-42) on Windows, ``fcntl.lockf`` on POSIX (lines
83-96). The parent checks the exact reason and byte offset, release, reads while every
word is held, and a clean state after the child exits.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import queue
import threading
import time

import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssLockedError

from conftest import FIXTURES, fixture_path

TIMEOUT = 30

HOLDER = r"""
import os, sys
path, deny = sys.argv[1], sys.argv[2] == "1"
if os.name == "nt":
    import msvcrt
    if deny:
        # _SH_DENYRW open (heclib_c/src/System/zopendisk.c line 85) via CreateFileW.
        import ctypes
        from ctypes import wintypes
        k = ctypes.WinDLL("kernel32", use_last_error=True)
        k.CreateFileW.restype = wintypes.HANDLE
        k.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD,
                                  wintypes.HANDLE]
        h = k.CreateFileW(path, 0xC0000000, 0, None, 3, 0x80, None)
        if h in (None, wintypes.HANDLE(-1).value):
            raise OSError(ctypes.get_last_error(), "CreateFileW")
    else:
        fd = os.open(path, os.O_RDWR | os.O_BINARY)
    def setlock(off, n, on):
        os.lseek(fd, off, 0)
        msvcrt.locking(fd, msvcrt.LK_NBLCK if on else msvcrt.LK_UNLCK, n)
else:
    import fcntl
    fd = os.open(path, os.O_RDWR)
    def setlock(off, n, on):
        fcntl.lockf(fd, (fcntl.LOCK_EX | fcntl.LOCK_NB) if on else fcntl.LOCK_UN, n, off, 0)
print("READY", flush=True)
for line in sys.stdin:
    op, off, n = line.split()
    try:
        setlock(int(off), int(n), op == "L")
        print("OK", flush=True)
    except OSError as e:
        print("ERR", e, flush=True)
"""


class Holder:
    def __init__(self, path, deny=False):
        self.p = subprocess.Popen(
            [sys.executable, "-I", "-c", HOLDER, str(path), "1" if deny else "0"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        self.q: queue.Queue = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        assert self._reply() == "READY"

    def _pump(self):
        for line in self.p.stdout:
            self.q.put(line.strip())

    def _reply(self):
        try:
            return self.q.get(timeout=TIMEOUT)
        except queue.Empty:
            self.kill()
            pytest.fail("lock holder did not answer in time")

    def _cmd(self, op, off, n):
        self.p.stdin.write(f"{op} {off} {n}\n")
        self.p.stdin.flush()
        r = self._reply()
        assert r == "OK", r

    def lock(self, off, n):
        self._cmd("L", off, n)

    def unlock(self, off, n):
        self._cmd("U", off, n)

    def exit(self):
        """Close stdin; the child exits with its locks still held (the OS drops them)."""
        self.p.stdin.close()
        try:
            assert self.p.wait(timeout=TIMEOUT) == 0
        finally:
            self.kill()

    def kill(self):
        if self.p.poll() is None:
            self.p.kill()
            self.p.wait(timeout=TIMEOUT)


@pytest.fixture
def holder_factory():
    made = []

    def make(path, deny=False):
        h = Holder(path, deny)
        made.append(h)
        return h

    yield make
    for h in made:
        h.kill()


def copy(tmp_path, name):
    dst = tmp_path / name
    shutil.copyfile(fixture_path(name), dst)
    return dst


def ranges(path):
    """Expected (reason, byte, length) per word: DSS7 header words 14/90/91/92 or DSS6
    KLOCKB (dss7.md §6.1, dss6.md §9.1), computed independently of the Rust code."""
    import struct

    b = open(path, "rb").read(800)
    if b[16:17] == b"7":
        w = struct.unpack("<100q", b)
        if w[92] == 0:  # old scheme, zlockingOld.c line 127
            return [("primary_write", 800, 8), ("request", 808, 8), ("exclusive", 816, 8)]
        size = w[90]
        out = [("primary_write", w[14] * 8, 8), ("exclusive", (w[14] + 1) * 8, 8)]
        out += [("write_array", (w[91] + i) * 8, 8) for i in range(size)]
        out += [("read_array", (w[92] + i) * 8, 8) for i in range(size)]
        return out
    # KLOCKB from KHASH (word 14) and KTABLE (word 15), zopen6Int.f lines 280-287.
    w = struct.unpack("<69i", b[:276])
    size = w[14] + 70 if w[15] == 1 else 70
    rec = (size - 1) // 127 + 1 + (1 if (size - 1) % 127 else 0)
    kb = (rec - 1) * 512
    out = [("primary_write", kb, 4), ("request", kb + 4, 4), ("exclusive", kb + 8, 4)]
    out += [("read_array", kb + 16 + 4 * k, 4) for k in range(100)]
    return out


def assert_clean_soon(path, timeout=10.0):
    """After a holder exits the OS may take a moment to drop its locks."""
    end = time.monotonic() + timeout
    while True:
        st = dss.lock_status(path)
        if not st["locked"]:
            return
        assert time.monotonic() < end, st
        time.sleep(0.05)


def held(st):
    return [(h["reason"], h["byte"], h["length"]) for h in st["held"]]


def read_all(path):
    """Catalog plus every time series; must succeed while HEC words are held."""
    n = 0
    with dss.open(path) as f:
        cat = f.catalog()
        for p, rt in zip(cat["pathname"], cat["record_type"]):
            if 100 <= int(rt) < 200:
                f.read_timeseries(p)
                n += 1
    return n


# ---------------------------------------------------------------------------------


@pytest.mark.parametrize("name", sorted(p.name for p in FIXTURES.glob("*.dss")))
def test_public_fixture_not_locked(name):
    st = dss.lock_status(fixture_path(name))
    assert st["locked"] is False
    assert st["reasons"] == [] and st["held"] == [] and st["holder_pids"] == []
    assert st["version"] in (6, 7)
    assert st["readers_detectable"] is (name != "sample7.dss")
    assert st["ranges_tested"] == (3 if name == "sample7.dss" else 52 if st["version"] == 7 else 103)


@pytest.mark.parametrize(
    "name", ["synthetic_python7.dss", "sample7.dss", "castro.dss", "synthetic_python6.dss"]
)
def test_every_word_detected_and_released(tmp_path, holder_factory, name):
    path = copy(tmp_path, name)
    before = path.read_bytes()
    exp = ranges(path)
    h = holder_factory(path)
    assert dss.lock_status(path)["locked"] is False
    for reason, off, n in exp:
        h.lock(off, n)
        st = dss.lock_status(path)
        assert held(st) == [(reason, off, n)], (reason, off)
        assert st["reasons"] == [reason]
        if os.name != "nt" and sys.platform.startswith("linux"):
            assert st["holder_pids"] == [h.p.pid]
        h.unlock(off, n)
        assert dss.lock_status(path)["locked"] is False
    # Hold every word at once: reads still work and never raise DssLockedError.
    for _, off, n in exp:
        h.lock(off, n)
    st = dss.lock_status(path)
    assert len(st["held"]) == len(exp)
    assert read_all(path) > 0
    h.exit()
    assert_clean_soon(path)
    assert path.read_bytes() == before


@pytest.mark.parametrize("name,klockb", [("synthetic_python6.dss", 512), ("castro.dss", 1024)])
def test_dss6_read_slot_plus_write_word(tmp_path, holder_factory, name, klockb):
    """Review M1: with KLOCKB = 512 the lock record lies inside the first 800 bytes; a
    held read slot must be reported (not an OSError), together with the write word."""
    path = copy(tmp_path, name)
    assert ranges(path)[0] == ("primary_write", klockb, 4)
    h = holder_factory(path)
    h.lock(klockb + 16, 4)
    st = dss.lock_status(path)
    assert held(st) == [("read_array", klockb + 16, 4)]
    h.lock(klockb, 4)
    st = dss.lock_status(path)
    assert held(st) == [("primary_write", klockb, 4), ("read_array", klockb + 16, 4)]
    assert st["reasons"] == ["primary_write", "read_array"]
    assert read_all(path) > 0
    h.exit()
    assert_clean_soon(path)


def test_truncated_dss6_is_format_error(tmp_path):
    """Review m2: lock record past EOF is an error, never 'unlocked'."""
    path = copy(tmp_path, "castro.dss")
    path.write_bytes(path.read_bytes()[:1100])
    with pytest.raises(dss.DssFormatError):
        dss.lock_status(path)


def test_stale_write_marker(tmp_path):
    path = copy(tmp_path, "castro.dss")
    b = bytearray(path.read_bytes())
    b[20:24] = (-1357).to_bytes(4, "little", signed=True)  # KFSIZE, zwlock6.f line 69
    path.write_bytes(bytes(b))
    st = dss.lock_status(path)
    assert st["reasons"] == ["stale_write_marker"]
    assert held(st) == [("stale_write_marker", 20, 4)]


@pytest.mark.skipif(os.name != "nt", reason="share modes are Windows-only")
def test_windows_sharing_violation(tmp_path, holder_factory):
    path = copy(tmp_path, "synthetic_python7.dss")
    h = holder_factory(path, deny=True)
    st = dss.lock_status(path)
    assert st["reasons"] == ["sharing_violation"] and st["version"] == 0
    h.exit()
    assert_clean_soon(path)


def test_errors(tmp_path):
    with pytest.raises(FileNotFoundError):
        dss.lock_status(tmp_path / "missing.dss")
    bad = tmp_path / "bad.dss"
    bad.write_bytes(b"x" * 1000)
    with pytest.raises(dss.DssFormatError):
        dss.lock_status(bad)


def test_locked_error_attributes():
    e = DssLockedError("plain")
    assert str(e) == "plain" and e.reasons == () and e.holder_pids == () and e.path is None
    assert isinstance(e, dss.DssError) and isinstance(e, RuntimeError)
    st = {"reasons": ["primary_write", "read_array"], "holder_pids": [42]}
    e = DssLockedError.from_lock_status(st, "C:/x.dss")
    assert str(e) == "DSS file is in use by HEC software (primary_write, read_array): C:/x.dss"
    assert e.reasons == ("primary_write", "read_array") and e.holder_pids == (42,)
    assert e.path == "C:/x.dss"
    with pytest.raises(DssLockedError) as ei:
        raise DssLockedError("w", reasons=["exclusive"])
    assert ei.value.reasons == ("exclusive",)

"""P74: one held read+write native handle per ``DssFile`` session.

The first write through a ``DssFile`` opens one read+write handle; later writes and every
read use it. Before each write and before the read view is rebuilt after a write, the
handle's identity is checked against the file the path names now. These tests cover:

- the session opens the file once, and HEC lock state between writes is unchanged;
- a write by another process between two session writes is seen and kept;
- the file replaced, renamed away or deleted under the held handle;
- the handle closed on ``close()``, ``__exit__`` and garbage collection (Windows: an
  exclusive open succeeds only when no handle is left);
- a lock held by an HEC-style holder still refuses a session write;
- the held path writes the same bytes as the module functions.

Checks with HEC software (hecdss and HEC Monolith writing between session writes) and on a
network share need those programs or a share, so they are not part of this suite.

Synthetic data in ``tmp_path`` only.
"""

from __future__ import annotations

import gc
import os
import shutil
import subprocess
import sys
import textwrap

import numpy as np
import pytest

import ras_commander_fast_dss as dss

PD_KW = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")
TS_KW = dict(start="01JAN2021 01:00", units="CFS", data_type="INST-VAL")


def new(tmp_path, name, v):
    p = tmp_path / f"{name}{v}.dss"
    dss.create(p, version=v)
    return p


def pd_values(f, pathname):
    return f.read_paired_data(pathname).values.ravel().tolist()


def exclusive_open_ok(path) -> bool:
    """Windows: whether a no-share open succeeds, that is, no handle on the file is left."""
    import ctypes
    from ctypes import wintypes

    k = ctypes.WinDLL("kernel32", use_last_error=True)
    k.CreateFileW.restype = wintypes.HANDLE
    k.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                              wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    h = k.CreateFileW(str(path), 0x80000000, 0, None, 3, 0x80, None)
    if h in (None, wintypes.HANDLE(-1).value):
        assert ctypes.get_last_error() == 32, ctypes.get_last_error()  # sharing violation
        return False
    k.CloseHandle(h)
    return True


def posix_replace(src, dst):
    """Windows: rename ``src`` over ``dst`` with POSIX semantics (``FileRenameInfoEx``,
    ``FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS``), which
    replaces a target that other handles hold open with ``FILE_SHARE_DELETE``."""
    import ctypes
    from ctypes import wintypes

    k = ctypes.WinDLL("kernel32", use_last_error=True)
    k.CreateFileW.restype = wintypes.HANDLE
    k.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                              wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    k.SetFileInformationByHandle.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p,
                                             wintypes.DWORD]
    h = k.CreateFileW(str(src), 0x00010000, 7, None, 3, 0x80, None)  # DELETE access
    assert h not in (None, wintypes.HANDLE(-1).value), ctypes.get_last_error()
    try:
        name = str(dst).encode("utf-16-le")
        # FILE_RENAME_INFO (x64): DWORD Flags, pad, HANDLE RootDirectory, DWORD
        # FileNameLength, WCHAR FileName[]; FileName at offset 20.
        buf = ctypes.create_string_buffer(20 + len(name) + 2)
        ctypes.memmove(ctypes.addressof(buf), (ctypes.c_uint32 * 1)(0x1 | 0x2), 4)
        ctypes.memmove(ctypes.addressof(buf) + 16, (ctypes.c_uint32 * 1)(len(name)), 4)
        ctypes.memmove(ctypes.addressof(buf) + 20, name, len(name))
        ok = k.SetFileInformationByHandle(h, 22, buf, len(buf))  # FileRenameInfoEx
        if not ok:
            err = ctypes.get_last_error()
            if err == 87:  # an SMB share: no POSIX rename semantics
                pytest.skip("this filesystem has no POSIX-semantics rename")
            raise OSError(err, "SetFileInformationByHandle")
    finally:
        k.CloseHandle(h)


@pytest.mark.parametrize("v", [7, 6])
def test_one_handle_for_writes_and_reads(tmp_path, v):
    p = new(tmp_path, "one", v)
    with dss.open(p) as f:
        assert f._native._held_opens == 0
        for i in range(20):
            f.write_paired_data(f"/A/P{i}/STAGE-FLOW///S/", [0.0, 1.0], [i, i + 1.0], **PD_KW)
            f.write_timeseries(f"/A/R{i}/FLOW//1HOUR/S/", [float(i), 2.0], **TS_KW)
            assert pd_values(f, f"/A/P{i}/STAGE-FLOW///S/") == [i, i + 1.0]
            # The file is unlocked between writes (lock detection sees nothing).
            assert not dss.lock_status(p)["locked"]
        assert f._native._held_opens == 1
        assert f.signature()["number_records"] == 40
    with dss.open(p) as g:
        assert len(g.catalog()) == 40


@pytest.mark.parametrize("v", [7, 6])
def test_held_and_path_writes_store_the_same_records(tmp_path, v):
    dss._native._set_write_clock(1_600_000_000_000)
    try:
        a, b = new(tmp_path, "path", v), new(tmp_path, "held", v)
        for i in range(5):
            dss.write_paired_data(a, f"/A/P{i}/STAGE-FLOW///S/", [0.0, 1.0], [i, 2.0],
                                  version=v, **PD_KW)
            dss.write_timeseries(a, f"/A/R{i}/FLOW//1HOUR/S/", [float(i)], version=v, **TS_KW)
        with dss.open(b) as f:
            for i in range(5):
                f.write_paired_data(f"/A/P{i}/STAGE-FLOW///S/", [0.0, 1.0], [i, 2.0], **PD_KW)
                f.write_timeseries(f"/A/R{i}/FLOW//1HOUR/S/", [float(i)], **TS_KW)
    finally:
        dss._native._set_write_clock(None)
    assert a.read_bytes() == b.read_bytes()


OTHER = textwrap.dedent(
    """
    import sys
    import ras_commander_fast_dss as dss
    path, v, name = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    dss.write_paired_data(path, name, [0.0, 1.0], [7.0, 8.0], version=v, x_units="FT",
                          x_type="LINEAR", y_units="CFS", y_type="LINEAR")
    """
)


@pytest.mark.parametrize("v", [7, 6])
def test_another_process_writes_between_session_writes(tmp_path, v):
    p = new(tmp_path, "other", v)
    with dss.open(p) as f:
        f.write_paired_data("/A/MINE1/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        for k in range(3):
            subprocess.run([sys.executable, "-I", "-c", OTHER, str(p), str(v),
                            f"/A/THEIRS{k}/STAGE-FLOW///S/"], check=True, timeout=120)
            # Our next write sees theirs (the session re-reads the header) and keeps it.
            f.write_paired_data(f"/A/MINE{k + 2}/STAGE-FLOW///S/", [0.0, 1.0], [k, 5.0],
                                **PD_KW)
            assert pd_values(f, f"/A/THEIRS{k}/STAGE-FLOW///S/") == [7.0, 8.0]
            assert pd_values(f, f"/A/MINE{k + 2}/STAGE-FLOW///S/") == [k, 5.0]
        assert f._native._held_opens == 1
        cat = f.catalog()
        assert not cat.attrs["anomalies"]
        assert len(cat) == 7
    with dss.open(p) as g:
        for k in range(3):
            assert pd_values(g, f"/A/THEIRS{k}/STAGE-FLOW///S/") == [7.0, 8.0]
        assert pd_values(g, "/A/MINE1/STAGE-FLOW///S/") == [1.0, 2.0]


@pytest.mark.parametrize("v", [7, 6])
def test_file_replaced_under_the_held_handle(tmp_path, v):
    p = new(tmp_path, "rep", v)
    other = new(tmp_path, "replacement", v)
    dss.write_paired_data(other, "/B/THEIRS/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0],
                          version=v, **PD_KW)
    keep = tmp_path / "original.dss"
    with dss.open(p) as f:
        f.write_paired_data("/A/BEFORE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        os.link(p, keep)  # a second name for the original file
        if os.name == "nt":
            # MoveFileExW (os.replace) refuses to replace a file that has an open handle,
            # even one opened with FILE_SHARE_DELETE: a held session blocks it.
            # A POSIX-semantics rename replaces it.
            with pytest.raises(PermissionError):
                os.replace(other, p)
            posix_replace(other, p)
        else:
            os.replace(other, p)
        f.write_paired_data("/A/AFTER/STAGE-FLOW///S/", [0.0, 1.0], [5.0, 6.0], **PD_KW)
        # The identity check saw the new file and reopened the path.
        assert f._native._held_opens == 2
        assert pd_values(f, "/B/THEIRS/STAGE-FLOW///S/") == [3.0, 4.0]
        assert pd_values(f, "/A/AFTER/STAGE-FLOW///S/") == [5.0, 6.0]
        with pytest.raises(dss.DssPathnameNotFoundError):
            f.read_paired_data("/A/BEFORE/STAGE-FLOW///S/")
    with dss.open(keep) as g:  # the original file did not receive the later write
        assert g.pathnames() == ["/A/BEFORE/STAGE-FLOW///S/"]


@pytest.mark.parametrize("v", [7, 6])
def test_file_renamed_away_under_the_held_handle(tmp_path, v):
    p = new(tmp_path, "ren", v)
    moved = tmp_path / "moved.dss"
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        os.rename(p, moved)
        # As a module-level write would: the path no longer exists.
        with pytest.raises(FileNotFoundError):
            f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0], **PD_KW)
        assert not f._native._holds_write_handle
        with pytest.raises(FileNotFoundError):
            f.catalog()
        assert not f.closed
        os.rename(moved, p)
        f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0], **PD_KW)
        assert f._native._held_opens == 2
        assert sorted(f.pathnames()) == ["/A/ONE/STAGE-FLOW///S/", "/A/TWO/STAGE-FLOW///S/"]


@pytest.mark.parametrize("v", [7, 6])
def test_file_deleted_under_the_held_handle(tmp_path, v):
    p = new(tmp_path, "del", v)
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        os.remove(p)  # the held handle allows deletion (FILE_SHARE_DELETE on Windows)
        # Local NTFS removes the name at once (POSIX delete). On an SMB share the delete
        # is pending until the last handle closes and the name stays; the
        # identity check fails either way.
        pending = p.exists()
        with pytest.raises(FileNotFoundError):
            f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0], **PD_KW)
        assert not f._native._holds_write_handle
        if pending:
            # Dropping the held handle (and the view) let the pending delete complete.
            assert not p.exists()
        # A new file at the path: the next write goes to it, not to the deleted one.
        dss.create(p, version=v)
        f.write_paired_data("/A/THREE/STAGE-FLOW///S/", [0.0, 1.0], [5.0, 6.0], **PD_KW)
        assert f.pathnames() == ["/A/THREE/STAGE-FLOW///S/"]
        assert f._native._held_opens == 2


@pytest.mark.skipif(os.name != "nt", reason="exclusive-open probe is Windows-only")
@pytest.mark.parametrize("how", ["close", "exit", "gc"])
def test_handle_released_on_close_exit_and_gc(tmp_path, how):
    p = new(tmp_path, f"rel_{how}", 7)
    f = dss.open(p)
    f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
    f.read_paired_data("/A/ONE/STAGE-FLOW///S/")
    assert not exclusive_open_ok(p)
    if how == "close":
        f.close()
    elif how == "exit":
        with f:
            f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
    else:
        del f
        gc.collect()
    assert exclusive_open_ok(p)


@pytest.mark.skipif(os.name != "nt", reason="exclusive-open probe is Windows-only")
def test_pure_read_session_holds_no_write_handle(tmp_path):
    p = new(tmp_path, "ro", 7)
    dss.write_paired_data(p, "/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
    os.chmod(p, 0o444)  # read-only file: reading through DssFile still works
    try:
        with dss.open(p) as f:
            assert pd_values(f, "/A/ONE/STAGE-FLOW///S/") == [1.0, 2.0]
            assert not f._native._holds_write_handle
            with pytest.raises(PermissionError):
                f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
            # A refused open leaves the session readable.
            assert pd_values(f, "/A/ONE/STAGE-FLOW///S/") == [1.0, 2.0]
    finally:
        os.chmod(p, 0o666)


def test_session_write_refused_while_an_hec_style_lock_is_held(tmp_path, holder_factory_p74):
    p = new(tmp_path, "lock", 7)
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        h = holder_factory_p74(p)
        byte = _primary_lock_byte(p)
        h.lock(byte, 8)
        with pytest.raises(dss.DssLockedError):
            f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0], **PD_KW)
        h.unlock(byte, 8)
        f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0], **PD_KW)
        assert f._native._held_opens == 1
        assert sorted(f.pathnames()) == ["/A/ONE/STAGE-FLOW///S/", "/A/TWO/STAGE-FLOW///S/"]


def _primary_lock_byte(p):
    """Byte offset of the primary write-lock word (``test_lock.ranges``)."""
    from test_lock import ranges  # noqa: PLC0415

    reason, byte, _ = ranges(p)[0]
    assert reason == "primary_write"
    return byte


@pytest.fixture
def holder_factory_p74():
    from test_lock import Holder  # noqa: PLC0415

    made = []

    def make(path):
        h = Holder(path)
        made.append(h)
        return h

    yield make
    for h in made:
        h.kill()


def test_handle_must_name_the_same_path(tmp_path):
    a, b = new(tmp_path, "a", 7), new(tmp_path, "b", 7)
    with dss.open(a) as f:
        with pytest.raises(ValueError, match="handle is open on"):
            dss._native.write_pd(str(b), "/A/X/STAGE-FLOW///S/", np.array([0.0, 1.0]),
                                 np.array([1.0, 2.0]), 1, "FT", "LINEAR", "CFS", "LINEAR",
                                 handle=f._native)
    assert dss.open(b).pathnames() == []


def test_copied_session_file_is_independent(tmp_path):
    """A copy of the file made while a session holds it is a different file: the session
    keeps writing to the original."""
    p = new(tmp_path, "copy", 7)
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        c = tmp_path / "copy.dss"
        shutil.copyfile(p, c)
        f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
    assert dss.open(c).pathnames() == ["/A/ONE/STAGE-FLOW///S/"]
    assert sorted(dss.open(p).pathnames()) == ["/A/ONE/STAGE-FLOW///S/", "/A/TWO/STAGE-FLOW///S/"]


def test_feature_flag():
    assert "dssfile_held_handle" in dss.FEATURES


@pytest.mark.parametrize("v", [7, 6])
def test_replacement_during_a_session_write_is_reported(tmp_path, v):
    """The path replaced after the identity check and before the write
    (forced with the native test hook). The write is stored in the previous file, the
    call raises OSError, the handle is dropped, and the next write goes to the new file."""
    p = new(tmp_path, "race", v)
    replacement = new(tmp_path, "racenew", v)
    dss.write_paired_data(replacement, "/B/THEIRS/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0],
                          version=v, **PD_KW)
    aside = tmp_path / "aside.dss"
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        dss._native._set_session_race(str(aside), str(replacement))
        with pytest.raises(OSError, match="replaced, renamed or deleted while the write ran") as e:
            f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [5.0, 6.0], **PD_KW)
        assert not isinstance(e.value, FileNotFoundError)
        assert not f._native._holds_write_handle
        # The path names the replacement; the raced write is not in it.
        assert f.pathnames() == ["/B/THEIRS/STAGE-FLOW///S/"]
        f.write_paired_data("/A/THREE/STAGE-FLOW///S/", [0.0, 1.0], [7.0, 8.0], **PD_KW)
        assert f._native._held_opens == 2
        assert sorted(f.pathnames()) == ["/A/THREE/STAGE-FLOW///S/", "/B/THEIRS/STAGE-FLOW///S/"]
    # The raced write was stored, in the previous file (not rolled back).
    with dss.open(aside) as g:
        assert sorted(g.pathnames()) == ["/A/ONE/STAGE-FLOW///S/", "/A/TWO/STAGE-FLOW///S/"]
        assert pd_values(g, "/A/TWO/STAGE-FLOW///S/") == [5.0, 6.0]


@pytest.mark.parametrize("v", [7, 6])
def test_rename_during_a_session_write_is_reported(tmp_path, v):
    p = new(tmp_path, "raceren", v)
    aside = tmp_path / "aside.dss"
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        dss._native._set_session_race(str(aside))
        with pytest.raises(FileNotFoundError, match="while the write ran"):
            f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [5.0, 6.0], **PD_KW)
        assert not f._native._holds_write_handle
        assert not p.exists()
    with dss.open(aside) as g:
        assert sorted(g.pathnames()) == ["/A/ONE/STAGE-FLOW///S/", "/A/TWO/STAGE-FLOW///S/"]


# -- the file replaced by the other DSS major version, and failed writes ------------------


def _replace(src, dst):
    """Replace ``dst`` with ``src`` although a session holds ``dst`` (POSIX semantics on
    Windows; skipped where the filesystem has none)."""
    if os.name == "nt":
        posix_replace(src, dst)
    else:
        os.replace(src, dst)


@pytest.mark.parametrize("written_first", [False, True])
@pytest.mark.parametrize("v_old,v_new", [(7, 6), (6, 7)])
def test_write_after_replacement_by_the_other_major_version(tmp_path, v_old, v_new,
                                                           written_first):
    """The writer's version comes from the file the path names now, not from
    the session's read view, before and after the session's first write."""
    p = new(tmp_path, "ver", v_old)
    other = new(tmp_path, "other", v_new)
    with dss.open(p) as f:
        assert f.version == v_old
        if written_first:
            f.write_paired_data("/A/OLD/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        _replace(other, p)
        f.write_paired_data("/A/NEW/STAGE-FLOW///S/", [0.0, 1.0], [3.0, 4.0], **PD_KW)
        assert f.version == v_new
        assert f.pathnames() == ["/A/NEW/STAGE-FLOW///S/"]
    assert dss.file_signature(p)["version"] == v_new


def test_write_location_after_replacement_by_dss6(tmp_path):
    """The profile/location writers choose DSS6 or DSS7 from the file at the path, not the
    read view's version."""
    p = new(tmp_path, "loc", 7)
    other = new(tmp_path, "loc6", 6)
    with dss.open(p) as f:
        _replace(other, p)
        with pytest.raises(dss.DssUnsupportedRecordError, match="DSS6 files cannot store profiles"):
            f.write_profile("/A/B/TEMP-WATER//1Hour/S/", [0.5], np.ones((2, 1)),
                            start="01JAN2021 01:00", depth_units="m", value_units="deg C",
                            data_type="INST-VAL")


def test_native_session_write_refuses_the_wrong_major_version(tmp_path):
    """A version that no longer matches the file on the held handle is refused before
    anything is written."""
    p = new(tmp_path, "wrong", 6)
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        before = p.read_bytes()
        with pytest.raises(ValueError, match="is a DSS6 file, not DSS7"):
            dss._native.write_pd(str(p), "/A/TWO/STAGE-FLOW///S/", np.array([0.0, 1.0]),
                                 np.array([1.0, 2.0]), 1, "FT", "LINEAR", "CFS", "LINEAR",
                                 version=7, handle=f._native)
        assert p.read_bytes() == before
        assert f.pathnames() == ["/A/ONE/STAGE-FLOW///S/"]


@pytest.mark.parametrize("v", [7, 6])
def test_write_version_does_not_rebuild_the_read_view(tmp_path, v):
    """Choosing the next write's version does not rebuild the view that the
    next write drops again; the next read rebuilds it."""
    p = new(tmp_path, "view", v)
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        assert f._native._view_stale
        assert f._write_version() == v
        assert f._native._view_stale  # not rebuilt
        f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        assert f._native._view_stale
        assert len(f.pathnames()) == 2
        assert not f._native._view_stale


def test_version_getter_after_a_write_and_after_deletion(tmp_path):
    """The getter refreshes a stale view (now without the GIL) and reports the last known
    version when the file is gone."""
    p = new(tmp_path, "gone", 6)
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        assert f._native._view_stale
        assert f.version == 6
        assert not f._native._view_stale
        f.write_paired_data("/A/TWO/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        os.remove(p)
        assert f.version == 6
        with pytest.raises(FileNotFoundError):
            f.pathnames()


@pytest.mark.parametrize("v", [7, 6])
def test_failed_write_after_a_rename_drops_the_handle(tmp_path, v):
    """A writer error is raised as it is, and the identity check still runs,
    so a handle whose file the path no longer names is dropped."""
    p = new(tmp_path, "failren", v)
    aside = tmp_path / "aside.dss"
    with dss.open(p) as f:
        f.write_paired_data("/A/ONE/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        dss._native._set_session_race(str(aside))
        with pytest.raises(ValueError) as e:  # an invalid pathname, refused by the writer
            dss._native.write_pd(str(p), "not a pathname", np.array([0.0, 1.0]),
                                 np.array([1.0, 2.0]), 1, "FT", "LINEAR", "CFS", "LINEAR",
                                 version=v, handle=f._native)
        assert "replaced, renamed or deleted" not in str(e.value)
        assert not f._native._holds_write_handle
    with dss.open(aside) as g:
        assert g.pathnames() == ["/A/ONE/STAGE-FLOW///S/"]


@pytest.mark.parametrize("written_first", [False, True])
def test_write_location_after_replacement_by_dss6_uses_the_dss6_writer(tmp_path, written_first):
    """``write_location``'s version argument: a DSS7
    session whose file is replaced by a DSS6 file writes the location with the DSS6 writer,
    into the DSS6 file's time-series dataset."""
    p = new(tmp_path, "loc7", 7)
    other = new(tmp_path, "loc6", 6)
    dss.write_timeseries(other, "/A/G/FLOW//1HOUR/S/", [1.0, 2.0], version=6, **TS_KW)
    with dss.open(p) as f:
        if written_first:
            f.write_paired_data("/A/OLD/STAGE-FLOW///S/", [0.0, 1.0], [1.0, 2.0], **PD_KW)
        _replace(other, p)
        f.write_location("/A/G/FLOW//1HOUR/S/", x=-97.25, y=30.5, z=612.5)
        assert f.version == 6
        loc = f.read_associated_location("/A/G/FLOW/01JAN2021/1HOUR/S/")
        assert (loc.x, loc.y, loc.z) == (-97.25, 30.5, 612.5)

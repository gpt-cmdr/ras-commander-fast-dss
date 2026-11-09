"""P62: catalog rows are selected natively before export.

``DssFile.catalog()`` asks the native side for live and alias rows only, and
``DssFile.pathnames()`` lists them without a DataFrame. These tests hold the result to the
pre-P62 construction (every physical row exported, framed, then filtered), reproduced here
as ``_reference_catalog`` from the unfiltered native export.
"""

from pathlib import Path

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as fd
from ras_commander_fast_dss._file import _LIVE_STATUSES, record_type_name

FIXTURES = Path(__file__).resolve().parents[2] / "fixtures" / "public"
PUBLIC = sorted(p.name for p in FIXTURES.glob("*.dss"))


def _reference_catalog(raw, all_entries, version):
    """The pre-P62 ``DssFile.catalog`` body, applied to a full native export."""
    status = raw["status"]
    last = np.asarray(raw["last_write_ms"], dtype=np.int64)
    last_write = np.where(last >= 0, last, np.iinfo(np.int64).min).astype("datetime64[ms]")
    rtype = np.asarray(raw["record_type"], dtype=np.int32)
    df = pd.DataFrame(
        {
            "pathname": raw["pathname"],
            "record_type": rtype,
            "record_type_name": [record_type_name(t) for t in rtype],
            "status": status,
            "duplicate": np.asarray(raw["duplicate"], dtype=bool),
            "recovered": np.asarray(raw["recovered"], dtype=bool),
            "last_write": last_write,
        }
    )
    if all_entries:
        df["raw_status"] = np.asarray(raw["raw_status"], dtype=np.int64)
        df["stored_pathname"] = raw["stored_pathname"]
    else:
        df = df[df["status"].isin(_LIVE_STATUSES).to_numpy()].reset_index(drop=True)
    df.attrs["anomalies"] = list(raw["anomalies"])
    df.attrs["dss_version"] = version
    return df


def _assert_same(got, want):
    pd.testing.assert_frame_equal(got, want, check_exact=True)
    assert list(got.columns) == list(want.columns)
    assert got.dtypes.to_dict() == want.dtypes.to_dict()
    assert type(got.index) is type(want.index) and got.index.equals(want.index)
    assert got.attrs == want.attrs


def _open_or_skip(name, recover=False):
    try:
        return fd.open(FIXTURES / name, recover=recover)
    except fd.DssError as e:  # a negative fixture that does not open
        pytest.skip(f"{name} does not open: {e}")


@pytest.mark.parametrize("recover", [False, True])
@pytest.mark.parametrize("name", PUBLIC)
def test_catalog_matches_pre_p62_construction(name, recover):
    with _open_or_skip(name) as f:
        try:
            raw = f._native.catalog(recover)
        except fd.DssError as e:
            with pytest.raises(type(e)):
                f.catalog(recover=recover)
            return
        for all_entries in (False, True):
            want = _reference_catalog(raw, all_entries, f.version)
            _assert_same(f.catalog(recover=recover, all_entries=all_entries), want)


@pytest.mark.parametrize("name", PUBLIC)
def test_pathnames_equal_catalog_column(name):
    with _open_or_skip(name) as f:
        try:
            want = f.catalog()["pathname"].tolist()
        except fd.DssError as e:
            with pytest.raises(type(e)):
                f.pathnames()
            return
        got = f.pathnames()
        assert type(got) is list and all(type(p) is str for p in got)
        assert got == want


def test_live_only_export_keeps_native_catalog_whole():
    # synthetic_catalog_ops7 holds live, alias, deleted and renamed entries.
    with fd.open(FIXTURES / "synthetic_catalog_ops7.dss") as f:
        full = f._native.catalog(None)
        live = f._native.catalog(None, live_only=True)
        assert full["entries"] == live["entries"] == len(full["pathname"]) == 16
        assert "stored_pathname" not in live and "raw_status" not in live
        keep = [i for i, s in enumerate(full["status"]) if s in _LIVE_STATUSES]
        assert len(keep) == 13 < len(full["status"])
        for k in ("pathname", "status", "duplicate", "recovered"):
            assert list(live[k]) == [full[k][i] for i in keep]
        for k in ("record_type", "last_write_ms"):
            assert np.array_equal(live[k], np.asarray(full[k])[keep])
        assert live["anomalies"] == full["anomalies"]
        # Forensic indices still address the full catalog after a live-only export.
        idx = full["status"].index("deleted")
        assert f.diagnose(idx)["outcome"] == "not_live"
        assert f.catalog(all_entries=True).iloc[idx]["status"] == "deleted"


def test_repeated_calls_are_independent():
    with fd.open(FIXTURES / "synthetic_catalog_ops7.dss") as f:
        first = f.pathnames()
        first.append("/X/Y/Z///W/")
        df = f.catalog()
        df.loc[0, "pathname"] = "/CHANGED/"
        df.attrs["anomalies"].append(("x", 0, "y"))
        again = f.catalog()
        assert f.pathnames() == again["pathname"].tolist()
        assert again.loc[0, "pathname"] != "/CHANGED/"
        assert ("x", 0, "y") not in again.attrs["anomalies"]


class _NoLiveRows:
    """Native stand-in whose catalog holds entries, none of them live."""

    def __init__(self, native):
        self._n = native
        full = native.catalog(None)
        dead = [i for i, s in enumerate(full["status"]) if s not in _LIVE_STATUSES]
        assert dead
        self.full = dict(full)
        for k in ("pathname", "stored_pathname", "status", "duplicate", "recovered"):
            self.full[k] = [full[k][i] for i in dead]
        for k in ("record_type", "raw_status", "last_write_ms"):
            self.full[k] = np.asarray(full[k])[dead]
        self.full["entries"] = len(dead)

    def catalog(self, recover=None, live_only=False):
        if not live_only:
            return dict(self.full)
        out = {k: v for k, v in self.full.items() if k not in ("stored_pathname", "raw_status")}
        out.update(pathname=[], status=[], duplicate=[], recovered=[],
                   record_type=np.zeros(0, np.int32), last_write_ms=np.zeros(0, np.int64))
        return out

    def __getattr__(self, name):
        return getattr(self._n, name)


def test_no_live_rows_keeps_column_dtypes():
    with fd.open(FIXTURES / "synthetic_catalog_ops7.dss") as f:
        stub = _NoLiveRows(f._native)
        real, f._native = f._native, stub
        try:
            got = f.catalog()
        finally:
            f._native = real
        want = _reference_catalog(stub.full, False, f.version)
        assert len(got) == 0
        _assert_same(got, want)


def test_empty_catalog_keeps_column_dtypes(tmp_path):
    p = tmp_path / "empty7.dss"
    fd.create(p, version=7)
    with fd.open(p) as f:
        raw = f._native.catalog(None)
        assert raw["entries"] == 0
        _assert_same(f.catalog(), _reference_catalog(raw, False, 7))
        _assert_same(f.catalog(all_entries=True), _reference_catalog(raw, True, 7))
        assert f.pathnames() == []

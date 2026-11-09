"""P65: ``repair(src, dest)`` copies every readable DSS7 record into a new file.

Every file is created in ``tmp_path``; the public fixtures are copied before use, never
written.
"""

from __future__ import annotations

import shutil

import numpy as np
import pytest

import ras_commander_fast_dss as dss

from conftest import fixture_path


def _records(path):
    """Pathname -> decoded values of every time-series and paired-data record."""
    out = {}
    with dss.open(path) as f:
        cat = f.catalog()
        for _, row in cat.iterrows():
            p = row["pathname"]
            rt = int(row["record_type"]) if "record_type" in cat.columns else None
            try:
                if rt is not None and 200 <= rt < 210:
                    pd_ = f.read_paired_data(p)
                    out[p] = (np.asarray(pd_.ordinates).tobytes(),
                              np.asarray(pd_.values).tobytes())
                elif rt is not None and 100 <= rt < 120:
                    df = f.read_timeseries(p)
                    out[p] = df.to_numpy().tobytes()
            except Exception:  # noqa: BLE001 - only records both files can read are compared
                pass
    return out


def test_repair_copies_every_record_and_leaves_src_unchanged(tmp_path):
    src = tmp_path / "src.dss"
    dss.create(src)
    dss.write_timeseries(src, "/A/B/FLOW//1Hour/S/", np.arange(30.0),
                         start="01JAN2021 0100", units="cfs", data_type="INST-VAL")
    dss.write_paired_data(src, "/A/B/STAGE-FLOW///S/", [0.0, 1.0, 2.0], [5.0, 6.0, 7.0],
                          x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")
    before = src.read_bytes()
    dest = tmp_path / "dest.dss"
    r = dss.repair(src, dest)
    assert src.read_bytes() == before
    assert r["complete"] is True
    assert r["fallback_copies"] == []
    # One ledger entry per source candidate; complete derives from it.
    assert [e[3] for e in r["ledger"]] == ["copied", "copied"]
    assert r["complete"] == all(e[3] != "not_copied" for e in r["ledger"])
    assert r["records_copied"] == 2  # one monthly hourly block and the paired data
    assert r["records_copied"] == r["src_listed_records"]
    assert r["not_copied"] == []
    assert _records(dest) == _records(src)


def test_repair_refuses_an_existing_dest(tmp_path):
    src = tmp_path / "src.dss"
    dss.create(src)
    dest = tmp_path / "dest.dss"
    dest.write_bytes(b"keep")
    with pytest.raises(FileExistsError):
        dss.repair(src, dest)
    assert dest.read_bytes() == b"keep"


def test_repair_of_public_fixtures(tmp_path):
    for name in ["sample7.dss", "synthetic_pd7.dss", "BaldEagleDamBrk.dss"]:
        src = tmp_path / name
        shutil.copyfile(fixture_path(name), src)
        before = src.read_bytes()
        dest = tmp_path / f"{name}.repaired.dss"
        r = dss.repair(src, dest)
        assert src.read_bytes() == before, name
        not_copied = {p for p, _ in r["not_copied"]}
        assert r["records_copied"] + len(not_copied) + r["aliases_skipped"] \
            >= r["src_listed_records"], name
        got, want = _records(dest), _records(src)
        for p, v in want.items():
            if p not in not_copied:
                assert got.get(p) == v, (name, p)


def test_repair_feature_flag():
    assert "repair_copy_out" in dss.FEATURES
    assert "repair" in dss.__all__


def test_repair_missing_source_names_the_source(tmp_path):
    src = tmp_path / "missing.dss"
    dest = tmp_path / "dest.dss"
    with pytest.raises(FileNotFoundError) as e:
        dss.repair(src, dest)
    assert "missing.dss" in str(e.value)
    assert not dest.exists()
    assert list(tmp_path.iterdir()) == []

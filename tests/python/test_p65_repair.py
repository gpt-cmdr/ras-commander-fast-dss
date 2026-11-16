"""P65: ``repair(src, dest)`` copies every readable DSS7 record into a new file.

Every file is created in ``tmp_path``; the public fixtures are copied before use, never
written.
"""

from __future__ import annotations

import os
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


# ---------------------------------------------------------------------------------------
# An unsafe publication is refused (DssPublishError), never downgraded.
# ---------------------------------------------------------------------------------------


@pytest.fixture
def publish_faults():
    """Fault injection: only in a wheel built with ``--features test-hooks``.
    CI's Python tests use such a test wheel and set ``FAST_DSS_TEST_HOOKS=1``, which
    turns a missing hook into a failure instead of a skip."""
    if not hasattr(dss._native, "_set_publish_faults"):
        if os.environ.get("FAST_DSS_TEST_HOOKS") == "1":
            pytest.fail("FAST_DSS_TEST_HOOKS=1 but this wheel has no fault hooks")
        pytest.skip("release wheel: no fault-injection hooks (build with --features test-hooks)")

    def install(*names):
        dss._native._set_publish_faults(list(names))

    try:
        yield install
    finally:
        dss._native._set_publish_faults([])


def test_publish_error_is_an_oserror_and_a_dss_error():
    assert issubclass(dss.DssPublishError, OSError)
    assert issubclass(dss.DssPublishError, dss.DssError)
    assert dss.DssPublishError is dss.exceptions.DssPublishError


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("link", ["link_unsupported", "link_permission"])
def test_create_refuses_when_publication_is_unsafe(tmp_path, publish_faults, version, link):
    (tmp_path / "bystander.dss").write_bytes(b"untouched")
    dest = tmp_path / "new.dss"
    publish_faults(link, "rename_unavailable")
    with pytest.raises(dss.DssPublishError) as ei:
        dss.create(dest, version=version)
    e = ei.value
    assert isinstance(e, OSError)
    assert e.dest == str(dest) and e.kept_path is None
    assert "not created" in str(e) and "copy it" in str(e)
    assert not dest.exists()
    assert sorted(p.name for p in tmp_path.iterdir()) == ["bystander.dss"]  # no temp file
    assert (tmp_path / "bystander.dss").read_bytes() == b"untouched"


def test_repair_refuses_and_keeps_its_verified_copy(tmp_path, publish_faults):
    src = tmp_path / "src.dss"
    dss.create(src)
    dss.write_timeseries(src, "/A/B/FLOW/01JAN2020/1HOUR/S/", [1.0, 2.0, 3.0],
                         start="01JAN2020 01:00", units="CFS", data_type="INST-VAL")
    before = src.read_bytes()
    dest = tmp_path / "out.dss"
    publish_faults("link_permission", "rename_permission")
    with pytest.raises(dss.DssPublishError) as ei:
        dss.repair(src, dest)
    e = ei.value
    assert not dest.exists()
    assert src.read_bytes() == before
    kept = e.kept_path
    assert kept and os.path.exists(kept) and kept in str(e)
    publish_faults()  # no faults: open the kept copy normally
    with dss.open(kept) as f:
        assert [p.upper() for p in f.pathnames()] == ["/A/B/FLOW/01JAN2020/1HOUR/S/"]


def test_transient_link_failures_are_retried(tmp_path, publish_faults):
    dest = tmp_path / "new.dss"
    publish_faults("link_transient")
    dss.create(dest)
    with dss.open(dest) as f:
        assert f.pathnames() == []

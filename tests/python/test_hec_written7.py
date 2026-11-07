"""P32: DSS7 variants with no corpus data, in files written by HEC's C library.

Fixtures (``fixtures/public``): ``synthetic_notes_quality7.dss``,
``synthetic_irregular_variants7.dss`` and ``synthetic_catalog_ops7.dss``, written by
``evidence/P32/scripts/P32Write7.c`` (heclib built from hec-dss
``f14554d``, file version ``7-JA``). They are not regenerated: the SHA-256 is the
fixture's identity. HEC's C library agrees with the engine on 106/106 requests
(REPORT §4). The Python dicts carry values, times and the first quality word; integer
and character notes and extra quality words are checked in
``crates/dss-core/tests/hec_written7.rs``.
"""

import hashlib

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from conftest import fixture_path

SHA = {
    "synthetic_notes_quality7.dss": "6ade1f57f4390f08e590cf8a519a06fe732b3f5f1597ec790706ed03c595b0ba",
    "synthetic_irregular_variants7.dss": "d9747f5b9c07fcbfec1b958d8b8ed9f0c307bba21e1a97753a1bd6f906c17a3d",
    "synthetic_catalog_ops7.dss": "839369ba9ffd651a79bfb29efcc705d85d4676d518f8aea8dd2e86d9afb85f3d",
}
NOTES, IRREG, CATALOG = SHA
DAY = 86400


def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()


def at(y, m, d, secs=0):
    return dss.to_hec_seconds(f"{y:04d}-{m:02d}-{d:02d}") + secs


def fv(base, n):
    return np.float32(base) + np.arange(n, dtype=np.float32) * np.float32(0.25)


def dv(base, n):
    return base + np.arange(n, dtype=np.float64) * 0.125


@pytest.fixture
def opened():
    files = {}

    def get(name):
        p = fixture_path(name)
        assert sha(p) == SHA[name]
        files[name] = (p, dss.open(p))
        return files[name][1]

    yield get
    for name, (p, fh) in files.items():
        fh.close()
        assert sha(p) == SHA[name], f"{name} changed on read"


@pytest.mark.parametrize("name", list(SHA))
def test_fixture_sha256(name):
    assert sha(fixture_path(name)) == SHA[name]


def test_quality_first_word_with_notes_and_multiword_quality(opened):
    f = opened(NOTES)
    r = f.read_record_arrays("/P32/NOTES/FLOW/01JAN2001/1Hour/CNOTES REG/")
    assert len(r["values"]) == 744
    assert r["times"][0] == at(2001, 1, 1, 3600)
    np.testing.assert_array_equal(r["values"][:24], fv(10, 24))
    q = np.where(np.arange(24) % 3 == 0, 3, 5)
    np.testing.assert_array_equal(r["quality"][:24], q)
    # Two quality words per value: the dict keeps the first word of each value.
    r = f.read_record_arrays("/P32/QUALITY/FLOW/01JAN2001/1Hour/Q2 REG/")
    np.testing.assert_array_equal(r["quality"][:24], np.arange(24) * 16 + 1)
    assert (r["quality"][24:] == 0).all()
    # Three words per value, doubles across two year blocks.
    d = f.read_arrays("/P32/QUALITY/STAGE//1Day/Q3 DBL/", trim_missing=False)
    assert d["values"].dtype == np.float64
    np.testing.assert_array_equal(d["values"], dv(80, 40))
    np.testing.assert_array_equal(d["quality"], np.arange(40) * 3)
    assert d["blocks_found"] == 2


def test_integer_notes_records_read_values(opened):
    f = opened(NOTES)
    d = f.read_arrays("/P32/NOTES/STAGE//1Day/INOTES QUAL/", trim_missing=False)
    assert len(d["values"]) == 40
    assert d["times"][0] == at(2001, 12, 11)
    np.testing.assert_array_equal(d["quality"], np.arange(1, 41))
    w = f.read_arrays("/P32/NOTES/FLOW//IR-Month/INOTES IRR/")
    np.testing.assert_array_equal(w["values"], dv(60, 10))
    np.testing.assert_array_equal(w["times"], at(2001, 3, 3) + np.arange(10) * 300 * 60)


def test_trim_keeps_quality_aligned(opened):
    f = opened(NOTES)
    d = f.read_arrays("/P32/QUALITY/FLOW//1Hour/Q2 MISSING/")
    # -901 first and -FLT_MAX last are trimmed; the -902 inside is kept.
    assert len(d["values"]) == 22
    assert d["values"][4] == -902.0
    np.testing.assert_array_equal(d["quality"], 9 * np.arange(2, 46, 2) + 2)


def test_pseudo_regular_and_century(opened):
    f = opened(IRREG)
    d = f.read_arrays("/P32/PSEUDO/FLOW//~1Day/PR DAY/")
    assert not d["regular"]
    assert d["irregular_block"] == "IR-Year"
    gaps = np.where(np.arange(10) >= 6, 3, 0)
    np.testing.assert_array_equal(d["times"], at(2001, 12, 27) + DAY * (np.arange(10) + gaps))
    r = f.read_record_arrays("/P32/PSEUDO/FLOW/01JAN2000/~1Month/PR MONTH/")
    assert r["irregular_block"] == "IR-Century"
    d = f.read_arrays("/P32/CENTURY/FLOW//IR-Century/MINUTES/")
    years = [1950, 1960, 1970, 1980, 1990, 1999, 2005, 2020]
    want = [at(y, 3, 15, (390 + i) * 60) for i, y in enumerate(years)]
    np.testing.assert_array_equal(d["times"], want)
    w = f.read_arrays("/P32/CENTURY/FLOW//IR-Century/MINUTES/", "1985-01-01", "2011-01-01")
    np.testing.assert_array_equal(w["times"], want[4:7])


def test_second_granularity_far_dates(opened):
    f = opened(IRREG)
    d = f.read_arrays("/P32/SECONDS/FLOW//IR-Year/FUTURE 2350/")
    i = np.arange(6)
    np.testing.assert_array_equal(d["times"], at(2350, 1, 1) + DAY * 100 * i + 3600 * i + 17 * i + 1)
    d = f.read_arrays("/P32/SECONDS/FLOW//IR-Year/PAST 1650/")
    assert d["times"][0] == -7_889_011_193
    np.testing.assert_array_equal(d["values"], dv(6, 6))
    d = f.read_arrays("/P32/SECONDS/FLOW//IR-Day/FUTURE 7500/")
    assert d["times"][0] == 176_734_915_201
    np.testing.assert_array_equal(d["times"] - d["times"][0], 13 * np.arange(5) ** 2)


def test_irregular_pattern_times_in_seconds(opened):
    f = opened(IRREG)
    r = f.read_record_arrays("/P32/PATTERN/FLOW/TS-PATTERN/IR-Year/IRR PATTERN/")
    assert r["pattern"] and r["record_type"] == 111
    np.testing.assert_array_equal(r["times"], (1440 * 60 * np.arange(5) + 120) * 60)
    r = f.read_record_arrays("/P32/PATTERN/FLOW/TS-PATTERN/IR-Day/IRR PATTERN DBL/")
    np.testing.assert_array_equal(r["times"], [1800, 7200, 12600, 18000])


def test_catalog_aliases_deleted_renamed(opened):
    f = opened(CATALOG)
    live = f.catalog()
    assert len(live) == 13
    by = dict(zip(live["pathname"], live["status"]))
    assert by["/P32/ALIAS ONE/FLOW/01JAN2001/1Day/OBS/"] == "alias"
    assert by["/P32/ALIAS TWO/STAGE/01JAN2001/1Day/ALT/"] == "alias"
    assert "/P32/EDIT/FLOW/01Jan2001/1Hour/DELETED/" not in by
    assert "/P32/EDIT/FLOW/01Jan2001/1Hour/OLD NAME/" not in by
    full = f.catalog(all_entries=True)
    raw = dict(zip(full["pathname"], full["raw_status"]))
    assert raw["/P32/EDIT/FLOW/01Jan2001/1Hour/DELETED/"] == 11
    assert raw["/P32/EDIT/FLOW/01Jan2001/1Hour/OLD NAME/"] == 12
    # zaliasRemove stores one character fewer: no final slash.
    assert raw["/P32/ALIAS GONE/FLOW/01JAN2001/1Day/OBS"] == 13


def test_alias_reads_match_primary(opened):
    f = opened(CATALOG)
    p = f.read_arrays("/P32/ALIAS PRIMARY/FLOW//1Day/OBS/")
    a = f.read_arrays("/P32/ALIAS ONE/FLOW//1Day/OBS/")
    np.testing.assert_array_equal(a["values"], p["values"])
    np.testing.assert_array_equal(a["times"], p["times"])
    np.testing.assert_array_equal(a["values"], fv(1, 40))
    assert len(f.read_arrays("/P32/ALIAS TWO/STAGE//1Day/ALT/")["values"]) == 22
    with pytest.raises(dss.DssPathnameNotFoundError):
        f.read_record_arrays("/P32/ALIAS GONE/FLOW/01JAN2001/1Day/OBS/")


def test_deleted_rewritten_renamed_and_collections(opened):
    f = opened(CATALOG)
    d = f.read_arrays("/P32/EDIT/FLOW//1Hour/READDED/")
    np.testing.assert_array_equal(d["values"], fv(600, 6))
    assert d["times"][0] == at(2001, 1, 1, 3 * 3600)
    with pytest.raises(dss.DssPathnameNotFoundError):
        f.read_record_arrays("/P32/EDIT/FLOW/01JAN2001/1Hour/DELETED/")
    with pytest.raises(dss.DssPathnameNotFoundError):
        f.read_record_arrays("/P32/EDIT/FLOW/01JAN2001/1Hour/OLD NAME/")
    np.testing.assert_array_equal(f.read_arrays("/P32/EDIT/FLOW//1Hour/NEW NAME/")["values"], fv(900, 12))
    np.testing.assert_array_equal(f.read_arrays("/P32/EDIT/FLOW//1Hour/UNDELETED/")["values"], fv(800, 12))
    for seq, base in (("000001", 100), ("000002", 200), ("000003", 300), ("ABCDEF", 400)):
        d = f.read_arrays(f"/P32/COLLECTION/FLOW//1Hour/C:{seq}|RUN SET/")
        np.testing.assert_array_equal(d["values"], fv(base, 12))

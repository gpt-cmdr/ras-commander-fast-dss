"""Maintainer decision 2026-11-14: historical NOHRSC SNODAS header conventions.

Melt and sublimation dated before 2019-02-05 state 06:00 UTC of the previous day to a 05:00 stop;
precipitation dated before 2008-10-23 states a 07:00 start. By default both are imported with the
header's own D and E parts, PER-CUM; ``snodas_times="vortex"`` is unchanged.

The headers are the real NOHRSC members of NSIDC's public G02158 tars, byte for byte
(``fixtures/public/snodas_headers``; provenance in the fixtures README). The grid bytes are the
committed 2020 sublimation member's, named as each dated member: only the times are under test.
"""

from __future__ import annotations

import hashlib
import re
import shutil

import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import gridded

from conftest import FIXTURES, fixture_path

GRID = "p71/us_ssmv11039lL00T0024TTNATS2020010105DP000.dat.gz"
KW = dict(cell_size=2000.0, align="vortex", envelope=(-200_000.0, 1_700_000.0, 0.0, 1_900_000.0))

# Member stem, variable, default D and E parts, Vortex D and E parts.
CASES = [
    # Melt and sublimation: the last historical day and the first modern one.
    ("us_ssmv11039lL00T0024TTNATS2019020405DP000", "Sublimation of Blowing Snow",
     ("03FEB2019:0600", "04FEB2019:0500"), ("04FEB2019:0600", "05FEB2019:0600")),
    ("us_ssmv11039lL00T0024TTNATS2019020505DP000", "Sublimation of Blowing Snow",
     ("04FEB2019:0600", "05FEB2019:0600"), ("05FEB2019:0600", "06FEB2019:0600")),
    ("us_ssmv11044bS__T0024TTNATS2019020405DP000", "Snow Melt Runoff at the Base of the Snow Pack",
     ("03FEB2019:0600", "04FEB2019:0500"), ("04FEB2019:0600", "05FEB2019:0600")),
    ("us_ssmv11050lL00T0024TTNATS2003100105DP000", "Sublimation from the Snow Pack",
     ("30SEP2003:0600", "01OCT2003:0500"), ("01OCT2003:0600", "02OCT2003:0600")),
    # Precipitation: the last historical day and the first modern one.
    ("us_ssmv01025SlL00T0024TTNATS2008102205DP001", "Liquid Precipitation",
     ("21OCT2008:0700", "22OCT2008:0600"), ("22OCT2008:0600", "23OCT2008:0600")),
    ("us_ssmv01025SlL01T0024TTNATS2008102305DP001", "Solid Precipitation",
     ("22OCT2008:0600", "23OCT2008:0600"), ("23OCT2008:0600", "24OCT2008:0600")),
]


def _source(tmp_path, stem):
    shutil.copyfile(fixture_path(GRID), tmp_path / f"{stem}.dat.gz")
    shutil.copyfile(fixture_path(f"snodas_headers/{stem}.txt.gz"), tmp_path / f"{stem}.txt.gz")
    return tmp_path / f"{stem}.dat.gz"


def test_fixtures_match_their_provenance():
    """Every committed header is the member the fixtures README names, by SHA-256."""
    readme = (FIXTURES / "README.md").read_text(encoding="utf-8")
    rows = re.findall(r"\| `(snodas_headers/[^`]+)` \|.*\| `([0-9a-f]{64})` \|", readme)
    files = sorted(p.name for p in (FIXTURES / "snodas_headers").iterdir())
    assert sorted(name.split("/", 1)[1] for name, _ in rows) == files
    assert len(files) == 25
    for name, sha in rows:
        assert hashlib.sha256((FIXTURES / name).read_bytes()).hexdigest() == sha, name


@pytest.mark.parametrize("stem, variable, header, vortex", CASES, ids=[c[0][-15:-7] + c[0][8:12] for c in CASES])
def test_historical_and_modern_headers_import(tmp_path, stem, variable, header, vortex):
    src = _source(tmp_path, stem)
    assert gridded.list_variables(src) == [variable]
    [d] = gridded.import_gridded([src], tmp_path / "d.dss", variable=variable, **KW).pathnames
    [v] = gridded.import_gridded([src], tmp_path / "v.dss", variable=variable,
                                 snodas_times="vortex", **KW).pathnames
    assert tuple(d.split("/")[4:6]) == header
    assert tuple(v.split("/")[4:6]) == vortex
    with dss.open(tmp_path / "d.dss") as f:
        assert f.read_grid(d).data_type_name == "PER-CUM"
    with dss.open(tmp_path / "v.dss") as f:
        assert f.read_grid(v).data_type_name == "PER-AVER"
